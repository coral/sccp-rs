//! Typed composition API consumed by the Asterisk channel callbacks.

use super::boundary::MutexExt as _;
use super::raw::handles::ChannelRef;
use super::runtime::{
    Access, AsteriskBackend, ChannelAllocationOwner, ChannelAllocationRequest, ChannelBinding,
    Module, RuntimeCallSignalDeliveryError, RuntimeCallSignalDeliveryResult, RuntimeCallSignalKind,
    RuntimeCliDiagnosticError, RuntimeCliInventoryError, allocate_channel, ast_log, audio_framing,
    channel_binding, complete_runtime_cli_diagnostics, complete_runtime_cli_inventory, config_path,
    configured_audio_processing, configured_dtmf_mode, device_state, direct_media_call,
    direct_media_policy, enqueue_media_retarget, execute_answer_call_transition,
    execute_forwarding_mutation, format_for, handle_runtime_hangup_signal, install_mwi,
    local_media_endpoint, module_access, preferred_codec_upgrade, preferred_inbound_codec,
    prepare_channel_allocation_text, publish_line, queue_unavailable, read_channel_metadata,
    read_party_snapshot, registered_device_ids, reload, reload_selected, reload_sorcery,
    remove_channel, render_runtime_cli_diagnostics, render_runtime_cli_inventory,
    requestor_auto_answer_mode, retarget_station_to_anchor, state_from_channel, station_nat_active,
    take_state_from_channel, uninstall_mwi, with_channel,
};
use super::{
    AppearanceRingMode, Arc, AsteriskRealtime, AsteriskSorcerySource, AutoAnswerPolicy, CStr,
    CallDirection, CallId, CallMetadata, CallState, CliControlError, Codec,
    ConfigReconciliationObjectType, ConfigReconciliationOperation, ConfigReconciliationTrigger,
    ConfigurationProvider, ConfigurationSource, ConfiguredChannelMetadata, ControlOutcome,
    DeviceId, DeviceState, DirectMediaRoute, DndMode, DriverEffect, Duration,
    FeatureControlMutation, FeatureControlOutcome, ForwardingContext, ForwardingOperation,
    HandsetEffect, HashMap, HybridConfigurationProvider, InboundCallDisposition,
    InboundDialRequest, InboundUnavailableReason, IncomingOfferDelivery, IncomingOfferReceipt,
    IncomingPresentation, IncomingRing, LineBinding, LineInstance, LogLevel,
    MAX_ASSIGNED_CHANNEL_ID_BYTES, MAX_BOOLEAN_BYTES, MAX_CALL_ID_BYTES, MAX_DEVICE_SELECTOR_BYTES,
    MAX_DIAL_DESTINATION_BYTES, MAX_DND_MODE_BYTES, MAX_LINE_SELECTOR_BYTES, MAX_MESSAGE_BYTES,
    MAX_TIMEOUT_BYTES, MODULE, MediaEndpoint, ModuleConfig, NoAnswerPolicy, NonNull, PartySnapshot,
    PbxCallId, PhoneCallState, PhoneCommand, PhoneCommandAction, REQUESTED_CHANNEL_UNAVAILABLE,
    ReloadSelection, ResetMode, ResetTarget, RingDuration, RingerMode, SharedNoAnswerRoute,
    SorceryConfigurationProvider, StationSessionTarget, Tone, USER_BUSY, c_int,
    canonical_ip_address, complete_cli_device, complete_cli_reset_target, complete_cli_value,
    compose_channel_metadata, controller_step, execute_cli_answer, execute_cli_device_control,
    execute_cli_dnd, execute_cli_end, execute_cli_message, execute_cli_originate, native_channel,
    parse_cli_forwarding_mutation, pbx_audio_format, pbx_audio_formats_from_mask,
    pbx_video_formats_from_mask, plan_inbound_bindings, plan_shared_no_answer_route, raw, sys,
};
use crate::config::provider::StaticConfigurationSource as _;
use crate::runtime::backend::SupplementaryBackend as _;
use std::net::IpAddr;
use std::time::Instant;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModuleLifecycleError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChannelOperationError {
    Invalid,
}

impl From<std::sync::mpsc::RecvError> for ChannelOperationError {
    fn from(_: std::sync::mpsc::RecvError) -> Self {
        Self::Invalid
    }
}

impl From<RuntimeCallSignalDeliveryError> for ChannelOperationError {
    fn from(_: RuntimeCallSignalDeliveryError) -> Self {
        Self::Invalid
    }
}

pub struct ChannelOperationReceipt(std::sync::mpsc::Receiver<RuntimeCallSignalDeliveryResult>);

impl ChannelOperationReceipt {
    pub fn wait(self) -> Result<(), ChannelOperationError> {
        Ok(self.0.recv()??)
    }
}

async fn reconcile_incoming_offer(
    access: Access,
    pbx_id: PbxCallId,
    call_id: CallId,
    line: String,
    tone: Option<Tone>,
    tone_interval: Duration,
    receipt: IncomingOfferReceipt,
) {
    match receipt.wait().await {
        Ok(IncomingOfferDelivery::Presented) => {
            let effects = controller_step(&access.shared.controller, |controller| {
                controller.start_call_waiting_tone(call_id, tone, tone_interval, Instant::now())
            });
            for effect in effects {
                let DriverEffect::Handset(HandsetEffect::StartTone {
                    device_id,
                    call_id,
                    tone,
                }) = effect
                else {
                    debug_assert!(false, "call-waiting policy emitted a non-tone effect");
                    continue;
                };
                if let Err(error) = access.phone.try_send(PhoneCommand::new(
                    device_id,
                    PhoneCommandAction::StartTone { call_id, tone },
                )) {
                    controller_step(&access.shared.controller, |controller| {
                        controller.cancel_call_waiting_tone(call_id)
                    });
                    ast_log(
                        LogLevel::Warning,
                        &format!("unable to enqueue SCCP call-waiting tone: {error}"),
                    );
                }
            }
        }
        delivery => {
            let removed_all = controller_step(&access.shared.controller, |controller| {
                let removed = controller.cancel_inbound_offer(call_id);
                controller.cancel_call_waiting_tone(call_id);
                removed && controller.pbx_call(pbx_id).is_none()
            });
            if !removed_all
                && controller_step(&access.shared.controller, |controller| {
                    controller.call(call_id).is_some()
                })
            {
                return;
            }
            ast_log(
                LogLevel::Warning,
                &format!("SCCP incoming presentation failed for call {call_id:?}: {delivery:?}"),
            );
            publish_line(&access, &line);
            if removed_all {
                let _ = with_channel(&access, pbx_id, |channel| unsafe {
                    queue_unavailable(channel)
                });
            }
        }
    }
}

pub struct ChannelRequest<'a> {
    pub capabilities: *mut sys::ast_format_cap,
    pub assigned_ids: *const sys::ast_assigned_ids,
    pub requestor: *const sys::ast_channel,
    pub address: &'a CStr,
}

struct ParsedChannelRequest {
    dial: InboundDialRequest,
    requestor_party: Option<PartySnapshot>,
    requestor_metadata: Option<CallMetadata>,
}

