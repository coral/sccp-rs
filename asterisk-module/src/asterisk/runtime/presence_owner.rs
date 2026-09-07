//! Owns native presence subscriptions on one tracked blocking worker. Callback
//! producers only copy normalized values into a bounded coalescing mailbox.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use sccp_protocol::{BlfSpeedDialDefinition, DeviceId, LineInstance, SessionGeneration};
use tokio::sync::{Notify, mpsc};

use super::{Access, LogLevel, PhoneCommand, PhoneCommandAction, ast_log};
use super::{DeviceState, MwiSubscriptionChange};
use crate::asterisk::MANAGER_CONTROL_TIMEOUT;
use crate::asterisk::adapters::{AsteriskHints, AsteriskRegistrationExtensions};
use crate::asterisk::raw::presence::{NativeMwiSubscription, publish_device_state, subscribe_mwi};
use crate::config::{HintTarget, ModuleConfig};
use crate::pbx::registration::{
    RegistrationContextRegistry, RegistrationExtensionBackendError, RegistrationRegistryError,
    RegistrationRegistryOperation, configured_registration_appearances,
};
use crate::presence::blf::{BlfEvent, BlfSubscriptions};
use crate::runtime::callback_updates::CallbackUpdates;
use crate::runtime::configuration_transaction::ConfigurationLease;
use crate::runtime::mailbox::{
    MailboxReceiver, MailboxSender, RUNTIME_MAILBOX_CAPACITY, WorkPermit, mailbox,
};

pub(super) type BlfPlan = Vec<(BlfSpeedDialDefinition, HintTarget)>;

#[derive(Clone)]
pub(super) struct PresenceHandle {
    commands: MailboxSender<PresenceCommand>,
    retired_contexts: Arc<Notify>,
    finishes: mpsc::Sender<StageFinish>,
}

pub(super) struct PresenceMailbox {
    commands: MailboxReceiver<PresenceCommand>,
    retired_contexts: Arc<Notify>,
    finishes: mpsc::Receiver<StageFinish>,
}

pub(super) enum PresenceCommand {
    InstallBlf {
        device: DeviceId,
        generation: SessionGeneration,
        plan: BlfPlan,
    },
    RemoveBlf {
        device: DeviceId,
        generation: Option<SessionGeneration>,
    },
    RetryBlf(Instant),
    InstallMwi(Vec<(String, String)>),
    ClearMwi,
    PublishLine {
        line: String,
        state: DeviceState,
    },
    RegisterContexts {
        config: Arc<ModuleConfig>,
        registered: Vec<DeviceId>,
        device: DeviceId,
        lease: ConfigurationLease,
        response: std::sync::mpsc::SyncSender<Result<(), RegistrationRegistryError>>,
    },
    StageRegistrations {
        config: Arc<ModuleConfig>,
        registered: Vec<DeviceId>,
        previous: Arc<ModuleConfig>,
        previous_registered: Vec<DeviceId>,
        lease: ConfigurationLease,
        finish: mpsc::OwnedPermit<StageFinish>,
        response: std::sync::mpsc::SyncSender<
            Result<StagedRegistrationContexts, RegistrationRegistryError>,
        >,
    },
    StageMwi {
        changes: Vec<MwiSubscriptionChange>,
        finish: mpsc::OwnedPermit<StageFinish>,
        response: std::sync::mpsc::SyncSender<Result<StagedMwiSubscriptions, String>>,
    },
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub(super) struct StageId(u64);

pub(super) enum StageFinish {
    Mwi {
        id: StageId,
        removed: Option<Vec<String>>,
        response: Option<std::sync::mpsc::SyncSender<()>>,
    },
    Registrations {
        id: StageId,
        affected: Option<HashSet<DeviceId>>,
        response: Option<std::sync::mpsc::SyncSender<Result<(), RegistrationRegistryError>>>,
    },
}

/// Retains guaranteed completion capacity before staging starts. Drop only sends
/// cancellation; native unsubscription remains the blocking owner's responsibility.
pub struct StagedMwiSubscriptions {
    id: StageId,
    finish: Option<mpsc::OwnedPermit<StageFinish>>,
}

impl StagedMwiSubscriptions {
    pub fn new(access: &Access, changes: &[MwiSubscriptionChange]) -> Result<Self, String> {
        let presence = &access.shared.presence;
        let finish = presence
            .finishes
            .clone()
            .try_reserve_owned()
            .map_err(|_| "presence transaction completion capacity unavailable".to_owned())?;
        let (response, reply) = std::sync::mpsc::sync_channel(1);
        presence
            .commands
            .try_send(
                PresenceCommand::StageMwi {
                    changes: changes.to_vec(),
                    finish,
                    response,
                },
                Some(Instant::now() + MANAGER_CONTROL_TIMEOUT),
            )
            .map_err(|error| error.to_string())?;
        reply
            .recv_timeout(MANAGER_CONTROL_TIMEOUT)
            .map_err(|_| "presence transaction result unavailable".to_owned())?
    }

