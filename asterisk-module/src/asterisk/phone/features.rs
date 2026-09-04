//! Device feature, forwarding, DND, and voicemail interactions.

use super::{
    Access, AsteriskBackend, ButtonDefinition, ButtonType, CallId, DeviceFeatureState, DeviceId,
    Digit, DndButtonMode, DndMode, DndMutation, DriverEffect, Duration, FeatureChange,
    FeatureControlProviderError, FeatureStoreError, ForwardingCommit, ForwardingDestination,
    ForwardingDigitOutcome, ForwardingEntryTiming, ForwardingExpiryOutcome, ForwardingKind,
    ForwardingRejection, ForwardingWriteOutcome, Instant, LineInstance, LogLevel,
    MANAGER_CONTROL_DELIVERY_TIMEOUT, ManagementEvent, ModuleConfig, MutexExt as _, PbxAudioFormat,
    PbxEffect, PhoneCommand, PhoneCommandAction, PhoneDndButtonMode, PhoneDndMode,
    RuntimeRecordings, SoftKey, VoicemailNativeOutcome, VoicemailPlan, VoicemailTarget, ast_log,
    configure_pickup_policy, configured_feature_state, controller_step, default_button_mode,
    dial_terminator_digit, execute_effects, execute_forwarding_mutation, feature_changes,
    feature_event, forwarding_ui_line_instances, handset_status_message, preferred_codec,
    publish_device_lines, publish_line, publish_recording_button_state, toggle_monitor_recording,
    with_channel,
};
use crate::runtime::backend::SupplementaryBackend as _;

pub fn update_device_features_locked(
    access: &Access,
    config: &ModuleConfig,
    device_id: &DeviceId,
    mutation: impl FnOnce(&mut DeviceFeatureState),
) -> Result<Option<(DeviceFeatureState, DeviceFeatureState)>, FeatureStoreError> {
    let Some(defaults) = configured_feature_state(config, device_id) else {
        return Ok(None);
    };
    let current = controller_step(&access.shared.controller, |controller| {
        controller
            .feature_state(device_id)
            .cloned()
            .unwrap_or_else(|| defaults.clone())
    });
    let next = access
        .shared
        .feature_store
        .update(device_id, &current, &defaults, mutation)?;
    controller_step(&access.shared.controller, |controller| {
        controller.set_feature_state(device_id, next.clone())
    });
    Ok(Some((current, next)))
}

pub fn publish_device_features(access: &Access, device_id: &DeviceId, state: &DeviceFeatureState) {
    let config = access.config();
    let line_instances = forwarding_ui_line_instances(
        None,
        config
            .appearances_for_device(device_id)
            .map(|binding| (binding.line.number.as_str(), binding.line_instance)),
    )
    .unwrap_or_default();
    for line_instance in line_instances {
        access.spawn_phone(PhoneCommand::new(
            device_id.clone(),
            PhoneCommandAction::SetForwardStatus {
                line_instance: LineInstance::new(line_instance),
                forward_all: state
                    .forwarding
                    .all
                    .clone()
                    .map(ForwardingDestination::into_string),
                forward_busy: state
                    .forwarding
                    .busy
                    .clone()
                    .map(ForwardingDestination::into_string),
                forward_no_answer: state
                    .forwarding
                    .no_answer
                    .clone()
                    .map(ForwardingDestination::into_string),
            },
        ));
    }
    let Some(device) = config.devices.get(device_id) else {
        return;
    };
    access.spawn_phone(PhoneCommand::new(
        device_id.clone(),
        PhoneCommandAction::SetStatusMessage {
            message: handset_status_message(state.dnd),
            beep: false,
        },
    ));
    for (instance, button_mode) in config.dnd_buttons_for_device(device_id) {
        access.spawn_phone(PhoneCommand::new(
            device_id.clone(),
            PhoneCommandAction::SetDoNotDisturbStatus {
                instance: LineInstance::new(instance),
                mode: phone_dnd_mode(state.dnd),
                button_mode: phone_dnd_button_mode(button_mode),
            },
        ));
    }
    for button in &device.buttons {
        let ButtonDefinition::Feature(feature) = button else {
            continue;
        };
        if feature.feature == ButtonType::DoNotDisturb {
            continue;
        }
        let enabled = match feature.feature {
            ButtonType::ForwardAll => state.forwarding.all.is_some(),
            ButtonType::ForwardBusy => state.forwarding.busy.is_some(),
            ButtonType::ForwardNoAnswer => state.forwarding.no_answer.is_some(),
            ButtonType::ParkingLot => config
                .parking_lot_for_button(device_id, feature.instance)
                .is_some_and(|button| {
                    access
                        .shared
                        .parking_registry
                        .lock_unpoisoned()
                        .lot_has_calls(&button.lot)
                }),
            _ => state
                .buttons
                .get(&feature.instance)
                .copied()
                .unwrap_or(false),
        };
        access.spawn_phone(PhoneCommand::new(
            device_id.clone(),
            PhoneCommandAction::SetFeatureStatus {
                instance: LineInstance::new(feature.instance),
                enabled,
            },
        ));
    }
}

