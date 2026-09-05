//! Serializes durable background state and handset delivery on one blocking owner.

use std::sync::Arc;
use std::time::Instant;

use sccp_protocol::{
    CiscoIpPhoneSetBackground, Command as PhoneCommand, CommandAction as PhoneCommandAction,
    DeviceId, DeviceType, PhoneBackgroundHttpUrl, ServerError, TransactionId,
};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

use super::{Access, ast_log};
use crate::asterisk::MANAGER_CONTROL_TIMEOUT;
use crate::asterisk::adapters::AsteriskDatabase;
use crate::asterisk::boundary::LogLevel;
use crate::config::{
    BackgroundThumbnailSource, DeviceBackground, DeviceBackgroundError, DeviceBackgroundSelection,
    ModuleConfig, ResolvedDeviceBackground,
};
use crate::runtime::controller::controller_step;
use crate::state::background::{BackgroundStore, BackgroundStoreError};

const BACKGROUND_REQUEST_CAPACITY: usize = 64;

#[derive(Debug, Error)]
pub(super) enum BackgroundRuntimeError {
    #[error(transparent)]
    Store(#[from] BackgroundStoreError),
    #[error(transparent)]
    Resolution(#[from] DeviceBackgroundError),
    #[error(transparent)]
    Delivery(#[from] ServerError),
    #[error("background request queue is full")]
    QueueFull,
    #[error("background runtime has stopped")]
    Stopped,
    #[error("background request timed out")]
    TimedOut,
}

enum BackgroundCliOperation {
    Show,
    Set {
        image_url: String,
        thumbnail_url: Option<String>,
    },
    Reset,
}

enum BackgroundRequest {
    Apply {
        device: DeviceId,
    },
    Cli {
        device: DeviceId,
        operation: BackgroundCliOperation,
        deadline: Instant,
        response: std::sync::mpsc::SyncSender<String>,
    },
    Reconcile {
        previous: Arc<ModuleConfig>,
        registered: Vec<DeviceId>,
        response: oneshot::Sender<Vec<(DeviceId, BackgroundRuntimeError)>>,
    },
    Shutdown {
        response: oneshot::Sender<()>,
    },
}

#[derive(Clone)]
pub(super) struct BackgroundRuntimeHandle {
    requests: mpsc::Sender<BackgroundRequest>,
}

pub(super) struct BackgroundRuntimeMailbox {
    requests: mpsc::Receiver<BackgroundRequest>,
}

pub(super) fn background_runtime_channel() -> (BackgroundRuntimeHandle, BackgroundRuntimeMailbox) {
    let (requests, receiver) = mpsc::channel(BACKGROUND_REQUEST_CAPACITY);
    (
        BackgroundRuntimeHandle { requests },
        BackgroundRuntimeMailbox { requests: receiver },
    )
}

impl BackgroundRuntimeHandle {
    async fn apply(&self, device: DeviceId) -> Result<(), BackgroundRuntimeError> {
        match tokio::time::timeout(
            MANAGER_CONTROL_TIMEOUT,
            self.requests.send(BackgroundRequest::Apply { device }),
        )
        .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(BackgroundRuntimeError::Stopped),
            Err(_) => Err(BackgroundRuntimeError::TimedOut),
        }
    }

    fn execute_cli(
        &self,
        device: DeviceId,
        operation: BackgroundCliOperation,
    ) -> Result<String, BackgroundRuntimeError> {
        let deadline = Instant::now()
            .checked_add(MANAGER_CONTROL_TIMEOUT)
            .ok_or(BackgroundRuntimeError::TimedOut)?;
        let (response, result) = std::sync::mpsc::sync_channel(1);
        match self.requests.try_send(BackgroundRequest::Cli {
            device,
            operation,
            deadline,
            response,
        }) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                return Err(BackgroundRuntimeError::QueueFull);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                return Err(BackgroundRuntimeError::Stopped);
            }
        }
        match result.recv_timeout(MANAGER_CONTROL_TIMEOUT) {
            Ok(result) => Ok(result),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                Err(BackgroundRuntimeError::TimedOut)
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                Err(BackgroundRuntimeError::Stopped)
            }
        }
    }

    async fn reconcile(
        &self,
        previous: Arc<ModuleConfig>,
        registered: Vec<DeviceId>,
    ) -> Result<Vec<(DeviceId, BackgroundRuntimeError)>, BackgroundRuntimeError> {
        let (response, result) = oneshot::channel();
        let request = async {
            self.requests
                .send(BackgroundRequest::Reconcile {
                    previous,
                    registered,
                    response,
                })
                .await
                .map_err(|_| BackgroundRuntimeError::Stopped)?;
            result.await.map_err(|_| BackgroundRuntimeError::Stopped)
        };
        match tokio::time::timeout(MANAGER_CONTROL_TIMEOUT, request).await {
            Ok(result) => result,
            Err(_) => Err(BackgroundRuntimeError::TimedOut),
        }
    }

    pub(super) async fn shutdown(&self) -> Result<(), BackgroundRuntimeError> {
        let (response, result) = oneshot::channel();
        let request = async {
            self.requests
                .send(BackgroundRequest::Shutdown { response })
                .await
                .map_err(|_| BackgroundRuntimeError::Stopped)?;
            result.await.map_err(|_| BackgroundRuntimeError::Stopped)
        };
        match tokio::time::timeout(MANAGER_CONTROL_TIMEOUT, request).await {
            Ok(result) => result,
            Err(_) => Err(BackgroundRuntimeError::TimedOut),
        }
    }
}