impl ParsedChannelRequest {
    unsafe fn parse(
        address: &CStr,
        requestor: *const sys::ast_channel,
    ) -> Result<Self, ChannelRequestError> {
        let address = address.to_str().map_err(|_| ChannelRequestError {
            cause: Some(REQUESTED_CHANNEL_UNAVAILABLE),
        })?;
        let mut dial = InboundDialRequest::parse(address).map_err(|error| {
            ast_log(
                LogLevel::Warning,
                &format!("unable to parse SCCP channel request: {error}"),
            );
            ChannelRequestError {
                cause: Some(REQUESTED_CHANNEL_UNAVAILABLE),
            }
        })?;
        let requestor_mode = requestor_auto_answer_mode(requestor).map_err(|()| {
            ast_log(
                LogLevel::Warning,
                "unable to parse requestor AUTO_ANSWER mode",
            );
            ChannelRequestError {
                cause: Some(REQUESTED_CHANNEL_UNAVAILABLE),
            }
        })?;
        dial.apply_requestor_mode(requestor_mode);
        let requestor_party = read_party_snapshot(requestor as *mut sys::ast_channel);
        let requestor_metadata = if requestor.is_null() {
            None
        } else {
            Some(
                read_channel_metadata(requestor as *mut sys::ast_channel).ok_or(
                    ChannelRequestError {
                        cause: Some(REQUESTED_CHANNEL_UNAVAILABLE),
                    },
                )?,
            )
        };
        Ok(Self {
            dial,
            requestor_party,
            requestor_metadata,
        })
    }
}

struct SelectedChannelPolicy {
    primary_call_id: CallId,
    primary_binding: LineBinding,
    primary_codec: Codec,
    forwarded: bool,
    no_answer: Option<SharedNoAnswerRoute>,
}

/// Rolls back every controller/runtime/native allocation made while preparing
/// a request unless ownership is explicitly committed to Asterisk.
struct PreparedChannelRequest<'a> {
    access: &'a Access,
    pbx_id: PbxCallId,
    committed: bool,
}

impl<'a> PreparedChannelRequest<'a> {
    const fn new(access: &'a Access, pbx_id: PbxCallId) -> Self {
        Self {
            access,
            pbx_id,
            committed: false,
        }
    }

    fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for PreparedChannelRequest<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        remove_channel(self.access, self.pbx_id);
        let _ = controller_step(&self.access.shared.controller, |controller| {
            controller.pbx_hangup_with_effects(self.pbx_id)
        });
    }
}