pub(super) const fn phone_dnd_mode(mode: DndMode) -> PhoneDndMode {
    match mode {
        DndMode::Off => PhoneDndMode::Off,
        DndMode::Silent => PhoneDndMode::Silent,
        DndMode::Reject => PhoneDndMode::Reject,
    }
}

pub(super) const fn phone_dnd_button_mode(mode: DndButtonMode) -> PhoneDndButtonMode {
    match mode {
        DndButtonMode::Cycle => PhoneDndButtonMode::Cycle,
        DndButtonMode::Silent => PhoneDndButtonMode::Silent,
        DndButtonMode::Reject => PhoneDndButtonMode::Reject,
    }
}

pub fn publish_ami_event(access: &Access, event: &ManagementEvent) {
    if let Err(error) = access.shared.ami_events.publish(event) {
        ast_log(
            LogLevel::Warning,
            &format!("unable to publish a management event: {error}"),
        );
    }
}

pub fn publish_feature_changes(
    access: &Access,
    device_id: &DeviceId,
    previous: &DeviceFeatureState,
    current: &DeviceFeatureState,
) {
    for change in feature_changes(previous, current) {
        publish_ami_event(access, &feature_event(device_id, change));
    }
}

pub async fn expire_forwarding_entries(access: &Access, now: Instant) {
    let expired = access
        .shared
        .forwarding_entries
        .lock_unpoisoned()
        .claim_expired(now);
    for outcome in expired {
        match outcome {
            ForwardingExpiryOutcome::Cancel(entry) => {
                if send_confirmed_forwarding(
                    access,
                    PhoneCommand::new(
                        entry.device_id,
                        PhoneCommandAction::CloseCall {
                            call_id: entry.call_id,
                        },
                    ),
                )
                .await
                    == ForwardingWriteOutcome::Failed
                {
                    ast_log(
                        LogLevel::Warning,
                        "unable to close expired forwarding collection on the handset",
                    );
                }
            }
            ForwardingExpiryOutcome::Commit(commit) => {
                finish_forwarding_commit(access, commit).await;
            }
        }
    }
}

pub enum RuntimeDndMutation {
    Set(DndMode),
    Scheduled(DndMode),
    SoftKey,
    Button(u32),
}

pub enum RuntimeDndMutationError {
    Unavailable,
    DeviceNotFound,
    FeatureDisabled,
    ButtonNotFound,
    Store(FeatureStoreError),
}

pub struct RuntimeDndMutationOutcome {
    pub previous: DeviceFeatureState,
    pub current: DeviceFeatureState,
}

pub fn execute_dnd_mutation(
    access: &Access,
    device_id: &DeviceId,
    request: RuntimeDndMutation,
) -> Result<RuntimeDndMutationOutcome, RuntimeDndMutationError> {
    let _schedule_guard = access
        .shared
        .dnd_schedule_mutations
        .lock()
        .map_err(|_| RuntimeDndMutationError::Unavailable)?;
    execute_dnd_mutation_serialized(access, device_id, request)
}