pub(super) struct BackgroundRuntime {
    access: Access,
    store: BackgroundStore<AsteriskDatabase>,
    mailbox: BackgroundRuntimeMailbox,
    next_transaction_id: u32,
}

impl BackgroundRuntime {
    pub(super) fn new(
        access: Access,
        store: BackgroundStore<AsteriskDatabase>,
        mailbox: BackgroundRuntimeMailbox,
    ) -> Self {
        Self {
            access,
            store,
            mailbox,
            next_transaction_id: 1,
        }
    }

    pub(super) fn run(mut self) {
        while let Some(request) = self.mailbox.requests.blocking_recv() {
            match request {
                BackgroundRequest::Apply { device } => {
                    if let Err(error) = self.apply_registered(&device) {
                        ast_log(
                            LogLevel::Warning,
                            &format!(
                                "unable to apply the registered background for device {device}: {error}"
                            ),
                        );
                    }
                }
                BackgroundRequest::Cli {
                    device,
                    operation,
                    deadline,
                    response,
                } => {
                    if Instant::now() < deadline {
                        let result = self.execute_cli_operation(device, operation);
                        drop(response.send(result));
                    }
                }
                BackgroundRequest::Reconcile {
                    previous,
                    registered,
                    response,
                } => {
                    let failures = self.reconcile_backgrounds(&previous, &registered);
                    drop(response.send(failures));
                }
                BackgroundRequest::Shutdown { response } => {
                    self.mailbox.requests.close();
                    let _response_was_dropped = response.send(());
                    return;
                }
            }
        }
    }

    fn apply_registered(&mut self, device: &DeviceId) -> Result<(), BackgroundRuntimeError> {
        match self.effective_background(device)?.into_background() {
            Some(background) => self.deliver(device.clone(), background).map_err(Into::into),
            None => Ok(()),
        }
    }

    fn reconcile_backgrounds(
        &mut self,
        previous: &ModuleConfig,
        registered: &[DeviceId],
    ) -> Vec<(DeviceId, BackgroundRuntimeError)> {
        let current = self.access.config();
        let mut failures = Vec::new();
        for device in registered {
            match self.store.load_override(device) {
                Ok(Some(_)) => continue,
                Ok(None) => {}
                Err(error) => {
                    failures.push((device.clone(), error.into()));
                    continue;
                }
            }
            let device_type = registered_device_type(&self.access, device);
            let previous_background = match resolve_configured_background(
                previous
                    .devices
                    .get(device)
                    .and_then(|config| config.background.as_ref()),
                device_type,
            ) {
                Ok(background) => background,
                Err(error) => {
                    failures.push((device.clone(), error.into()));
                    continue;
                }
            };
            let current_background = match resolve_configured_background(
                current
                    .devices
                    .get(device)
                    .and_then(|config| config.background.as_ref()),
                device_type,
            ) {
                Ok(background) => background,
                Err(error) => {
                    failures.push((device.clone(), error.into()));
                    continue;
                }
            };
            if same_resources(previous_background.as_ref(), current_background.as_ref()) {
                continue;
            }
            match current_background {
                Some(background) => {
                    if let Err(error) = self.deliver(device.clone(), background) {
                        failures.push((device.clone(), error.into()));
                    }
                }
                None => continue,
            }
        }
        failures
    }