    pub fn commit(mut self, _access: &Access, removed: &[MwiSubscriptionChange]) {
        if let Some(finish) = self.finish.take() {
            let (response, reply) = std::sync::mpsc::sync_channel(1);
            finish.send(StageFinish::Mwi {
                id: self.id,
                removed: Some(removed.iter().map(|change| change.line.clone()).collect()),
                response: Some(response),
            });
            if reply.recv().is_err() {
                ast_log(
                    LogLevel::Error,
                    "presence owner stopped before MWI commit completed",
                );
            }
        }
    }
}

impl Drop for StagedMwiSubscriptions {
    fn drop(&mut self) {
        if let Some(finish) = self.finish.take() {
            finish.send(StageFinish::Mwi {
                id: self.id,
                removed: None,
                response: None,
            });
        }
    }
}

pub(super) fn presence_channel() -> (PresenceHandle, PresenceMailbox) {
    let (commands, receiver) = mailbox(RUNTIME_MAILBOX_CAPACITY);
    let (finishes, finish_receiver) = mpsc::channel(RUNTIME_MAILBOX_CAPACITY);
    let retired_contexts = Arc::new(Notify::new());
    (
        PresenceHandle {
            commands,
            finishes,
            retired_contexts: Arc::clone(&retired_contexts),
        },
        PresenceMailbox {
            commands: receiver,
            finishes: finish_receiver,
            retired_contexts,
        },
    )
}

impl PresenceHandle {
    pub(super) fn retire_registration_contexts(&self) {
        self.retired_contexts.notify_one();
    }

    pub(super) fn snapshot(&self) -> crate::runtime::mailbox::QueueSnapshot {
        self.commands.snapshot()
    }

    pub(super) fn register_contexts(
        &self,
        config: Arc<ModuleConfig>,
        registered: Vec<DeviceId>,
        device: DeviceId,
        lease: &ConfigurationLease,
    ) -> Result<(), RegistrationRegistryError> {
        let (response, result) = std::sync::mpsc::sync_channel(1);
        self.commands
            .try_send(
                PresenceCommand::RegisterContexts {
                    config,
                    registered,
                    device,
                    lease: lease.clone(),
                    response,
                },
                Some(Instant::now() + MANAGER_CONTROL_TIMEOUT),
            )
            .map_err(|_| registration_unavailable())?;
        result.recv().map_err(|_| registration_unavailable())?
    }

    pub(super) fn enqueue(&self, command: PresenceCommand) {
        if let Err(error) = self.commands.try_send(command, None) {
            ast_log(
                LogLevel::Warning,
                &format!("presence admission failed: {error}"),
            );
        }
    }

    pub(super) fn close(&self) {
        self.commands.close();
    }
}

#[derive(Clone, Eq, Hash, PartialEq)]
enum UpdateKey {
    Blf(DeviceId, u32),
    Mwi(String, u64),
}

enum PresenceUpdate {
    Blf(BlfEvent),
    Mwi {
        line: String,
        generation: u64,
        active: bool,
    },
}

struct MwiEntry {
    generation: u64,
    latest: Option<bool>,
    _subscription: NativeMwiSubscription,
}

struct RegistrationContexts {
    registry: RegistrationContextRegistry<AsteriskRegistrationExtensions>,
    suppressed: HashSet<DeviceId>,
    config: Option<Arc<ModuleConfig>>,
    registered: Vec<DeviceId>,
}

impl RegistrationContexts {
    fn reconcile(
        &mut self,
        config: &Arc<ModuleConfig>,
        registered: &[DeviceId],
    ) -> Result<(), RegistrationRegistryError> {
        let published = registered
            .iter()
            .filter(|device| !self.suppressed.contains(*device));
        self.registry
            .reconcile(configured_registration_appearances(config, published))?;
        self.config = Some(Arc::clone(config));
        self.registered = registered.to_vec();
        Ok(())
    }