pub fn execute_dnd_mutation_serialized(
    access: &Access,
    device_id: &DeviceId,
    request: RuntimeDndMutation,
) -> Result<RuntimeDndMutationOutcome, RuntimeDndMutationError> {
    let _feature_guard = access
        .shared
        .feature_mutations
        .lock()
        .map_err(|_| RuntimeDndMutationError::Unavailable)?;
    let config = access.config();
    let device = config
        .devices
        .get(device_id)
        .ok_or(RuntimeDndMutationError::DeviceNotFound)?;
    if !device.feature_defaults.dnd_enabled && !matches!(request, RuntimeDndMutation::Scheduled(_))
    {
        return Err(RuntimeDndMutationError::FeatureDisabled);
    }
    let defaults = configured_feature_state(&config, device_id)
        .ok_or(RuntimeDndMutationError::DeviceNotFound)?;
    let mutation = match request {
        RuntimeDndMutation::Set(mode) | RuntimeDndMutation::Scheduled(mode) => {
            DndMutation::Set(mode)
        }
        RuntimeDndMutation::SoftKey => DndMutation::Toggle(default_button_mode(defaults.dnd)),
        RuntimeDndMutation::Button(instance) => DndMutation::Toggle(
            config
                .dnd_button_mode(device_id, instance)
                .ok_or(RuntimeDndMutationError::ButtonNotFound)?,
        ),
    };
    let result = update_device_features_locked(access, &config, device_id, |state| {
        state.dnd = mutation.apply(state.dnd);
    });
    let (previous, current) = match result {
        Ok(Some(states)) => states,
        Ok(None) => return Err(RuntimeDndMutationError::DeviceNotFound),
        Err(error) => {
            publish_current_device_features(access, device_id);
            return Err(RuntimeDndMutationError::Store(error));
        }
    };
    publish_device_features(access, device_id, &current);
    publish_device_lines(access, device_id);
    publish_feature_changes(access, device_id, &previous, &current);
    Ok(RuntimeDndMutationOutcome { previous, current })
}

pub(super) async fn handle_feature_soft_key(
    access: &Access,
    device_id: DeviceId,
    call_id: Option<CallId>,
    line_instance: u32,
    soft_key: SoftKey,
) {
    if matches!(
        soft_key,
        SoftKey::ForwardAll | SoftKey::ForwardBusy | SoftKey::ForwardNoAnswer
    ) {
        handle_forwarding_soft_key(access, device_id, line_instance, soft_key).await;
        return;
    }
    if soft_key == SoftKey::Private
        && let Some(call_id) = call_id
    {
        let change = controller_step(&access.shared.controller, |controller| {
            let call = controller.call(call_id)?;
            let previous = controller.call_privacy(call_id)?;
            let enabled = !previous;
            controller
                .set_call_privacy(call_id, enabled)
                .then_some((call, previous, enabled))
        });
        if let Some((call, previous, enabled)) = change {
            let binding = access.line_binding(&call.device_id, call.line_instance);
            let applied = binding.is_some_and(|binding| {
                with_channel(access, call.pbx_id, |channel| {
                    configure_pickup_policy(access, &binding, channel, enabled)
                })
                .is_some_and(|result| result.is_ok())
            });
            if !applied {
                controller_step(&access.shared.controller, |controller| {
                    controller.set_call_privacy(call_id, previous)
                });
            }
            let _ = access
                .phone
                .send(PhoneCommand::new(
                    device_id.clone(),
                    PhoneCommandAction::DisplayPrompt {
                        call_id,
                        timeout_seconds: 3,
                        text: if !applied {
                            "Unable to change privacy".into()
                        } else if enabled {
                            "Privacy enabled".into()
                        } else {
                            "Privacy disabled".into()
                        },
                    },
                ))
                .await;
            if applied {
                publish_ami_event(
                    access,
                    &feature_event(&device_id, FeatureChange::CallPrivacy { call_id, enabled }),
                );
            }
        }
        return;
    }

    if soft_key == SoftKey::DoNotDisturb {
        if let Err(RuntimeDndMutationError::Store(error)) =
            execute_dnd_mutation(access, &device_id, RuntimeDndMutation::SoftKey)
        {
            log_feature_store_error("persist handset DND mutation", Some(&device_id), &error);
        }
        return;
    }

    let _feature_guard = access.shared.feature_mutations.lock_unpoisoned();
    let config = access.config();
    let Some(device) = config.devices.get(&device_id) else {
        return;
    };
    let defaults = device.feature_defaults.clone();
    let permitted = match soft_key {
        SoftKey::Private => defaults.privacy_enabled,
        SoftKey::ForwardAll => defaults.forwarding.all_enabled,
        SoftKey::ForwardBusy => defaults.forwarding.busy_enabled,
        SoftKey::ForwardNoAnswer => defaults.forwarding.no_answer_enabled,
        _ => false,
    };
    if !permitted {
        return;
    }
    let mutation = move |state: &mut DeviceFeatureState| match soft_key {
        SoftKey::Private => state.privacy = !state.privacy,
        SoftKey::ForwardAll => {
            state.forwarding.all =
                toggle_forwarding(state.forwarding.all.take(), defaults.forwarding.all.clone());
        }
        SoftKey::ForwardBusy => {
            state.forwarding.busy = toggle_forwarding(
                state.forwarding.busy.take(),
                defaults.forwarding.busy.clone(),
            );
        }
        SoftKey::ForwardNoAnswer => {
            state.forwarding.no_answer = toggle_forwarding(
                state.forwarding.no_answer.take(),
                defaults.forwarding.no_answer.clone(),
            );
        }
        _ => {}
    };
    match update_device_features_locked(access, &config, &device_id, mutation) {
        Ok(Some((previous, state))) => {
            publish_device_features(access, &device_id, &state);
            publish_feature_changes(access, &device_id, &previous, &state);
        }
        Ok(None) => {}
        Err(error) => {
            log_feature_store_error("persist handset feature mutation", Some(&device_id), &error);
            publish_current_device_features(access, &device_id);
        }
    }
}