    fn execute_cli_operation(
        &mut self,
        device: DeviceId,
        operation: BackgroundCliOperation,
    ) -> String {
        if !self.access.config().devices.contains_key(&device) {
            return format!("Background command failed: device {device} is not configured\n");
        }
        match operation {
            BackgroundCliOperation::Show => self.show_background(&device),
            BackgroundCliOperation::Reset => self.reset_background(device),
            BackgroundCliOperation::Set {
                image_url,
                thumbnail_url,
            } => self.set_background(device, &image_url, thumbnail_url.as_deref()),
        }
    }

    fn show_background(&self, device: &DeviceId) -> String {
        let effective = match self.effective_background(device) {
            Ok(effective) => effective,
            Err(error) => return format!("Background command failed: {error}\n"),
        };
        let (source, mode, thumbnail) = effective.description();
        let registered = controller_step(&self.access.shared.controller, |controller| {
            controller.is_registered(device)
        });
        let registered = match registered {
            true => "yes",
            false => "no",
        };
        format!(
            "{device}: background source={source}, mode={mode}, thumbnail={thumbnail}, registered={registered}\n"
        )
    }

    fn set_background(
        &mut self,
        device: DeviceId,
        image_url: &str,
        thumbnail_url: Option<&str>,
    ) -> String {
        let image_url = match PhoneBackgroundHttpUrl::new(image_url) {
            Ok(url) => url,
            Err(error) => return format!("Background command failed: {error}\n"),
        };
        let thumbnail_url = match thumbnail_url.map(PhoneBackgroundHttpUrl::new).transpose() {
            Ok(url) => url,
            Err(error) => return format!("Background command failed: {error}\n"),
        };
        let background = match DeviceBackground::new(image_url, thumbnail_url) {
            Ok(background) => background,
            Err(error) => return format!("Background command failed: {error}\n"),
        };
        if let Err(error) = self.store.put_override(&device, &background) {
            return format!("Background command failed: {error}\n");
        }
        let device_type = registered_device_type(&self.access, &device);
        match device_type {
            None => format!("{device}: background override saved for the next registration\n"),
            Some(device_type) => match background.resolve_for(Some(device_type)) {
                Some(background) => match self.deliver(device.clone(), background) {
                    Ok(()) => format!("{device}: background override saved and queued\n"),
                    Err(_) => format!(
                        "{device}: background override saved, but delivery failed; it will be retried on registration\n"
                    ),
                },
                None => format!(
                    "{device}: background override saved, but the registered model has no compatible background command\n"
                ),
            },
        }
    }

    fn reset_background(&mut self, device: DeviceId) -> String {
        if let Err(error) = self.store.reset(&device) {
            return format!("Background command failed: {error}\n");
        }
        let configured = self
            .access
            .config()
            .devices
            .get(&device)
            .and_then(|config| config.background.clone());
        let device_type = registered_device_type(&self.access, &device);
        let resolved = match (configured.as_ref(), device_type) {
            (None, _) => {
                return format!(
                    "{device}: background override removed; no configured background is available to apply\n"
                );
            }
            (Some(_), None) => {
                return format!(
                    "{device}: background reset to configuration for the next registration\n"
                );
            }
            (Some(selection), Some(device_type)) => selection.resolve(Some(device_type)),
        };
        match resolved {
            Ok(Some(background)) => match self.deliver(device.clone(), background) {
                Ok(()) => format!("{device}: background reset to configuration and queued\n"),
                Err(_) => format!(
                    "{device}: background override removed, but configured background delivery failed\n"
                ),
            },
            Ok(None) => format!(
                "{device}: background override removed; the registered model has no compatible configured background\n"
            ),
            Err(error) => format!(
                "{device}: background override removed, but configured background resolution failed: {error}\n"
            ),
        }
    }

    fn effective_background(
        &self,
        device: &DeviceId,
    ) -> Result<EffectiveBackground, BackgroundRuntimeError> {
        let selection = self
            .access
            .config()
            .devices
            .get(device)
            .and_then(|config| config.background.clone());
        resolve_effective_background(
            self.store.load_override(device)?,
            selection.as_ref(),
            registered_device_type(&self.access, device),
        )
        .map_err(Into::into)
    }