    fn retire_disconnected(&mut self, access: &Access) -> Result<(), RegistrationRegistryError> {
        let Some(config) = self.config.clone() else {
            return Ok(());
        };
        let controller = access.shared.controller.snapshot();
        let registered = self
            .registered
            .iter()
            .filter(|device| controller.is_registered(device))
            .cloned()
            .collect::<Vec<_>>();
        self.reconcile(&config, &registered)
    }
}

struct PendingRegistrationContexts {
    previous: Arc<ModuleConfig>,
    registered: Vec<DeviceId>,
    _lease: ConfigurationLease,
    _permit: WorkPermit,
}

pub struct StagedRegistrationContexts {
    id: StageId,
    finish: Option<mpsc::OwnedPermit<StageFinish>>,
}

impl StagedRegistrationContexts {
    pub fn new(
        access: &Access,
        config: Arc<ModuleConfig>,
        registered: Vec<DeviceId>,
        previous: Arc<ModuleConfig>,
        previous_registered: Vec<DeviceId>,
        lease: &ConfigurationLease,
    ) -> Result<Self, RegistrationRegistryError> {
        let finish = access
            .shared
            .presence
            .finishes
            .clone()
            .try_reserve_owned()
            .map_err(|_| registration_unavailable())?;
        let (response, result) = std::sync::mpsc::sync_channel(1);
        access
            .shared
            .presence
            .commands
            .try_send(
                PresenceCommand::StageRegistrations {
                    config,
                    registered,
                    previous,
                    previous_registered,
                    lease: lease.clone(),
                    finish,
                    response,
                },
                Some(Instant::now() + MANAGER_CONTROL_TIMEOUT),
            )
            .map_err(|_| registration_unavailable())?;
        result
            .recv_timeout(MANAGER_CONTROL_TIMEOUT)
            .map_err(|_| registration_unavailable())?
    }

    pub fn commit(mut self, affected: HashSet<DeviceId>) -> Result<(), RegistrationRegistryError> {
        self.complete(Some(affected))
    }
    pub fn abort(mut self) -> Result<(), RegistrationRegistryError> {
        self.complete(None)
    }

    fn complete(
        &mut self,
        affected: Option<HashSet<DeviceId>>,
    ) -> Result<(), RegistrationRegistryError> {
        let Some(finish) = self.finish.take() else {
            return Ok(());
        };
        let (response, result) = std::sync::mpsc::sync_channel(1);
        finish.send(StageFinish::Registrations {
            id: self.id,
            affected,
            response: Some(response),
        });
        result.recv().map_err(|_| registration_unavailable())?
    }
}

impl Drop for StagedRegistrationContexts {
    fn drop(&mut self) {
        if let Some(finish) = self.finish.take() {
            finish.send(StageFinish::Registrations {
                id: self.id,
                affected: None,
                response: None,
            });
        }
    }
}

fn registration_unavailable() -> RegistrationRegistryError {
    RegistrationRegistryError::Backend {
        operation: RegistrationRegistryOperation::Publish,
        source: RegistrationExtensionBackendError::Unavailable,
    }
}

pub(super) struct PresenceOwner {
    access: Access,
    contexts: RegistrationContexts,
    staged_contexts: HashMap<StageId, PendingRegistrationContexts>,
    mailbox: PresenceMailbox,
    updates: Arc<CallbackUpdates<UpdateKey, PresenceUpdate>>,
    blf: BlfSubscriptions<AsteriskHints>,
    plans: HashMap<DeviceId, (SessionGeneration, BlfPlan)>,
    mwi: HashMap<String, MwiEntry>,
    staged: HashMap<StageId, (HashMap<String, MwiEntry>, WorkPermit)>,
    published: HashMap<String, DeviceState>,
    next_stage: u64,
    next_mwi_generation: u64,
}

impl PresenceOwner {
    pub(super) fn new(access: Access, mailbox: PresenceMailbox) -> Self {
        let updates = Arc::new(CallbackUpdates::new(RUNTIME_MAILBOX_CAPACITY));
        let sink = Arc::clone(&updates);
        let blf = BlfSubscriptions::with_sink(
            AsteriskHints::new(),
            Arc::new(move |event| {
                let key = UpdateKey::Blf(event.device_id.clone(), event.instance);
                let generation = event.generation();
                let terminal = event.is_terminal();
                let _ = sink.push(key, generation, PresenceUpdate::Blf(event), terminal);
            }),
        );
        Self {
            access,
            contexts: RegistrationContexts {
                registry: RegistrationContextRegistry::new(AsteriskRegistrationExtensions::new()),
                suppressed: HashSet::new(),
                config: None,
                registered: Vec::new(),
            },
            staged_contexts: HashMap::new(),
            mailbox,
            updates,
            blf,
            plans: HashMap::new(),
            mwi: HashMap::new(),
            staged: HashMap::new(),
            published: HashMap::new(),
            next_stage: 1,
            next_mwi_generation: 1,
        }
    }