pub(super) async fn handle_forwarding_soft_key(
    access: &Access,
    device_id: DeviceId,
    line_instance: u32,
    soft_key: SoftKey,
) {
    let kind = match soft_key {
        SoftKey::ForwardAll => ForwardingKind::All,
        SoftKey::ForwardBusy => ForwardingKind::Busy,
        SoftKey::ForwardNoAnswer => ForwardingKind::NoAnswer,
        _ => return,
    };
    let config = access.config();
    let binding = access.line_binding(&device_id, line_instance);
    let enabled = config
        .devices
        .get(&device_id)
        .is_some_and(|device| match kind {
            ForwardingKind::All => device.feature_defaults.forwarding.all_enabled,
            ForwardingKind::Busy => device.feature_defaults.forwarding.busy_enabled,
            ForwardingKind::NoAnswer => device.feature_defaults.forwarding.no_answer_enabled,
        });
    let current = controller_step(&access.shared.controller, |controller| {
        controller
            .feature_state(&device_id)
            .and_then(|state| match kind {
                ForwardingKind::All => state.forwarding.all.as_ref(),
                ForwardingKind::Busy => state.forwarding.busy.as_ref(),
                ForwardingKind::NoAnswer => state.forwarding.no_answer.as_ref(),
            })
            .is_some()
    });
    let timing = ForwardingEntryTiming {
        now: Instant::now(),
        first_digit_timeout: Duration::from_millis(config.general.first_digit_timeout_ms),
        interdigit_timeout: Duration::from_millis(config.general.interdigit_timeout_ms),
    };
    let Ok(dial_terminator) = dial_terminator_digit(config.general.dial_terminator.character)
    else {
        ast_log(
            LogLevel::Error,
            "configured forwarding dial terminator is invalid",
        );
        return;
    };
    drop(config);
    let Some(binding) = binding.filter(|_| enabled) else {
        return;
    };

    if current {
        if let Err(error) =
            execute_forwarding_mutation(access, device_id.clone(), binding.line.number, kind, None)
        {
            ast_log(
                LogLevel::Warning,
                &format!("unable to disable handset forwarding: {error}"),
            );
        }
        return;
    }

    let Some(codec) = preferred_codec(access, &device_id, line_instance, &PbxAudioFormat::ALL)
    else {
        return;
    };
    let call_id = access.phone.reserve_call_id();
    let entry = access.shared.forwarding_entries.lock_unpoisoned().begin(
        device_id.clone(),
        line_instance,
        call_id,
        kind,
        dial_terminator,
        timing,
    );
    let Ok(entry) = entry else {
        return;
    };
    let begin_outcome = send_confirmed_forwarding(
        access,
        PhoneCommand::new(
            device_id.clone(),
            PhoneCommandAction::BeginCall {
                line_instance: LineInstance::new(line_instance),
                call_id,
                codec,
            },
        ),
    )
    .await;
    let begin_settled = access
        .shared
        .forwarding_entries
        .lock_unpoisoned()
        .settle_collection_write(&device_id, entry.id, begin_outcome);
    if begin_settled != Ok(ForwardingWriteOutcome::Written) {
        if begin_outcome == ForwardingWriteOutcome::Written {
            let _ = send_confirmed_forwarding(
                access,
                PhoneCommand::new(device_id, PhoneCommandAction::CloseCall { call_id }),
            )
            .await;
        }
        return;
    }
    let prompt_outcome = send_confirmed_forwarding(
        access,
        PhoneCommand::new(
            device_id.clone(),
            PhoneCommandAction::DisplayPrompt {
                call_id,
                timeout_seconds: 0,
                text: "Enter forwarding destination".into(),
            },
        ),
    )
    .await;
    let prompt_settled = access
        .shared
        .forwarding_entries
        .lock_unpoisoned()
        .settle_collection_write(&device_id, entry.id, prompt_outcome);
    if prompt_settled != Ok(ForwardingWriteOutcome::Written) {
        let _ = send_confirmed_forwarding(
            access,
            PhoneCommand::new(device_id, PhoneCommandAction::CloseCall { call_id }),
        )
        .await;
    }
}