    fn deliver(
        &mut self,
        device: DeviceId,
        background: ResolvedDeviceBackground,
    ) -> Result<(), ServerError> {
        let transaction_id = self.next_transaction_id();
        let action = match background {
            ResolvedDeviceBackground::Set(background) => PhoneCommandAction::SetBackgroundImage {
                transaction_id,
                document: CiscoIpPhoneSetBackground::new(
                    background.image_url().clone(),
                    background.thumbnail_url().clone(),
                ),
            },
            ResolvedDeviceBackground::Display(image_url) => {
                PhoneCommandAction::DisplayBackgroundImage {
                    transaction_id,
                    image_url,
                }
            }
        };
        self.access
            .phone
            .try_send(PhoneCommand::new(device, action))
    }

    fn next_transaction_id(&mut self) -> TransactionId {
        let current = self.next_transaction_id;
        self.next_transaction_id = self.next_transaction_id.wrapping_add(1).max(1);
        TransactionId::new(current)
    }
}

enum EffectiveBackground {
    Unconfigured,
    ConfiguredStatic(Option<ResolvedDeviceBackground>),
    ConfiguredDynamic(Option<ResolvedDeviceBackground>),
    Override(Option<ResolvedDeviceBackground>),
}

impl EffectiveBackground {
    fn into_background(self) -> Option<ResolvedDeviceBackground> {
        match self {
            Self::Unconfigured => None,
            Self::ConfiguredStatic(background)
            | Self::ConfiguredDynamic(background)
            | Self::Override(background) => background,
        }
    }

    fn description(&self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Unconfigured => ("none", "none", "none"),
            Self::ConfiguredStatic(background) => (
                "configuration",
                "static",
                thumbnail_description(background.as_ref()),
            ),
            Self::ConfiguredDynamic(_) => ("configuration", "dynamic", "dynamic"),
            Self::Override(background) => (
                "override",
                "static",
                thumbnail_description(background.as_ref()),
            ),
        }
    }
}

pub(super) async fn enqueue_registered_background(
    access: &Access,
    device: &DeviceId,
) -> Result<(), BackgroundRuntimeError> {
    access.shared.background_runtime.apply(device.clone()).await
}

pub(super) async fn reconcile_backgrounds_after_reload(
    access: &Access,
    previous: Arc<ModuleConfig>,
    registered: Vec<DeviceId>,
) -> Result<Vec<(DeviceId, BackgroundRuntimeError)>, BackgroundRuntimeError> {
    access
        .shared
        .background_runtime
        .reconcile(previous, registered)
        .await
}

pub(super) fn execute_background_cli(access: &Access, arguments: &[String]) -> String {
    let (device_text, operation) = match arguments {
        [device, operation] if operation.eq_ignore_ascii_case("show") => {
            (device, BackgroundCliOperation::Show)
        }
        [device, operation] if operation.eq_ignore_ascii_case("reset") => {
            (device, BackgroundCliOperation::Reset)
        }
        [device, operation, image_url] if operation.eq_ignore_ascii_case("set") => (
            device,
            BackgroundCliOperation::Set {
                image_url: image_url.clone(),
                thumbnail_url: None,
            },
        ),
        [device, operation, image_url, thumbnail_url] if operation.eq_ignore_ascii_case("set") => (
            device,
            BackgroundCliOperation::Set {
                image_url: image_url.clone(),
                thumbnail_url: Some(thumbnail_url.clone()),
            },
        ),
        _ => return "Invalid background command arguments\n".into(),
    };
    let device = match DeviceId::new(device_text) {
        Ok(device) => device,
        Err(_) => return "Invalid device selector\n".into(),
    };
    if !access.config().devices.contains_key(&device) {
        return format!("Background command failed: device {device} is not configured\n");
    }
    match access
        .shared
        .background_runtime
        .execute_cli(device, operation)
    {
        Ok(output) => output,
        Err(error) => format!("Background command failed: {error}\n"),
    }
}