    pub(super) fn run(mut self) {
        enum Turn {
            Command(Option<crate::runtime::mailbox::Admitted<PresenceCommand>>),
            Finish(StageFinish),
            Updates,
            RetiredContexts,
        }
        let mut retired_contexts = false;
        let mut commands_open = true;
        loop {
            if !commands_open && self.staged.is_empty() && self.staged_contexts.is_empty() {
                break;
            }
            let turn = self.access.handle.block_on(async {
                tokio::select! {
                    biased;
                    Some(finish) = self.mailbox.finishes.recv() => Turn::Finish(finish),
                    _ = self.mailbox.retired_contexts.notified() => Turn::RetiredContexts,
                    command = self.mailbox.commands.recv(), if commands_open => Turn::Command(command),
                    _ = self.updates.notified() => Turn::Updates,
                }
            });
            match turn {
                Turn::Command(None) => commands_open = false,
                Turn::Command(Some(command)) => {
                    if command.is_expired(Instant::now()) {
                        command.record_expiration();
                        continue;
                    }
                    let (command, permit) = command.into_parts();
                    self.execute(command, permit);
                }
                Turn::Finish(finish) => self.finish(finish),
                Turn::Updates => {}
                Turn::RetiredContexts => retired_contexts = true,
            }
            // Retirement can bypass a stalled configuration transaction without
            // overwriting its provisional dialplan: native cleanup only removes
            // owners from the last reconciled configuration, after commit/abort.
            if retired_contexts {
                self.retire_blf_sessions();
            }
            if retired_contexts && self.staged_contexts.is_empty() {
                retired_contexts = false;
                if let Err(error) = self.contexts.retire_disconnected(&self.access) {
                    ast_log(
                        LogLevel::Error,
                        &format!("unable to retire registration-context extensions: {error}"),
                    );
                }
            }
            // One key per turn keeps continuous callback traffic from starving commands.
            if let Some((first, latest)) = self.updates.pop() {
                self.update(first);
                if let Some(latest) = latest {
                    self.update(latest);
                }
            }
        }
        // These drops unsubscribe and join native callbacks on this worker.
        self.blf.clear();
        self.mwi.clear();
        self.staged.clear();
        self.staged_contexts.clear();
        if let Err(error) = self.contexts.registry.clear() {
            ast_log(
                LogLevel::Error,
                &format!("unable to remove registration-context extensions during unload: {error}"),
            );
        }
    }