pub(super) async fn send_confirmed_forwarding(
    access: &Access,
    command: PhoneCommand,
) -> ForwardingWriteOutcome {
    if tokio::time::timeout(
        MANAGER_CONTROL_DELIVERY_TIMEOUT,
        access.phone.send_confirmed(command),
    )
    .await
    .is_ok_and(|result| result.is_ok())
    {
        ForwardingWriteOutcome::Written
    } else {
        ForwardingWriteOutcome::Failed
    }
}

pub(super) fn forwarding_entry_exists(
    access: &Access,
    device_id: &DeviceId,
    call_id: CallId,
) -> bool {
    access
        .shared
        .forwarding_entries
        .lock_unpoisoned()
        .for_call(call_id)
        .is_some_and(|entry| &entry.device_id == device_id)
}

pub(super) fn cancel_forwarding_entry_for_call(
    access: &Access,
    device_id: &DeviceId,
    call_id: CallId,
) -> bool {
    let mut entries = access.shared.forwarding_entries.lock_unpoisoned();
    let Some(entry) = entries
        .for_call(call_id)
        .filter(|entry| &entry.device_id == device_id)
        .cloned()
    else {
        return false;
    };
    entries.cancel_collection(device_id, entry.id).is_ok()
}

pub(super) fn cancel_forwarding_entry_for_device(access: &Access, device_id: &DeviceId) -> bool {
    let mut entries = access.shared.forwarding_entries.lock_unpoisoned();
    let Some(entry_id) = entries.get(device_id).map(|entry| entry.id) else {
        return false;
    };
    entries.cancel(device_id, entry_id).is_ok()
}

pub(super) async fn handle_forwarding_digit(
    access: &Access,
    device_id: &DeviceId,
    call_id: CallId,
    digit: Digit,
) -> bool {
    let result = {
        let mut entries = access.shared.forwarding_entries.lock_unpoisoned();
        let Some(entry) = entries.for_call(call_id).cloned() else {
            return false;
        };
        if &entry.device_id != device_id {
            return true;
        }
        entries.input_digit(device_id, entry.id, digit, Instant::now())
    };
    match result {
        Ok(ForwardingDigitOutcome::Collected) => {}
        Ok(ForwardingDigitOutcome::Commit(commit)) => {
            finish_forwarding_commit(access, commit).await;
        }
        Err(error) => {
            display_voicemail_prompt(
                access,
                device_id.clone(),
                Some(call_id),
                if error == ForwardingRejection::InvalidDestination {
                    "Enter a forwarding destination"
                } else {
                    "Forwarding destination is too long"
                },
            )
            .await;
        }
    }
    true
}