fn resolve_effective_background(
    override_background: Option<DeviceBackground>,
    selection: Option<&DeviceBackgroundSelection>,
    device_type: Option<DeviceType>,
) -> Result<EffectiveBackground, DeviceBackgroundError> {
    match (override_background, selection) {
        (Some(background), _) => Ok(EffectiveBackground::Override(
            background.resolve_for(device_type),
        )),
        (None, None) => Ok(EffectiveBackground::Unconfigured),
        (None, Some(DeviceBackgroundSelection::Static(background))) => Ok(
            EffectiveBackground::ConfiguredStatic(background.resolve_for(device_type)),
        ),
        (None, Some(DeviceBackgroundSelection::Dynamic(pattern))) => {
            let background = match device_type {
                Some(device_type) => pattern.resolve(device_type)?,
                None => None,
            };
            Ok(EffectiveBackground::ConfiguredDynamic(background))
        }
    }
}

fn resolve_configured_background(
    selection: Option<&DeviceBackgroundSelection>,
    device_type: Option<DeviceType>,
) -> Result<Option<ResolvedDeviceBackground>, DeviceBackgroundError> {
    match selection {
        Some(selection) => selection.resolve(device_type),
        None => Ok(None),
    }
}

fn registered_device_type(access: &Access, device: &DeviceId) -> Option<DeviceType> {
    controller_step(&access.shared.controller, |controller| {
        controller
            .registered_device(device)
            .map(|registered| registered.registration.device_type)
    })
}

fn same_resources(
    left: Option<&ResolvedDeviceBackground>,
    right: Option<&ResolvedDeviceBackground>,
) -> bool {
    match (left, right) {
        (Some(ResolvedDeviceBackground::Set(left)), Some(ResolvedDeviceBackground::Set(right))) => {
            left.image_url() == right.image_url() && left.thumbnail_url() == right.thumbnail_url()
        }
        (
            Some(ResolvedDeviceBackground::Display(left)),
            Some(ResolvedDeviceBackground::Display(right)),
        ) => left == right,
        (None, None) => true,
        _ => false,
    }
}

fn thumbnail_description(background: Option<&ResolvedDeviceBackground>) -> &'static str {
    match background {
        Some(ResolvedDeviceBackground::Set(background)) => match background.thumbnail_source() {
            BackgroundThumbnailSource::Explicit => "explicit",
            BackgroundThumbnailSource::Derived => "derived",
            BackgroundThumbnailSource::Dynamic => "dynamic",
        },
        Some(ResolvedDeviceBackground::Display(_)) | None => "none",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DynamicBackgroundPattern;

    fn dynamic_selection() -> DeviceBackgroundSelection {
        DeviceBackgroundSelection::Dynamic(
            DynamicBackgroundPattern::new(
                "https://images.example.test/render.{FORMAT}?w={W}&h={H}&bitdepth={B}",
            )
            .unwrap(),
        )
    }

    #[test]
    fn durable_override_takes_precedence_over_dynamic_configuration() {
        let override_background = DeviceBackground::new(
            PhoneBackgroundHttpUrl::new("https://assets.example.test/override.jpg").unwrap(),
            None,
        )
        .unwrap();
        let effective = resolve_effective_background(
            Some(override_background.clone()),
            Some(&dynamic_selection()),
            Some(DeviceType::Cisco7965),
        )
        .unwrap();

        assert!(matches!(
            effective,
            EffectiveBackground::Override(Some(ResolvedDeviceBackground::Set(background)))
                if background == override_background
        ));
    }

    #[test]
    fn dynamic_configuration_waits_for_a_model_and_skips_unsupported_models() {
        let selection = dynamic_selection();
        assert!(matches!(
            resolve_effective_background(None, Some(&selection), None).unwrap(),
            EffectiveBackground::ConfiguredDynamic(None)
        ));
        assert!(matches!(
            resolve_effective_background(None, Some(&selection), Some(DeviceType::Cisco7931))
                .unwrap(),
            EffectiveBackground::ConfiguredDynamic(None)
        ));
        let supported =
            resolve_effective_background(None, Some(&selection), Some(DeviceType::Cisco7965))
                .unwrap();
        assert!(matches!(
            supported,
            EffectiveBackground::ConfiguredDynamic(Some(ResolvedDeviceBackground::Set(background)))
                if background.image_url().as_str()
                    == "https://images.example.test/render.png?w=320&h=212&bitdepth=16"
        ));
    }
}