    fn execute(&mut self, command: PresenceCommand, permit: WorkPermit) {
        match command {
            PresenceCommand::InstallBlf {
                device,
                generation,
                plan,
            } => {
                if !self
                    .access
                    .shared
                    .controller
                    .snapshot()
                    .session_is_current(&device, generation)
                {
                    return;
                }
                self.blf.remove_device(&device);
                self.updates
                    .discard(|key| matches!(key, UpdateKey::Blf(retired, _) if retired == &device));
                for (definition, target) in &plan {
                    if self.subscription_count() >= RUNTIME_MAILBOX_CAPACITY {
                        ast_log(
                            LogLevel::Warning,
                            "presence subscription capacity exhausted",
                        );
                        self.blf.defer(device.clone(), definition.instance);
                        continue;
                    }
                    if let Err(error) = self.blf.subscribe(device.clone(), definition, target) {
                        ast_log(
                            LogLevel::Warning,
                            &format!(
                                "unable to subscribe BLF button {} for {device}: {error}",
                                definition.instance
                            ),
                        );
                    }
                }
                self.plans.insert(device, (generation, plan));
            }
            PresenceCommand::RemoveBlf { device, generation } => {
                if generation.is_some_and(|expected| {
                    self.plans
                        .get(&device)
                        .is_none_or(|(current, _)| *current != expected)
                }) {
                    return;
                }
                self.blf.remove_device(&device);
                self.updates
                    .discard(|key| matches!(key, UpdateKey::Blf(retired, _) if retired == &device));
                self.plans.remove(&device);
            }
            PresenceCommand::RetryBlf(now) => {
                let mut remaining =
                    RUNTIME_MAILBOX_CAPACITY.saturating_sub(self.subscription_count());
                let controller = self.access.shared.controller.snapshot();
                for (device, (generation, plan)) in &self.plans {
                    if !controller.session_is_current(device, *generation) {
                        continue;
                    }
                    for (definition, target) in plan {
                        if remaining == 0 {
                            break;
                        }
                        if self.blf.retry_due(device, definition.instance, now) {
                            remaining -= 1;
                        }
                        if self.blf.retry_due(device, definition.instance, now)
                            && let Err(error) =
                                self.blf.subscribe(device.clone(), definition, target)
                        {
                            ast_log(
                                LogLevel::Warning,
                                &format!(
                                    "unable to retry BLF button {} for {device}: {error}",
                                    definition.instance
                                ),
                            );
                        }
                    }
                }
            }
            PresenceCommand::InstallMwi(subscriptions) => {
                if self
                    .subscription_count()
                    .saturating_add(subscriptions.len())
                    > RUNTIME_MAILBOX_CAPACITY
                {
                    ast_log(
                        LogLevel::Warning,
                        "presence subscription capacity exhausted",
                    );
                    return;
                }
                let mut installed = HashMap::new();
                for (line, mailbox) in subscriptions {
                    match self.subscribe_mwi(line.clone(), mailbox) {
                        Ok(entry) => {
                            installed.insert(line, entry);
                        }
                        Err(error) => ast_log(LogLevel::Warning, &error),
                    }
                }
                let retired = self
                    .mwi
                    .iter()
                    .map(|(line, entry)| (line.clone(), entry.generation))
                    .collect::<Vec<_>>();
                self.mwi = installed;
                for (line, generation) in retired {
                    self.updates.discard(|key| matches!(key, UpdateKey::Mwi(old_line, old_generation) if old_line == &line && *old_generation == generation));
                }
            }
            PresenceCommand::ClearMwi => {
                let retired = self
                    .mwi
                    .iter()
                    .map(|(line, entry)| (line.clone(), entry.generation))
                    .collect::<Vec<_>>();
                self.mwi.clear();
                for (line, generation) in retired {
                    self.discard_mwi(&line, generation);
                }
            }
            PresenceCommand::PublishLine { line, state } => {
                let config = self.access.config();
                self.published
                    .retain(|line, _| config.lines.contains_key(line));
                if self.published.get(&line) != Some(&state) {
                    self.published.insert(line.clone(), state);
                    publish_device_state(&line, state);
                }
            }
            PresenceCommand::RegisterContexts {
                config,
                registered,
                device,
                lease,
                response,
            } => {
                let _lease = lease;
                let current = self.access.shared.controller.snapshot();
                let registered = registered
                    .into_iter()
                    .filter(|device| current.is_registered(device))
                    .collect::<Vec<_>>();
                if current.is_registered(&device) {
                    self.contexts.suppressed.remove(&device);
                }
                let result = self.contexts.reconcile(&config, &registered);
                if result.is_err() {
                    self.contexts.suppressed.insert(device);
                }
                let _ = response.send(result);
            }
            PresenceCommand::StageRegistrations {
                config,
                registered,
                previous,
                previous_registered,
                lease,
                finish,
                response,
            } => {
                let Some(next) = self.next_stage.checked_add(1) else {
                    let _ = response.send(Err(registration_unavailable()));
                    return;
                };
                let id = StageId(self.next_stage);
                self.next_stage = next;
                let current = self.access.shared.controller.snapshot();
                let registered = registered
                    .into_iter()
                    .filter(|device| current.is_registered(device))
                    .collect::<Vec<_>>();
                if let Err(error) = self.contexts.reconcile(&config, &registered) {
                    let _ = response.send(Err(error));
                    return;
                }
                self.staged_contexts.insert(
                    id,
                    PendingRegistrationContexts {
                        previous,
                        registered: previous_registered,
                        _lease: lease,
                        _permit: permit,
                    },
                );
                let _ = response.send(Ok(StagedRegistrationContexts {
                    id,
                    finish: Some(finish),
                }));
            }
            PresenceCommand::StageMwi {
                changes,
                finish,
                response,
            } => {
                if self.subscription_count().saturating_add(changes.len())
                    > RUNTIME_MAILBOX_CAPACITY
                {
                    let _ =
                        response.send(Err("presence subscription capacity exhausted".to_owned()));
                    return;
                }
                let Some(next_stage) = self.next_stage.checked_add(1) else {
                    let _ =
                        response.send(Err("presence transaction identifiers exhausted".to_owned()));
                    return;
                };
                let id = StageId(self.next_stage);
                self.next_stage = next_stage;
                let mut staged = HashMap::new();
                for change in changes {
                    match self.subscribe_mwi(change.line.clone(), change.mailbox) {
                        Ok(entry) => {
                            staged.insert(change.line, entry);
                        }
                        Err(error) => {
                            self.retire_mwi_entries(staged);
                            let _ = response.send(Err(error));
                            return;
                        }
                    }
                }
                self.staged.insert(id, (staged, permit));
                let _ = response.send(Ok(StagedMwiSubscriptions {
                    id,
                    finish: Some(finish),
                }));
            }
        }
    }