pub(super) fn handle_forwarding_backspace(access: &Access, device_id: &DeviceId, call_id: CallId) {
    let mut entries = access.shared.forwarding_entries.lock_unpoisoned();
    let Some(entry) = entries
        .for_call(call_id)
        .filter(|entry| &entry.device_id == device_id)
        .cloned()
    else {
        return;
    };
    let _ = entries.backspace(device_id, entry.id, Instant::now());
}

pub(super) async fn replace_and_commit_forwarding_entry(
    access: &Access,
    device_id: &DeviceId,
    call_id: CallId,
    digits: &str,
) -> bool {
    let result = {
        let mut entries = access.shared.forwarding_entries.lock_unpoisoned();
        let Some(entry) = entries.for_call(call_id).cloned() else {
            return false;
        };
        if &entry.device_id != device_id {
            return true;
        }
        entries.replace_digits(device_id, entry.id, digits, Instant::now())
    };
    if result.is_err() {
        display_voicemail_prompt(
            access,
            device_id.clone(),
            Some(call_id),
            "Invalid forwarding destination",
        )
        .await;
        return true;
    }
    commit_forwarding_entry(access, device_id, call_id).await;
    true
}

pub(super) async fn replace_forwarding_entry(
    access: &Access,
    device_id: &DeviceId,
    call_id: CallId,
    digits: &str,
) -> bool {
    let result = {
        let mut entries = access.shared.forwarding_entries.lock_unpoisoned();
        let Some(entry) = entries.for_call(call_id).cloned() else {
            return false;
        };
        if &entry.device_id != device_id {
            return true;
        }
        entries.replace_digits(device_id, entry.id, digits, Instant::now())
    };
    if result.is_err() {
        display_voicemail_prompt(
            access,
            device_id.clone(),
            Some(call_id),
            "Invalid forwarding destination",
        )
        .await;
    }
    true
}

pub(super) async fn commit_forwarding_entry(
    access: &Access,
    device_id: &DeviceId,
    call_id: CallId,
) {
    let commit = {
        let mut entries = access.shared.forwarding_entries.lock_unpoisoned();
        let Some(entry) = entries
            .for_call(call_id)
            .filter(|entry| &entry.device_id == device_id)
            .cloned()
        else {
            return;
        };
        entries.begin_commit(device_id, entry.id)
    };
    let Ok(commit) = commit else {
        display_voicemail_prompt(
            access,
            device_id.clone(),
            Some(call_id),
            "Enter a forwarding destination",
        )
        .await;
        return;
    };
    finish_forwarding_commit(access, commit).await;
}

pub(super) async fn finish_forwarding_commit(access: &Access, commit: ForwardingCommit) {
    let device_id = &commit.device_id;
    let call_id = commit.call_id;
    let close_outcome = send_confirmed_forwarding(
        access,
        PhoneCommand::new(device_id.clone(), PhoneCommandAction::CloseCall { call_id }),
    )
    .await;
    if access
        .shared
        .forwarding_entries
        .lock_unpoisoned()
        .settle_terminal_write(device_id, commit.entry_id, close_outcome)
        != Ok(ForwardingWriteOutcome::Written)
    {
        return;
    }
    let line = access
        .line_binding(device_id, commit.line_instance)
        .map(|binding| binding.line.number);
    let outcome = line
        .ok_or(FeatureControlProviderError::LineNotFound)
        .and_then(|line| {
            execute_forwarding_mutation(
                access,
                commit.device_id.clone(),
                line,
                commit.kind,
                Some(commit.destination.clone()),
            )
        });
    {
        let mut entries = access.shared.forwarding_entries.lock_unpoisoned();
        if outcome.is_ok() {
            let _ = entries.commit(device_id, commit.entry_id);
        } else {
            let _ = entries.cancel(device_id, commit.entry_id);
        }
    }
}