pub struct RequestedChannel {
    pub channel: std::ptr::NonNull<sys::ast_channel>,
    /// Asterisk hangup cause explicitly selected by request policy. `None`
    /// preserves the caller's existing cause value.
    pub cause: Option<c_int>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChannelRequestError {
    pub cause: Option<c_int>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChannelIndication {
    StopTone,
    Ringing,
    Answer,
    Busy,
    Congestion,
    Progress,
    Proceeding,
    ConnectedLine,
    Redirecting,
    Incomplete,
    SourceUpdate,
    SourceChange,
    UpdateRtpPeer,
    VideoUpdate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DirectMediaPeer {
    pub address: Option<IpAddr>,
    pub port: u16,
    pub audio_capabilities: u32,
    pub video_capabilities: u32,
    pub nat_active: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaPeerUpdate {
    Direct(DirectMediaPeer),
    Anchor,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChannelSecurity {
    pub signaling: bool,
    pub media: bool,
}

impl From<native_channel::NativeChannelSecurity> for ChannelSecurity {
    fn from(security: native_channel::NativeChannelSecurity) -> Self {
        Self {
            signaling: security.signaling,
            media: security.media,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlCliCommand {
    Dnd,
    Message,
    Answer,
    End,
    Originate,
}

impl ControlCliCommand {
    pub const fn accepts_argument_count(self, count: usize) -> bool {
        match self {
            Self::Dnd => count == 2,
            Self::Message => count >= 2 && count <= 4,
            Self::Answer => count >= 1 && count <= 2,
            Self::End => count == 1,
            Self::Originate => count >= 2 && count <= 4,
        }
    }

    pub const fn argument_bound(self, index: usize) -> Option<usize> {
        match (self, index) {
            (Self::Dnd, 0) => Some(MAX_DEVICE_SELECTOR_BYTES),
            (Self::Dnd, 1) => Some(MAX_DND_MODE_BYTES),
            (Self::Message, 0) => Some(MAX_DEVICE_SELECTOR_BYTES),
            (Self::Message, 1) => Some(MAX_MESSAGE_BYTES),
            (Self::Message, 2) => Some(MAX_BOOLEAN_BYTES),
            (Self::Message, 3) => Some(MAX_TIMEOUT_BYTES),
            (Self::Answer, 0) | (Self::End, 0) => Some(MAX_CALL_ID_BYTES),
            (Self::Answer, 1) | (Self::Originate, 0) => Some(MAX_DEVICE_SELECTOR_BYTES),
            (Self::Originate, 1) => Some(MAX_DIAL_DESTINATION_BYTES),
            (Self::Originate, 2) => Some(MAX_LINE_SELECTOR_BYTES),
            (Self::Originate, 3) => Some(MAX_ASSIGNED_CHANNEL_ID_BYTES),
            _ => None,
        }
    }
}

struct PreparedConfigurationProvider {
    provider: Arc<dyn ConfigurationProvider>,
    sorcery_registration: Option<Arc<raw::sorcery::SorceryRegistration>>,
    source: ConfigurationSource,
}

fn prepare_configuration_provider() -> Result<PreparedConfigurationProvider, String> {
    let file_provider = raw::config::AsteriskConfigurationSource::new(config_path());
    let source = file_provider
        .read_source()
        .and_then(|contents| {
            ModuleConfig::configuration_source_from_source(&contents).map_err(|error| {
                crate::config::provider::ConfigurationProviderError::invalid(
                    file_provider.origin(),
                    error,
                )
            })
        })
        .map_err(|error| error.to_string())?;

    let (provider, sorcery_registration): (
        Arc<dyn ConfigurationProvider>,
        Option<Arc<raw::sorcery::SorceryRegistration>>,
    ) = match source {
        ConfigurationSource::Sorcery => {
            let registration =
                raw::sorcery::SorceryRegistration::register(Arc::new(reconcile_sorcery_mutation))
                    .map(Arc::new)
                    .map_err(|error| error.to_string())?;
            let provider = SorceryConfigurationProvider::new(
                file_provider,
                Arc::new(AsteriskSorcerySource::new(Arc::clone(&registration))),
            );
            (Arc::new(provider), Some(registration))
        }
        ConfigurationSource::File => {
            let provider: Arc<dyn ConfigurationProvider> = if let Some(tables) = file_provider
                .realtime_tables()
                .map_err(|error| error.to_string())?
            {
                Arc::new(HybridConfigurationProvider::from_tables(
                    file_provider,
                    Arc::new(AsteriskRealtime::new()),
                    &tables,
                ))
            } else {
                Arc::new(file_provider)
            };
            (provider, None)
        }
    };

    Ok(PreparedConfigurationProvider {
        provider,
        sorcery_registration,
        source,
    })
}

fn reconcile_sorcery_mutation(mutation: raw::sorcery::SorceryMutation) {
    let Some(access) = module_access() else {
        return;
    };
    let operation = match mutation.kind {
        raw::sorcery::SorceryMutationKind::Created => ConfigReconciliationOperation::Create,
        raw::sorcery::SorceryMutationKind::Updated => ConfigReconciliationOperation::Update,
        raw::sorcery::SorceryMutationKind::Deleted => ConfigReconciliationOperation::Delete,
    };
    let object_type = match mutation.object_type {
        raw::sorcery::SorceryObjectType::Device => ConfigReconciliationObjectType::Device,
        raw::sorcery::SorceryObjectType::Line => ConfigReconciliationObjectType::Line,
    };
    let trigger =
        ConfigReconciliationTrigger::mutation(operation, object_type, mutation.id.clone());
    if let Err(error) = reload_sorcery(&access, trigger) {
        ast_log(
            LogLevel::Warning,
            &format!("SCCP Sorcery reconciliation failed after {mutation:?}: {error}"),
        );
    }
}

pub fn start_module() -> Result<(), ModuleLifecycleError> {
    let mut module = MODULE.lock_unpoisoned();
    if module.is_some() {
        return Ok(());
    }
    let PreparedConfigurationProvider {
        provider: config_provider,
        sorcery_registration,
        source,
    } = match prepare_configuration_provider() {
        Ok(prepared) => prepared,
        Err(error) => {
            ast_log(LogLevel::Error, &error);
            return Err(ModuleLifecycleError);
        }
    };
    let config = match config_provider.load() {
        Ok(config) => config,
        Err(error) => {
            ast_log(LogLevel::Error, &error.to_string());
            return Err(ModuleLifecycleError);
        }
    };
    match Module::start(config_provider, config) {
        Ok(mut started) => {
            started.sorcery_registration = sorcery_registration;
            if let Err(error) = started
                .access
                .shared
                .config_provider
                .activated(&started.access.config())
            {
                ast_log(LogLevel::Error, &error.to_string());
                started.stop();
                return Err(ModuleLifecycleError);
            }
            let access = started.access.clone();
            ast_log(
                LogLevel::Notice,
                &format!(
                    "SCCP driver loaded for Asterisk {} (lane {}, build options {})",
                    env!("SCCP_ASTERISK_VERSION"),
                    env!("SCCP_ASTERISK_LANE"),
                    env!("SCCP_ASTERISK_BUILDOPT_SUM")
                ),
            );
            *module = Some(started);
            drop(module);
            install_mwi(&access);
            if source == ConfigurationSource::Sorcery
                && let Err(error) = reload_sorcery(&access, ConfigReconciliationTrigger::startup())
            {
                ast_log(
                    LogLevel::Warning,
                    &format!("initial SCCP Sorcery reconciliation failed: {error}"),
                );
            }
            Ok(())
        }
        Err(error) => {
            ast_log(LogLevel::Error, &error);
            Err(ModuleLifecycleError)
        }
    }
}

pub fn stop_module() -> Result<(), ModuleLifecycleError> {
    let module = MODULE.lock_unpoisoned().take();
    if let Some(module) = module {
        if let Some(registration) = &module.sorcery_registration {
            registration.shutdown_observers();
        }
        uninstall_mwi(&module.access);
        module.stop();
    }
    let _ = raw::system::set_global_variable(raw::system::CONFIG_STATUS_VARIABLE, None);
    Ok(())
}

pub fn has_active_channels() -> bool {
    module_access().is_some_and(|access| !access.shared.channels.lock_unpoisoned().is_empty())
}

pub fn reload_module() -> Result<(), ModuleLifecycleError> {
    let Some(access) = module_access() else {
        return Err(ModuleLifecycleError);
    };
    match reload(&access) {
        Ok(()) => {
            ast_log(LogLevel::Notice, "SCCP configuration reloaded");
            Ok(())
        }
        Err(error) => {
            ast_log(LogLevel::Error, &error);
            Err(ModuleLifecycleError)
        }
    }
}

pub unsafe fn request_channel(
    request: ChannelRequest<'_>,
) -> Result<RequestedChannel, ChannelRequestError> {
    let ChannelRequest {
        capabilities,
        assigned_ids,
        requestor,
        address,
    } = request;
    let Some(access) = module_access() else {
        return Err(ChannelRequestError { cause: None });
    };
    let ParsedChannelRequest {
        dial: dial_request,
        requestor_party,
        requestor_metadata,
    } = unsafe { ParsedChannelRequest::parse(address, requestor) }?;
    let Some(request_capabilities) = NonNull::new(capabilities) else {
        return Err(ChannelRequestError {
            cause: Some(REQUESTED_CHANNEL_UNAVAILABLE),
        });
    };
    let request_formats = pbx_audio_formats_from_mask(unsafe {
        native_channel::audio_capability_mask(Some(request_capabilities)).bits()
    });
    let request_video_formats = pbx_video_formats_from_mask(unsafe {
        native_channel::video_capability_mask(Some(request_capabilities)).bits()
    });
    let config = access.config();
    let registered = controller_step(&access.shared.controller, |controller| {
        controller
            .registered_devices()
            .map(|(device_id, _)| device_id.clone())
            .collect::<Vec<_>>()
    });
    let bindings = access.inbound_line_bindings(dial_request.target());
    let ring_enabled = bindings
        .iter()
        .filter(|binding| binding.appearance.ring_mode != AppearanceRingMode::Disabled)
        .count();
    let eligible_bindings = bindings
        .iter()
        .filter(|binding| {
            binding.appearance.ring_mode != AppearanceRingMode::Disabled
                && registered.contains(&binding.device_id)
        })
        .cloned()
        .collect::<Vec<_>>();
    let registered_ring_enabled = eligible_bindings.len();
    let candidates = plan_inbound_bindings(
        eligible_bindings,
        |_| true,
        || access.phone.reserve_call_id(),
        |binding| {
            let selected = unsafe {
                preferred_inbound_codec(
                    &access,
                    &binding.device_id,
                    binding.appearance.instance,
                    request_capabilities,
                )
            };
            if let Some(codec) = selected
                && pbx_audio_format(codec)
                    .ok()
                    .is_some_and(|format| !request_formats.contains(&format))
            {
                ast_log(
                    LogLevel::Debug,
                    &format!(
                        "SCCP inbound target {} selected {codec:?} through Asterisk translation from {request_formats:?}",
                        dial_request.target()
                    ),
                );
            }
            selected
        },
    );
    if candidates.is_empty() {
        let reason = if bindings.is_empty() {
            "target has no configured SCCP appearance".to_owned()
        } else if ring_enabled == 0 {
            "all configured SCCP appearances have ringing disabled".to_owned()
        } else if registered_ring_enabled == 0 {
            "no ring-enabled SCCP appearance is registered".to_owned()
        } else {
            format!(
                "no registered ring-enabled SCCP appearance supports a configured station codec for requested formats {request_formats:?}"
            )
        };
        ast_log(
            LogLevel::Warning,
            &format!(
                "unable to offer inbound SCCP target {}: {reason}",
                dial_request.target(),
            ),
        );
        return Err(ChannelRequestError {
            cause: Some(REQUESTED_CHANNEL_UNAVAILABLE),
        });
    }
    let pbx_id = PbxCallId(candidates[0].call_id.0);
    let mut allocation_texts = candidates
        .iter()
        .map(|candidate| {
            prepare_channel_allocation_text(&access, &candidate.binding, pbx_id)
                .map(|text| (candidate.call_id, text))
        })
        .collect::<Result<HashMap<_, _>, _>>()
        .map_err(|error| {
            ast_log(
                LogLevel::Warning,
                &format!("unable to prepare SCCP native channel text: {error}"),
            );
            ChannelRequestError {
                cause: Some(REQUESTED_CHANNEL_UNAVAILABLE),
            }
        })?;
    let mut prepared = PreparedChannelRequest::new(&access, pbx_id);
    let disposition = controller_step(&access.shared.controller, |controller| {
        controller.offer_inbound_call_with_policy(pbx_id, candidates.clone())
    });
    let selected = match disposition {
        InboundCallDisposition::Offer(offers) => {
            let Some(primary_offer) = offers.first() else {
                return Err(ChannelRequestError {
                    cause: Some(REQUESTED_CHANNEL_UNAVAILABLE),
                });
            };
            let Some(primary) = candidates
                .iter()
                .find(|candidate| candidate.call_id == primary_offer.call_id)
            else {
                return Err(ChannelRequestError {
                    cause: Some(REQUESTED_CHANNEL_UNAVAILABLE),
                });
            };
            let no_answer_destinations = controller_step(&access.shared.controller, |controller| {
                offers
                    .iter()
                    .map(|offer| {
                        (
                            offer.device_id.clone(),
                            controller
                                .feature_state(&offer.device_id)
                                .and_then(|state| state.forwarding.no_answer.clone()),
                        )
                    })
                    .collect::<HashMap<_, _>>()
            });
            let no_answer = plan_shared_no_answer_route(offers.iter().filter_map(|offer| {
                let binding = access.line_binding(&offer.device_id, offer.line_instance)?;
                let device = config.devices.get(&offer.device_id)?;
                Some(NoAnswerPolicy {
                    context: ForwardingContext::new(&binding.line.context).ok()?,
                    destination: no_answer_destinations
                        .get(&offer.device_id)
                        .cloned()
                        .flatten(),
                    timeout_seconds: device.feature_defaults.forwarding.no_answer_timeout_seconds,
                })
            }));
            SelectedChannelPolicy {
                primary_call_id: primary.call_id,
                primary_binding: primary.binding.clone(),
                primary_codec: primary.codec,
                forwarded: false,
                no_answer,
            }
        }
        InboundCallDisposition::Forward {
            binding,
            destination,
            reason,
        } => {
            let Some(primary) = candidates.iter().find(|candidate| {
                candidate.binding.device_id == binding.device_id
                    && candidate.binding.line_instance == binding.line_instance
            }) else {
                return Err(ChannelRequestError {
                    cause: Some(REQUESTED_CHANNEL_UNAVAILABLE),
                });
            };
            let Ok(context) = ForwardingContext::new(&binding.line.context) else {
                return Err(ChannelRequestError {
                    cause: Some(REQUESTED_CHANNEL_UNAVAILABLE),
                });
            };
            access.shared.forwarded_calls.lock_unpoisoned().insert(
                pbx_id,
                ForwardingOperation {
                    call_id: pbx_id,
                    context,
                    destination,
                    reason,
                },
            );
            SelectedChannelPolicy {
                primary_call_id: primary.call_id,
                primary_binding: *binding,
                primary_codec: primary.codec,
                forwarded: true,
                no_answer: None,
            }
        }
        InboundCallDisposition::Unavailable(reason) => {
            return Err(ChannelRequestError {
                cause: Some(if reason == InboundUnavailableReason::DoNotDisturb {
                    USER_BUSY
                } else {
                    REQUESTED_CHANNEL_UNAVAILABLE
                }),
            });
        }
    };
    let SelectedChannelPolicy {
        primary_call_id,
        primary_binding,
        primary_codec,
        forwarded,
        no_answer,
    } = selected;
    let allocation_text = allocation_texts
        .remove(&primary_call_id)
        .ok_or(ChannelRequestError {
            cause: Some(REQUESTED_CHANNEL_UNAVAILABLE),
        })?;
    if !forwarded
        && let Some(request) = dial_request.auto_answer()
        && !controller_step(&access.shared.controller, |controller| {
            controller.set_auto_answer_request(pbx_id, request)
        })
    {
        return Err(ChannelRequestError {
            cause: Some(REQUESTED_CHANNEL_UNAVAILABLE),
        });
    }
    let metadata = match compose_channel_metadata(
        requestor_metadata.unwrap_or_default(),
        ConfiguredChannelMetadata {
            direction: CallDirection::Inbound,
            caller_number: &primary_binding.line.caller_number,
            dialed_number: None,
            account_code: primary_binding.line.account_code.as_deref(),
            language: &primary_binding.line.language,
            device_variables: &[],
            line_variables: &primary_binding.line.channel_variables,
        },
    ) {
        Ok(metadata) => metadata,
        Err(_) => {
            return Err(ChannelRequestError {
                cause: Some(REQUESTED_CHANNEL_UNAVAILABLE),
            });
        }
    };
    if !forwarded
        && !matches!(
            controller_step(&access.shared.controller, |controller| {
                controller.set_call_metadata(pbx_id, metadata.clone())
            }),
            Ok(true)
        )
    {
        return Err(ChannelRequestError {
            cause: Some(REQUESTED_CHANNEL_UNAVAILABLE),
        });
    }
    if !forwarded && let Some(snapshot) = requestor_party.as_ref() {
        // These appearances have not been offered yet; store the
        // typed seed and let `place_call` perform the first send.
        let _ = controller_step(&access.shared.controller, |controller| {
            controller.update_call_info_by_pbx(pbx_id, |current| {
                snapshot.apply_initial_inbound_to_call_info(current)
            })
        });
    }
    let directly_requested = pbx_audio_format(primary_codec)
        .ok()
        .is_some_and(|format| request_formats.contains(&format));
    let packet_ms = format_for(primary_codec).and_then(|format| unsafe {
        native_channel::audio_framing(
            directly_requested
                .then(|| NonNull::new(capabilities))
                .flatten(),
            format,
        )
    });
    let Some(packet_ms) = packet_ms else {
        return Err(ChannelRequestError {
            cause: Some(REQUESTED_CHANNEL_UNAVAILABLE),
        });
    };
    if let Some(route) = no_answer {
        access
            .shared
            .no_answer_plans
            .lock_unpoisoned()
            .insert(pbx_id, route);
    }
    if allocate_channel(
        &access,
        ChannelAllocationRequest {
            sccp_id: primary_call_id,
            pbx_id,
            binding: &primary_binding,
            codec: primary_codec,
            pbx_video_formats: &request_video_formats,
            assigned_ids,
            requestor,
            metadata: Some(metadata),
            text: allocation_text,
            owner: ChannelAllocationOwner::Asterisk,
        },
    )
    .is_err()
    {
        return Err(ChannelRequestError { cause: None });
    }
    access
        .shared
        .audio_packet_ms
        .lock_unpoisoned()
        .insert(pbx_id, packet_ms);
    let cause = dial_request
        .auto_answer()
        .and_then(|request| request.unavailable_cause)
        .map_or(0, |cause| cause.asterisk_code());
    let channel = channel_binding(&access, pbx_id)
        .and_then(|binding| binding.try_enter())
        .map(|channel| channel.resource().as_non_null())
        .ok_or(ChannelRequestError { cause: None })?;
    prepared.commit();
    Ok(RequestedChannel {
        channel,
        cause: Some(cause),
    })
}

pub unsafe fn place_call(
    channel: std::ptr::NonNull<sys::ast_channel>,
) -> Result<(), ChannelOperationError> {
    let Some(access) = module_access() else {
        return Err(ChannelOperationError::Invalid);
    };
    let Some(state) = (unsafe { state_from_channel(channel.as_ptr()) }) else {
        return Err(ChannelOperationError::Invalid);
    };
    let config = access.config();
    let forwarded = access
        .shared
        .forwarded_calls
        .lock_unpoisoned()
        .get(&state.pbx_id)
        .cloned();
    if let Some(route) = forwarded {
        return match AsteriskBackend::new(&access).forward(&route) {
            Ok(()) => Ok(()),
            Err(error) => {
                ast_log(
                    LogLevel::Warning,
                    &format!("unable to forward PBX call {}: {error}", state.pbx_id.0),
                );
                Err(ChannelOperationError::Invalid)
            }
        };
    }
    let no_answer_plan = access
        .shared
        .no_answer_plans
        .lock_unpoisoned()
        .remove(&state.pbx_id);
    let Some((line, offers)) = controller_step(&access.shared.controller, |controller| {
        let call = controller.pbx_call(state.pbx_id)?;
        let offers = controller
            .inbound_offers_for_pbx(state.pbx_id)
            .into_iter()
            .filter_map(|offer| {
                let generation = controller
                    .registered_device(&offer.device_id)?
                    .session_generation;
                Some((offer, generation))
            })
            .collect::<Vec<_>>();
        Some((call.line.clone(), offers))
    }) else {
        return Err(ChannelOperationError::Invalid);
    };
    if offers.is_empty() {
        return Err(ChannelOperationError::Invalid);
    }
    let mut offered = 0;
    let mut enqueued = Vec::with_capacity(offers.len());
    for (offer, session_generation) in offers {
        let Some(info) = controller_step(&access.shared.controller, |controller| {
            controller.call_info(offer.call_id).cloned()
        }) else {
            continue;
        };
        let ringer = match offer.ring_mode {
            AppearanceRingMode::Normal => Some(IncomingRing {
                mode: config.general.ring_type,
                duration: RingDuration::Normal,
            }),
            AppearanceRingMode::Silent => Some(IncomingRing {
                mode: RingerMode::Silent,
                duration: RingDuration::Normal,
            }),
            AppearanceRingMode::Disabled => None,
        };
        let presentation = match offer.state {
            PhoneCallState::RingIn => IncomingPresentation::RingIn,
            PhoneCallState::CallWaiting => IncomingPresentation::CallWaiting,
            _ => {
                debug_assert!(false, "incoming offer has a non-incoming call state");
                continue;
            }
        };
        match access.phone.try_offer_incoming_call_for_session(
            StationSessionTarget::new(offer.device_id.clone(), session_generation),
            LineInstance::new(offer.line_instance),
            offer.call_id,
            info,
            presentation,
            ringer,
        ) {
            Ok(receipt) => {
                offered += 1;
                enqueued.push((offer.device_id, offer.call_id));
                let reconcile = access.clone();
                let reconcile_line = line.clone();
                let pbx_id = state.pbx_id;
                let call_id = offer.call_id;
                let tone = config.general.call_waiting_tone;
                let tone_interval =
                    Duration::from_secs(config.general.call_waiting_interval_seconds.into());
                access.handle.spawn(async move {
                    reconcile_incoming_offer(
                        reconcile,
                        pbx_id,
                        call_id,
                        reconcile_line,
                        tone,
                        tone_interval,
                        receipt,
                    )
                    .await;
                });
            }
            Err(error) => {
                controller_step(&access.shared.controller, |controller| {
                    controller.cancel_inbound_offer(offer.call_id)
                });
                ast_log(
                    LogLevel::Warning,
                    &format!("unable to enqueue SCCP incoming call: {error}"),
                );
            }
        }
    }
    if offered == 0 {
        publish_line(&access, &line);
        return Err(ChannelOperationError::Invalid);
    }
    if unsafe { native_channel::start_ringing(channel) }.is_err() {
        for (device_id, call_id) in enqueued {
            controller_step(&access.shared.controller, |controller| {
                controller.cancel_inbound_offer(call_id)
            });
            if let Err(error) = access.phone.try_send(PhoneCommand::new(
                device_id,
                PhoneCommandAction::CloseCall { call_id },
            )) {
                ast_log(
                    LogLevel::Warning,
                    &format!("unable to enqueue SCCP incoming-call cleanup: {error}"),
                );
            }
        }
        publish_line(&access, &line);
        return Err(ChannelOperationError::Invalid);
    }
    publish_line(&access, &line);
    if let Some(route) = no_answer_plan {
        let deadline = Instant::now() + route.timeout;
        if let Err(error) = access.shared.no_answer_timers.lock_unpoisoned().schedule(
            state.pbx_id,
            deadline,
            route.context,
            route.destination,
        ) {
            ast_log(
                LogLevel::Warning,
                &format!("unable to schedule SCCP no-answer route: {error}"),
            );
        }
    }
    let now = Instant::now();
    let auto_answer = AutoAnswerPolicy {
        delay: Duration::from_secs(u64::from(config.auto_answer().ring_time_seconds)),
        tone: config.auto_answer().tone,
    };
    let (scheduled, transitions) = controller_step(&access.shared.controller, |controller| {
        if controller.has_auto_answer_request(state.pbx_id) {
            let scheduled = controller.schedule_auto_answers(state.pbx_id, auto_answer, now);
            (Some(scheduled), controller.expire_auto_answers(now))
        } else {
            (None, Vec::new())
        }
    });
    if let Some(Err(error)) = scheduled {
        ast_log(
            LogLevel::Warning,
            &format!("unable to schedule SCCP auto-answer: {error:?}"),
        );
    }
    for transition in transitions {
        let execute = access.clone();
        access.handle.spawn(async move {
            execute_answer_call_transition(&execute, transition).await;
        });
    }
    Ok(())
}

pub unsafe fn hangup_channel(
    channel: std::ptr::NonNull<sys::ast_channel>,
) -> Result<(), ChannelOperationError> {
    let Some(state) = (unsafe { take_state_from_channel(channel.as_ptr()) }) else {
        return Ok(());
    };
    let Some(access) = module_access() else {
        return Ok(());
    };
    let handset_call_id = controller_step(&access.shared.controller, |controller| {
        controller
            .active_or_primary_call_by_pbx(state.pbx_id)
            .map(|call| call.sccp_id)
    })
    .unwrap_or(state.sccp_id);
    let binding = access
        .shared
        .channels
        .lock_unpoisoned()
        .get(&state.pbx_id)
        .cloned();
    if let Some(binding) = binding {
        drop(binding.close());
    }
    if !access.enqueue_call_signal(
        state.pbx_id,
        RuntimeCallSignalKind::Hangup { handset_call_id },
    ) {
        let cleanup = access.clone();
        access.handle.spawn(async move {
            handle_runtime_hangup_signal(&cleanup, state.pbx_id, handset_call_id).await;
        });
    }
    Ok(())
}

pub unsafe fn answer_channel(
    channel: std::ptr::NonNull<sys::ast_channel>,
) -> Result<ChannelOperationReceipt, ChannelOperationError> {
    let Some(access) = module_access() else {
        return Err(ChannelOperationError::Invalid);
    };
    let Some(state) = (unsafe { state_from_channel(channel.as_ptr()) }) else {
        return Err(ChannelOperationError::Invalid);
    };
    access
        .enqueue_confirmed_answer_signal(state.pbx_id)
        .map(ChannelOperationReceipt)
        .ok_or(ChannelOperationError::Invalid)
}

pub unsafe fn indicate_channel(
    channel: std::ptr::NonNull<sys::ast_channel>,
    indication: ChannelIndication,
) -> Result<(), ChannelOperationError> {
    let channel_ptr = channel.as_ptr();
    match indication {
        ChannelIndication::SourceUpdate => {
            return unsafe { native_channel::update_source(channel) }
                .map_err(|_| ChannelOperationError::Invalid);
        }
        ChannelIndication::SourceChange => {
            return unsafe { native_channel::change_source(channel) }
                .map_err(|_| ChannelOperationError::Invalid);
        }
        ChannelIndication::UpdateRtpPeer => return Ok(()),
        _ => {}
    }
    let Some(access) = module_access() else {
        return Err(ChannelOperationError::Invalid);
    };
    let Some(state) = (unsafe { state_from_channel(channel_ptr) }) else {
        return Err(ChannelOperationError::Invalid);
    };
    let kind = match indication {
        ChannelIndication::StopTone => RuntimeCallSignalKind::StopTone,
        // Remote answer is owned by the technology `answer` callback.
        // Treating the control indication as a second answer entry point
        // would race an in-flight OpenReceiveChannel transaction.
        ChannelIndication::Answer => return Err(ChannelOperationError::Invalid),
        ChannelIndication::Proceeding => RuntimeCallSignalKind::Proceeding,
        ChannelIndication::Ringing => RuntimeCallSignalKind::Ringing,
        ChannelIndication::Progress => RuntimeCallSignalKind::Progress,
        ChannelIndication::Busy => RuntimeCallSignalKind::Busy,
        ChannelIndication::Congestion | ChannelIndication::Incomplete => {
            RuntimeCallSignalKind::Congestion
        }
        ChannelIndication::VideoUpdate => RuntimeCallSignalKind::VideoUpdate,
        ChannelIndication::ConnectedLine | ChannelIndication::Redirecting => {
            let Some(snapshot) = read_party_snapshot(channel_ptr) else {
                return Err(ChannelOperationError::Invalid);
            };
            RuntimeCallSignalKind::PartyUpdate(Box::new(snapshot))
        }
        ChannelIndication::SourceUpdate
        | ChannelIndication::SourceChange
        | ChannelIndication::UpdateRtpPeer => unreachable!(),
    };
    if !access.enqueue_call_signal(state.pbx_id, kind) {
        return Err(ChannelOperationError::Invalid);
    }
    Ok(())
}

pub unsafe fn send_digit_begin_to_channel(
    channel: std::ptr::NonNull<sys::ast_channel>,
    digit: u8,
) -> Result<(), ChannelOperationError> {
    unsafe { native_channel::send_digit_begin(channel, digit) }
        .map_err(|_| ChannelOperationError::Invalid)
}

pub unsafe fn send_digit_end_to_channel(
    channel: std::ptr::NonNull<sys::ast_channel>,
    digit: u8,
    duration: Duration,
) -> Result<(), ChannelOperationError> {
    let duration_ms = u32::try_from(duration.as_millis()).unwrap_or(u32::MAX);
    unsafe { native_channel::send_digit_end(channel, digit, duration_ms) }
        .map_err(|_| ChannelOperationError::Invalid)
}

pub unsafe fn send_text_to_channel(
    channel: std::ptr::NonNull<sys::ast_channel>,
    text: String,
) -> Result<(), ChannelOperationError> {
    let access = module_access().ok_or(ChannelOperationError::Invalid)?;
    let state =
        unsafe { state_from_channel(channel.as_ptr()) }.ok_or(ChannelOperationError::Invalid)?;
    let call = controller_step(&access.shared.controller, |controller| {
        controller.active_or_primary_call_by_pbx(state.pbx_id)
    })
    .ok_or(ChannelOperationError::Invalid)?;
    access
        .phone
        .try_send(PhoneCommand::new(
            call.device_id,
            PhoneCommandAction::DisplayPrompt {
                call_id: call.sccp_id,
                timeout_seconds: 0,
                text,
            },
        ))
        .map_err(|_| ChannelOperationError::Invalid)
}

pub unsafe fn fixup_channel(
    _old_channel: std::ptr::NonNull<sys::ast_channel>,
    new_channel: std::ptr::NonNull<sys::ast_channel>,
) -> Result<(), ChannelOperationError> {
    let new_channel = new_channel.as_ptr();
    let Some(access) = module_access() else {
        return Err(ChannelOperationError::Invalid);
    };
    let Some(retained) = (unsafe { ChannelRef::acquire(new_channel) }) else {
        return Err(ChannelOperationError::Invalid);
    };
    let binding = unsafe { binding_for_channel(&access, new_channel) }?;
    binding
        .replace_quiescent(retained)
        .then_some(())
        .ok_or(ChannelOperationError::Invalid)
}

unsafe fn binding_for_channel(
    access: &Access,
    channel: *mut sys::ast_channel,
) -> Result<Arc<ChannelBinding>, ChannelOperationError> {
    let state = unsafe { state_from_channel(channel) }.ok_or(ChannelOperationError::Invalid)?;
    channel_binding(access, state.pbx_id).ok_or(ChannelOperationError::Invalid)
}

pub unsafe fn suspend_channel_operations(
    channel: std::ptr::NonNull<sys::ast_channel>,
) -> Result<(), ChannelOperationError> {
    let access = module_access().ok_or(ChannelOperationError::Invalid)?;
    let binding = unsafe { binding_for_channel(&access, channel.as_ptr()) }?;
    binding
        .suspend()
        .then_some(())
        .ok_or(ChannelOperationError::Invalid)
}

pub unsafe fn resume_channel_operations(
    channel: std::ptr::NonNull<sys::ast_channel>,
) -> Result<(), ChannelOperationError> {
    let access = module_access().ok_or(ChannelOperationError::Invalid)?;
    let binding = unsafe { binding_for_channel(&access, channel.as_ptr()) }?;
    binding
        .resume()
        .then_some(())
        .ok_or(ChannelOperationError::Invalid)
}

pub unsafe fn direct_media_allowed(channel: std::ptr::NonNull<sys::ast_channel>) -> bool {
    let Some(access) = module_access() else {
        return false;
    };
    let Some(call) = direct_media_call(&access, channel.as_ptr()) else {
        return false;
    };
    if call.codec == Codec::Wideband256k {
        return false;
    }
    let config = access.config();
    let Some(policy) = direct_media_policy(&access, &config, &call) else {
        return false;
    };
    let Some(nat_active) = station_nat_active(&access, &config, &call.device_id) else {
        return false;
    };
    policy
        .validate(
            call.phone_endpoint.address,
            call.phone_endpoint.address,
            nat_active,
            true,
        )
        .is_ok()
}

pub unsafe fn channel_security(
    channel: std::ptr::NonNull<sys::ast_channel>,
) -> Result<ChannelSecurity, ChannelOperationError> {
    unsafe { native_channel::channel_security(channel) }
        .map(ChannelSecurity::from)
        .ok_or(ChannelOperationError::Invalid)
}

pub unsafe fn set_channel_audio_format(
    channel: std::ptr::NonNull<sys::ast_channel>,
    requested: std::ptr::NonNull<sys::ast_format>,
) -> Result<(), ChannelOperationError> {
    let access = module_access().ok_or(ChannelOperationError::Invalid)?;
    let state =
        unsafe { state_from_channel(channel.as_ptr()) }.ok_or(ChannelOperationError::Invalid)?;
    let requested = unsafe { native_channel::identify_audio_format(requested) }
        .map(super::runtime::pbx_audio_format_from_native)
        .ok_or(ChannelOperationError::Invalid)?;
    let call = controller_step(&access.shared.controller, |controller| {
        controller.active_or_primary_call_by_pbx(state.pbx_id)
    })
    .ok_or(ChannelOperationError::Invalid)?;
    if pbx_audio_format(call.codec).is_ok_and(|current| current == requested) {
        return Ok(());
    }
    let codec = preferred_codec_upgrade(
        &access,
        &call.device_id,
        call.line_instance,
        call.codec,
        &[requested],
    )
    .ok_or(ChannelOperationError::Invalid)?;
    let previous = controller_step(&access.shared.controller, |controller| {
        controller.set_held_codec(state.pbx_id, call.sccp_id, codec)
    })
    .ok_or(ChannelOperationError::Invalid)?;
    if unsafe {
        native_channel::set_private_audio_codec(
            channel,
            format_for(codec).ok_or(ChannelOperationError::Invalid)?,
        )
    }
    .is_err()
    {
        let _ = controller_step(&access.shared.controller, |controller| {
            controller.set_held_codec(state.pbx_id, call.sccp_id, previous)
        });
        return Err(ChannelOperationError::Invalid);
    }
    ast_log(
        LogLevel::Notice,
        &format!(
            "upgraded held SCCP call {} from {:?} to {:?} for bridge compatibility",
            call.sccp_id.0, previous, codec
        ),
    );
    Ok(())
}

pub unsafe fn update_rtp_peer(
    channel: std::ptr::NonNull<sys::ast_channel>,
    update: MediaPeerUpdate,
) -> Result<(), ChannelOperationError> {
    let channel = channel.as_ptr();
    let Some(access) = module_access() else {
        return Err(ChannelOperationError::Invalid);
    };
    let Some(call) = direct_media_call(&access, channel) else {
        return Err(ChannelOperationError::Invalid);
    };
    let MediaPeerUpdate::Direct(peer) = update else {
        return retarget_station_to_anchor(&access, &call)
            .then_some(())
            .ok_or(ChannelOperationError::Invalid);
    };
    let peer_address = if peer.port == 0 || peer.port == u16::MAX {
        None
    } else {
        peer.address.map(canonical_ip_address)
    };
    let config = access.config();
    let Some(policy) = direct_media_policy(&access, &config, &call) else {
        return Err(ChannelOperationError::Invalid);
    };
    let detected_nat = station_nat_active(&access, &config, &call.device_id).unwrap_or(true);
    let peer_formats = pbx_audio_formats_from_mask(peer.audio_capabilities);
    // let _peer_video_formats = pbx_video_formats_from_mask(peer.video_capabilities);
    let peer_supports_codec = pbx_audio_format(call.codec)
        .is_ok_and(|selected| peer_formats.contains(&selected))
        // This format uses a station-specific dynamic payload assignment, so
        // it remains anchored until peer payload maps are available here.
        && call.codec != Codec::Wideband256k;
    let route = policy.route(
        call.phone_endpoint.address,
        peer_address,
        peer.nat_active || detected_nat,
        peer_supports_codec,
    );
    let framing = audio_framing(&access, &call.device_id, call.call_id, call.codec)
        .map_err(|_| ChannelOperationError::Invalid)?;
    let endpoint = match route {
        DirectMediaRoute::Direct => {
            let Some(address) = peer_address else {
                return Err(ChannelOperationError::Invalid);
            };
            MediaEndpoint {
                address,
                rtp_port: peer.port,
                rtcp_port: peer.port + 1,
                codec: call.codec,
                packet_ms: framing.packet_ms,
                max_frames_per_packet: framing.max_frames_per_packet,
                telephone_event_payload: 0,
            }
        }
        DirectMediaRoute::Anchored(_) => {
            let Some(mut endpoint) =
                local_media_endpoint(&access, call.pbx_id, &call.device_id, call.codec)
            else {
                return Err(ChannelOperationError::Invalid);
            };
            endpoint.packet_ms = framing.packet_ms;
            endpoint.max_frames_per_packet = framing.max_frames_per_packet;
            endpoint
        }
    };
    let dtmf_mode = configured_dtmf_mode(&access, &call.device_id, call.call_id);
    let audio_processing = configured_audio_processing(&access, &call.device_id, call.call_id);
    if enqueue_media_retarget(&access, &call, endpoint, dtmf_mode, audio_processing) {
        Ok(())
    } else {
        ast_log(LogLevel::Warning, "unable to enqueue media-peer update");
        Err(ChannelOperationError::Invalid)
    }
}

pub fn line_device_state(line: &str) -> DeviceState {
    let Some(access) = module_access() else {
        return DeviceState::Unavailable;
    };
    device_state(&access, line)
}

pub fn execute_version_cli(fd: c_int) {
    raw::system::cli_write(fd, concat!(env!("CARGO_PKG_VERSION"), "\n"));
}

pub fn execute_reload_cli(fd: c_int, arguments: &[String]) {
    let Some(access) = module_access() else {
        return;
    };
    let borrowed = arguments.iter().map(String::as_str).collect::<Vec<_>>();
    let output = match ReloadSelection::parse(&borrowed)
        .map_err(|error| error.to_string())
        .and_then(|selection| reload_selected(&access, selection))
    {
        Ok(()) => "SCCP configuration reloaded\n".into(),
        Err(error) => format!("Reload failed: {error}\n"),
    };
    raw::system::cli_write(fd, &output);
}

pub fn complete_reload_cli(arguments: &[String], prefix: &str, ordinal: usize) -> Option<String> {
    let access = module_access()?;
    let borrowed = arguments.iter().map(String::as_str).collect::<Vec<_>>();
    crate::config::reload::complete_reload_selection(&borrowed, prefix, ordinal, &access.config())
}

pub fn execute_inventory_cli(
    fd: c_int,
    command: crate::ami::cli::CliInventoryCommand,
    arguments: &[String],
) {
    let borrowed = arguments.iter().map(String::as_str).collect::<Vec<_>>();
    let output = module_access()
        .ok_or(RuntimeCliInventoryError::Unavailable)
        .and_then(|access| render_runtime_cli_inventory(&access, command, &borrowed))
        .unwrap_or_else(|error| format!("Inventory query failed: {error}\n"));
    raw::system::cli_write(fd, &output);
}

pub fn complete_inventory_cli(
    command: crate::ami::cli::CliInventoryCommand,
    arguments: &[String],
    prefix: &str,
    ordinal: usize,
) -> Option<String> {
    let access = module_access()?;
    let borrowed = arguments.iter().map(String::as_str).collect::<Vec<_>>();
    complete_runtime_cli_inventory(&access, command, &borrowed, prefix, ordinal)
}

pub fn execute_diagnostic_cli(
    fd: c_int,
    command: crate::ami::diagnostics::CliDiagnosticCommand,
    arguments: &[String],
) {
    let borrowed = arguments.iter().map(String::as_str).collect::<Vec<_>>();
    let output = module_access()
        .ok_or(RuntimeCliDiagnosticError::Unavailable)
        .and_then(|access| render_runtime_cli_diagnostics(&access, command, &borrowed))
        .unwrap_or_else(|error| format!("Diagnostic query failed: {error}\n"));
    raw::system::cli_write(fd, &output);
}

pub fn complete_diagnostic_cli(
    command: crate::ami::diagnostics::CliDiagnosticCommand,
    arguments: &[String],
    prefix: &str,
    ordinal: usize,
) -> Option<String> {
    let access = module_access()?;
    let borrowed = arguments.iter().map(String::as_str).collect::<Vec<_>>();
    complete_runtime_cli_diagnostics(&access, command, &borrowed, prefix, ordinal)
}

pub fn execute_device_control_cli(fd: c_int, device: &str, mode: ResetMode) {
    let output = match module_access() {
        Some(access) => {
            match execute_cli_device_control(&access.control_provider(), device, mode) {
                Ok(ControlOutcome::Reset {
                    target: ResetTarget::Device(device_id),
                    mode,
                    ..
                }) => format!("{device_id}: {} command delivered\n", mode.as_str()),
                Ok(ControlOutcome::Reset {
                    target: ResetTarget::RegisteredDevices,
                    mode,
                    attempted,
                    delivered,
                }) => format!(
                    "{} command: {attempted} attempted, {delivered} delivered, {} failed\n",
                    mode.as_str(),
                    attempted.saturating_sub(delivered)
                ),
                Ok(_) => "Device command returned an unexpected result\n".to_owned(),
                Err(CliControlError::InvalidDevice) => "Invalid device selector\n".to_owned(),
                Err(CliControlError::Provider(error)) => {
                    format!("Device command failed: {error}\n")
                }
                Err(_) => "Invalid device command\n".to_owned(),
            }
        }
        None => "Device controls are unavailable\n".to_owned(),
    };
    raw::system::cli_write(fd, &output);
}

pub fn complete_device_control_cli(prefix: &str, ordinal: usize) -> Option<String> {
    let access = module_access()?;
    let devices = registered_device_ids(&access.shared);
    complete_cli_reset_target(devices.iter(), prefix, ordinal)
}

pub fn execute_control_cli(fd: c_int, command: ControlCliCommand, arguments: &[String]) {
    let output = module_access()
        .map(|access| execute_control_cli_with_access(&access, command, arguments))
        .unwrap_or_else(|| "SCCP controls are unavailable\n".to_owned());
    raw::system::cli_write(fd, &output);
}

fn execute_control_cli_with_access(
    access: &Access,
    command: ControlCliCommand,
    arguments: &[String],
) -> String {
    let provider = access.control_provider();
    match (command, arguments) {
        (ControlCliCommand::Dnd, [device, mode]) => {
            match execute_cli_dnd(&access.feature_control_provider(), device, mode) {
                Ok(FeatureControlOutcome::Dnd {
                    device_id,
                    mode,
                    changed,
                }) => format!(
                    "{device_id}: DND {} ({})\n",
                    dnd_mode_name(mode),
                    if changed { "updated" } else { "unchanged" }
                ),
                Ok(_) => "DND command returned an unexpected result\n".to_owned(),
                Err(error) => format!("DND command failed: {error}\n"),
            }
        }
        (ControlCliCommand::Message, [target, text]) => {
            format_control_cli_outcome(execute_cli_message(&provider, target, text, None, None))
        }
        (ControlCliCommand::Message, [target, text, beep]) => format_control_cli_outcome(
            execute_cli_message(&provider, target, text, Some(beep), None),
        ),
        (ControlCliCommand::Message, [target, text, beep, timeout]) => format_control_cli_outcome(
            execute_cli_message(&provider, target, text, Some(beep), Some(timeout)),
        ),
        (ControlCliCommand::Answer, [call_id]) => {
            format_control_cli_outcome(execute_cli_answer(&provider, call_id, None))
        }
        (ControlCliCommand::Answer, [call_id, device]) => {
            format_control_cli_outcome(execute_cli_answer(&provider, call_id, Some(device)))
        }
        (ControlCliCommand::End, [call_id]) => {
            format_control_cli_outcome(execute_cli_end(&provider, call_id))
        }
        (ControlCliCommand::Originate, [device, destination]) => format_control_cli_outcome(
            execute_cli_originate(&provider, device, destination, None, None),
        ),
        (ControlCliCommand::Originate, [device, destination, line]) => format_control_cli_outcome(
            execute_cli_originate(&provider, device, destination, Some(line), None),
        ),
        (ControlCliCommand::Originate, [device, destination, line, assigned_channel_id]) => {
            format_control_cli_outcome(execute_cli_originate(
                &provider,
                device,
                destination,
                Some(line),
                Some(assigned_channel_id),
            ))
        }
        _ => "Invalid command arguments\n".to_owned(),
    }
}

fn format_control_cli_outcome(result: Result<ControlOutcome, CliControlError>) -> String {
    match result {
        Ok(ControlOutcome::Message {
            attempted,
            delivered,
            persistent,
            ..
        }) => format!(
            "Message delivered to {delivered}/{attempted} device(s){}\n",
            if persistent { " and retained" } else { "" }
        ),
        Ok(ControlOutcome::Answer { device_id, call_id }) => {
            format!("Call {} answered on {device_id}\n", call_id.0)
        }
        Ok(ControlOutcome::End { device_id, call_id }) => {
            format!("Call {} ended on {device_id}\n", call_id.0)
        }
        Ok(ControlOutcome::Originate {
            device_id,
            line,
            call_id,
        }) => format!(
            "Call {} originated on {device_id} using line {line}\n",
            call_id.0
        ),
        Ok(_) => "Control command returned an unexpected result\n".to_owned(),
        Err(error) => format!("Control command failed: {error}\n"),
    }
}

pub fn complete_control_cli(
    command: ControlCliCommand,
    position: usize,
    prefix: &str,
    ordinal: usize,
    context: Option<&str>,
) -> Option<String> {
    let access = module_access()?;
    match (command, position) {
        (ControlCliCommand::Dnd, 2)
        | (ControlCliCommand::Answer, 3)
        | (ControlCliCommand::Originate, 2) => complete_registered_device(&access, prefix, ordinal),
        (ControlCliCommand::Dnd, 3) => complete_cli_value(
            ["off", "reject", "silent"],
            prefix,
            ordinal,
            MAX_DND_MODE_BYTES,
        ),
        (ControlCliCommand::Message, 2) => {
            let mut targets = registered_device_ids(&access.shared)
                .into_iter()
                .map(|device| device.to_string())
                .collect::<Vec<_>>();
            targets.extend(["all".to_owned(), "system".to_owned()]);
            complete_cli_value(targets, prefix, ordinal, MAX_DEVICE_SELECTOR_BYTES)
        }
        (ControlCliCommand::Message, 4) => {
            complete_cli_value(["no", "yes"], prefix, ordinal, MAX_BOOLEAN_BYTES)
        }
        (ControlCliCommand::Answer, 2) => complete_call_id(&access, prefix, ordinal, true),
        (ControlCliCommand::End, 2) => complete_call_id(&access, prefix, ordinal, false),
        (ControlCliCommand::Originate, 4) => {
            let device_id = DeviceId::new(context?).ok()?;
            let config = access.config();
            complete_cli_value(
                config
                    .appearances_for_device(&device_id)
                    .map(|binding| binding.line.number.as_str()),
                prefix,
                ordinal,
                MAX_LINE_SELECTOR_BYTES,
            )
        }
        _ => None,
    }
}

fn complete_registered_device(access: &Access, prefix: &str, ordinal: usize) -> Option<String> {
    let devices = registered_device_ids(&access.shared);
    complete_cli_device(devices.iter(), prefix, ordinal)
}

fn complete_call_id(
    access: &Access,
    prefix: &str,
    ordinal: usize,
    ringing_inbound_only: bool,
) -> Option<String> {
    let call_ids = controller_step(&access.shared.controller, |controller| {
        controller
            .calls()
            .filter(|call| {
                !ringing_inbound_only
                    || (call.direction == CallDirection::Inbound
                        && call.state == CallState::Ringing)
            })
            .map(|call| call.sccp_id.0.to_string())
            .collect::<Vec<_>>()
    });
    complete_cli_value(call_ids, prefix, ordinal, MAX_CALL_ID_BYTES)
}

const fn dnd_mode_name(mode: DndMode) -> &'static str {
    match mode {
        DndMode::Off => "off",
        DndMode::Silent => "silent",
        DndMode::Reject => "reject",
    }
}

pub fn execute_forwarding_cli(fd: c_int, device: &str, line: &str, kind: &str, destination: &str) {
    let Some(access) = module_access() else {
        return;
    };
    let output = match parse_cli_forwarding_mutation(device, line, kind, destination) {
        Ok(FeatureControlMutation::Forwarding {
            device_id,
            line,
            kind,
            destination,
        }) => match execute_forwarding_mutation(&access, device_id, line, kind, destination) {
            Ok(_) => "Forwarding updated\n",
            Err(_) => "Forwarding update failed\n",
        },
        Ok(FeatureControlMutation::Dnd { .. }) => "Forwarding command rejected\n",
        Err(_) => "Invalid forwarding arguments\n",
    };
    raw::system::cli_write(fd, output);
}

pub fn notify_mwi(line: &str, active: bool) {
    let Some(access) = module_access() else {
        return;
    };
    for binding in access.inbound_line_bindings(line) {
        let registered = controller_step(&access.shared.controller, |controller| {
            controller.is_registered(&binding.device_id)
        });
        if registered {
            access.spawn_phone(PhoneCommand::new(
                binding.device_id,
                PhoneCommandAction::SetMwi {
                    line_instance: LineInstance::new(binding.line_instance),
                    enabled: active,
                },
            ));
        }
    }
}