    fn retire_blf_sessions(&mut self) {
        let controller = self.access.shared.controller.snapshot();
        let retired = self
            .plans
            .iter()
            .filter(|(device, (generation, _))| !controller.session_is_current(device, *generation))
            .map(|(device, _)| device.clone())
            .collect::<Vec<_>>();
        for device in retired {
            self.blf.remove_device(&device);
            self.plans.remove(&device);
            self.updates
                .discard(|key| matches!(key, UpdateKey::Blf(retired, _) if retired == &device));
        }
    }

    fn subscription_count(&self) -> usize {
        self.blf.len()
            + self.mwi.len()
            + self
                .staged
                .values()
                .map(|(entries, _)| entries.len())
                .sum::<usize>()
    }

    fn discard_mwi(&self, line: &str, generation: u64) {
        self.updates.discard(|key| matches!(key, UpdateKey::Mwi(old_line, old_generation) if old_line == line && *old_generation == generation));
    }

    fn subscribe_mwi(&mut self, line: String, mailbox: String) -> Result<MwiEntry, String> {
        let generation = self.next_mwi_generation;
        self.next_mwi_generation = generation
            .checked_add(1)
            .ok_or_else(|| "MWI subscription generations exhausted".to_owned())?;
        let updates = Arc::clone(&self.updates);
        let key = UpdateKey::Mwi(line.clone(), generation);
        let callback_line = line.clone();
        let subscription = subscribe_mwi(
            mailbox,
            Arc::new(move |active| {
                let _ = updates.push(
                    key.clone(),
                    generation,
                    PresenceUpdate::Mwi {
                        line: callback_line.clone(),
                        generation,
                        active,
                    },
                    false,
                );
            }),
        )
        .map_err(|error| {
            self.discard_mwi(&line, generation);
            format!("unable to subscribe MWI for SCCP/{line}: {error}")
        })?;
        Ok(MwiEntry {
            generation,
            latest: None,
            _subscription: subscription,
        })
    }