pub(super) async fn handle_voicemail_soft_key(
    access: &Access,
    device_id: DeviceId,
    call_id: Option<CallId>,
    _line_instance: u32,
    soft_key: SoftKey,
) {
    let selected_call_id = match soft_key {
        SoftKey::ImmediateDivert => call_id,
        SoftKey::TransferToVoicemail => controller_step(&access.shared.controller, |controller| {
            let device = controller.registered_device(&device_id)?;
            let selected = device.selected_calls().collect::<Vec<_>>();
            (selected.len() == 1).then_some(selected[0])
        }),
        _ => None,
    };
    let Some(selected_call_id) = selected_call_id else {
        display_voicemail_prompt(access, device_id, call_id, "Select exactly one call").await;
        return;
    };
    let call = controller_step(&access.shared.controller, |controller| {
        controller.call(selected_call_id)
    });
    let target = call.as_ref().and_then(|call| {
        let config = access.config();
        let binding = access.line_binding(&device_id, call.line_instance)?;
        let features = config.features_for_line(&binding.line.number)?;
        let destination = features.voicemail.divert_destination()?;
        VoicemailTarget::new(&binding.line.context, destination.as_str()).ok()
    });
    let Some(target) = target else {
        display_voicemail_prompt(
            access,
            device_id,
            Some(selected_call_id),
            "Voicemail is not configured",
        )
        .await;
        return;
    };
    let plan = controller_step(&access.shared.controller, |controller| match soft_key {
        SoftKey::ImmediateDivert => {
            controller.begin_immediate_divert(&device_id, selected_call_id, target)
        }
        SoftKey::TransferToVoicemail => {
            controller.begin_selected_voicemail_transfer(&device_id, target)
        }
        _ => unreachable!("voicemail handler only receives voicemail soft keys"),
    });
    let Ok(plan) = plan else {
        display_voicemail_prompt(
            access,
            device_id,
            Some(selected_call_id),
            "Voicemail action unavailable",
        )
        .await;
        return;
    };
    execute_voicemail_plan(access, plan).await;
}

pub(super) async fn execute_voicemail_plan(access: &Access, plan: VoicemailPlan) {
    let transaction = plan.transaction;
    let (active, line) = controller_step(&access.shared.controller, |controller| {
        (
            controller.voicemail_generation_is_active(&transaction.device_id, transaction.id),
            controller
                .pbx_call(transaction.pbx_call_id)
                .map(|call| call.line.clone()),
        )
    });
    if !active {
        return;
    }
    let operation = plan.effects.into_iter().find_map(|effect| match effect {
        DriverEffect::Backend(PbxEffect::Voicemail { operation }) => Some(operation),
        _ => None,
    });
    let Some(operation) = operation else {
        let _ = controller_step(&access.shared.controller, |controller| {
            controller.abort_voicemail(&transaction.device_id, transaction.id)
        });
        return;
    };
    if let Err(error) = AsteriskBackend::new(access).voicemail(&operation) {
        let _ = controller_step(&access.shared.controller, |controller| {
            controller.abort_voicemail(&transaction.device_id, transaction.id)
        });
        ast_log(
            LogLevel::Warning,
            &format!(
                "unable to execute voicemail action for PBX call {}: {error}",
                transaction.pbx_call_id.0
            ),
        );
        display_voicemail_prompt(
            access,
            transaction.device_id,
            Some(transaction.handset_call_id),
            "Unable to reach voicemail",
        )
        .await;
        return;
    }
    let outcome = controller_step(&access.shared.controller, |controller| {
        controller.complete_voicemail_native(
            &transaction.device_id,
            transaction.id,
            transaction.pbx_call_id,
        )
    });
    let outcome = match outcome {
        Ok(VoicemailNativeOutcome::Committed(outcome)) => outcome,
        Ok(VoicemailNativeOutcome::CallAlreadyEnded) => return,
        Err(error) => {
            ast_log(
                LogLevel::Error,
                &format!(
                    "voicemail routing completed for PBX call {} but controller ownership diverged: {error}",
                    transaction.pbx_call_id.0
                ),
            );
            return;
        }
    };
    if let Some(line) = line {
        publish_line(access, &line);
    }
    execute_effects(access, outcome.effects).await;
}

pub(super) async fn display_voicemail_prompt(
    access: &Access,
    device_id: DeviceId,
    call_id: Option<CallId>,
    text: &str,
) {
    if let Some(call_id) = call_id {
        let _ = access
            .phone
            .send(PhoneCommand::new(
                device_id,
                PhoneCommandAction::DisplayPrompt {
                    call_id,
                    timeout_seconds: 4,
                    text: text.into(),
                },
            ))
            .await;
    }
}