    fn retire_mwi_entries(&self, entries: HashMap<String, MwiEntry>) {
        for (line, entry) in entries {
            let generation = entry.generation;
            drop(entry);
            self.discard_mwi(&line, generation);
        }
    }

    fn finish(&mut self, finish: StageFinish) {
        let StageFinish::Mwi {
            id,
            removed,
            response,
        } = finish
        else {
            self.finish_registrations(finish);
            return;
        };
        if let Some((staged, _permit)) = self.staged.remove(&id) {
            match removed {
                Some(removed) => {
                    for line in removed {
                        if let Some(entry) = self.mwi.remove(&line) {
                            let generation = entry.generation;
                            drop(entry);
                            self.discard_mwi(&line, generation);
                        }
                    }
                    let initial = staged
                        .iter()
                        .filter_map(|(line, entry)| {
                            entry.latest.map(|active| (line.clone(), active))
                        })
                        .collect::<Vec<_>>();
                    for (line, entry) in staged {
                        if let Some(old) = self.mwi.insert(line.clone(), entry) {
                            let generation = old.generation;
                            drop(old);
                            self.discard_mwi(&line, generation);
                        }
                    }
                    for (line, active) in initial {
                        crate::asterisk::exports::notify_mwi(&line, active);
                    }
                }
                None => self.retire_mwi_entries(staged),
            }
        }
        if let Some(response) = response {
            let _ = response.send(());
        }
    }

    fn finish_registrations(&mut self, finish: StageFinish) {
        let StageFinish::Registrations {
            id,
            affected,
            response,
        } = finish
        else {
            return;
        };
        let result = match self.staged_contexts.remove(&id) {
            Some(pending) => match affected {
                Some(affected) => {
                    self.contexts.suppressed.extend(affected);
                    if let Some(config) = &self.contexts.config {
                        self.contexts
                            .suppressed
                            .retain(|device| config.devices.contains_key(device));
                    }
                    Ok(())
                }
                None => {
                    let current = self.access.shared.controller.snapshot();
                    let registered = pending
                        .registered
                        .into_iter()
                        .filter(|device| current.is_registered(device))
                        .collect::<Vec<_>>();
                    self.contexts.reconcile(&pending.previous, &registered)
                }
            },
            None => Err(registration_unavailable()),
        };
        if let Err(error) = result {
            ast_log(
                LogLevel::Error,
                &format!("registration-context completion failed: {error}"),
            );
        }
        if let Some(response) = response {
            let _ = response.send(result);
        }
    }

    fn update(&mut self, update: PresenceUpdate) {
        match update {
            PresenceUpdate::Blf(event) => {
                if !self.blf.is_current(&event)
                    || !self
                        .plans
                        .get(&event.device_id)
                        .is_some_and(|(generation, _)| {
                            self.access
                                .shared
                                .controller
                                .snapshot()
                                .session_is_current(&event.device_id, *generation)
                        })
                {
                    return;
                }
                self.blf.retry_terminal(&event);
                if event.is_terminal() {
                    self.updates.discard(|key| matches!(key, UpdateKey::Blf(device, instance) if device == &event.device_id && *instance == event.instance));
                }
                self.access.spawn_phone(PhoneCommand::new(
                    event.device_id,
                    PhoneCommandAction::SetBlfStatus {
                        instance: LineInstance::new(event.instance),
                        state: event.state,
                        caller: event.caller,
                    },
                ));
            }
            PresenceUpdate::Mwi {
                line,
                generation,
                active,
            } => {
                if self
                    .mwi
                    .get(&line)
                    .is_some_and(|entry| entry.generation == generation)
                {
                    crate::asterisk::exports::notify_mwi(&line, active);
                } else {
                    for (staged, _) in self.staged.values_mut() {
                        if let Some(entry) = staged.get_mut(&line)
                            && entry.generation == generation
                        {
                            entry.latest = Some(active);
                            break;
                        }
                    }
                }
            }
        }
    }
}