pub(super) fn handle_dnd_button(access: &Access, device_id: DeviceId, instance: u32) {
    if let Err(RuntimeDndMutationError::Store(error)) =
        execute_dnd_mutation(access, &device_id, RuntimeDndMutation::Button(instance))
    {
        log_feature_store_error("persist handset DND mutation", Some(&device_id), &error);
    }
}

pub(super) async fn handle_recording_button(
    access: &Access,
    recordings: &mut RuntimeRecordings,
    device_id: DeviceId,
    instance: u32,
) {
    let config = access.config();
    if !config
        .recording_buttons_for_device(&device_id)
        .any(|button| button.instance == instance)
    {
        return;
    }
    let current_call = controller_step(&access.shared.controller, |controller| {
        let call_id = controller.registered_device(&device_id)?.active_call()?;
        let call = controller.call(call_id)?;
        (&call.device_id == &device_id
            && (recordings.is_active_call(call.pbx_id)
                || matches!(
                    call.state,
                    crate::runtime::controller::CallState::Connected
                        | crate::runtime::controller::CallState::Barged
                )))
        .then_some(call_id)
    });
    if let Some(call_id) = current_call {
        if let Err(error) = toggle_monitor_recording(access, recordings, &device_id, call_id).await
        {
            ast_log(
                LogLevel::Warning,
                &format!("unable to change SCCP recording state: {error}"),
            );
            let _ = access
                .phone
                .send(PhoneCommand::new(
                    device_id,
                    PhoneCommandAction::DisplayPrompt {
                        call_id,
                        timeout_seconds: 4,
                        text: "Recording unavailable".into(),
                    },
                ))
                .await;
        }
        return;
    }

    let result = {
        let _feature_guard = access.shared.feature_mutations.lock_unpoisoned();
        update_device_features_locked(access, &config, &device_id, |state| {
            state.recording_armed = !state.recording_armed;
        })
    };
    match result {
        Ok(Some((_previous, _current))) => {
            publish_recording_button_state(access, recordings, &device_id);
        }
        Ok(None) => {}
        Err(error) => {
            log_feature_store_error("persist recording armed state", Some(&device_id), &error);
            publish_recording_button_state(access, recordings, &device_id);
        }
    }
}

pub(super) fn handle_feature_button(access: &Access, device_id: DeviceId, instance: u32) {
    let _feature_guard = access.shared.feature_mutations.lock_unpoisoned();
    let config = access.config();
    let configured = config.devices.get(&device_id).is_some_and(|device| {
        device.buttons.iter().any(|button| {
            matches!(
                button,
                ButtonDefinition::Feature(feature)
                    if feature.instance == instance
                        && feature.feature == ButtonType::Feature
            )
        })
    });
    if !configured {
        return;
    }

    match update_device_features_locked(access, &config, &device_id, |state| {
        if let Some(enabled) = state.buttons.get_mut(&instance) {
            *enabled = !*enabled;
        }
    }) {
        Ok(Some((previous, state))) => {
            publish_device_features(access, &device_id, &state);
            publish_feature_changes(access, &device_id, &previous, &state);
        }
        Ok(None) => {}
        Err(error) => {
            log_feature_store_error("persist feature-button mutation", Some(&device_id), &error);
            publish_current_device_features(access, &device_id);
        }
    }
}

pub(super) fn publish_current_device_features(access: &Access, device_id: &DeviceId) {
    let state = controller_step(&access.shared.controller, |controller| {
        controller.feature_state(device_id).cloned()
    });
    if let Some(state) = state {
        publish_device_features(access, device_id, &state);
    }
}

pub fn log_feature_store_error(action: &str, device: Option<&DeviceId>, error: &FeatureStoreError) {
    let level = if matches!(error, FeatureStoreError::Rollback { .. }) {
        LogLevel::Error
    } else {
        LogLevel::Warning
    };
    let subject = device.map_or_else(String::new, |device| format!(" for {device}"));
    ast_log(level, &format!("unable to {action}{subject}: {error}"));
}

pub(super) fn toggle_forwarding(
    current: Option<ForwardingDestination>,
    configured: Option<ForwardingDestination>,
) -> Option<ForwardingDestination> {
    if current.is_some() { None } else { configured }
}
