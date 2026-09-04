//! Runtime enforcement and CLI mutation of recurring device DND schedules.

use std::collections::BTreeMap;
use std::sync::Arc;

use thiserror::Error;

use super::{
    Access, DeviceId, DndMode, Instant, LogLevel, ModuleConfig, MutexExt as _, RuntimeDndMutation,
    RuntimeDndMutationError, ast_log, controller_step, execute_dnd_mutation_serialized,
};
use crate::asterisk::raw::{AsteriskTiming, AsteriskTimingError};
use crate::config::{
    DndSchedule, DndScheduleMode, DndScheduleSegment, DndScheduleValidationError,
    MAX_DND_SCHEDULE_BYTES, MAX_DND_SCHEDULES, validate_dnd_schedules,
};
use crate::state::dnd_schedule::{DndScheduleStore, DndScheduleStoreError};
use crate::state::persistence::PersistentStore;

const FAILURE_WARNING_INTERVAL_SECONDS: u64 = 30;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DndScheduleSource {
    Configuration,
    Override,
}

impl DndScheduleSource {
    fn name(self) -> &'static str {
        match self {
            Self::Configuration => "config",
            Self::Override => "override",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DndSchedulePhase {
    Inactive,
    Silent,
    Reject,
}

impl DndSchedulePhase {
    fn runtime_mode(self) -> DndMode {
        match self {
            Self::Inactive => DndMode::Off,
            Self::Silent => DndMode::Silent,
            Self::Reject => DndMode::Reject,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Inactive => "inactive",
            Self::Silent => "active (silent)",
            Self::Reject => "active (reject)",
        }
    }
}

#[derive(Clone)]
struct CompiledDndRule {
    mode: DndScheduleMode,
    timings: Vec<Arc<AsteriskTiming>>,
}

impl CompiledDndRule {
    fn new(schedule: &DndSchedule) -> Result<Self, DeviceDndScheduleError> {
        let timings = schedule
            .timing_segments()
            .into_iter()
            .map(|segment| {
                let spec = asterisk_timing_spec(segment)?;
                AsteriskTiming::parse(&spec)
                    .map(Arc::new)
                    .map_err(DeviceDndScheduleError::Timing)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            mode: schedule.mode(),
            timings,
        })
    }

    fn matches_now(&self) -> bool {
        self.timings.iter().any(|timing| timing.matches_now())
    }
}

#[derive(Clone)]
struct DeviceDndSchedule {
    source: DndScheduleSource,
    rules: Vec<DndSchedule>,
    compiled: Vec<CompiledDndRule>,
    last_phase: Option<DndSchedulePhase>,
    pending_catchup: bool,
    last_failure_warning: Option<Instant>,
}

impl DeviceDndSchedule {
    fn compile(
        source: DndScheduleSource,
        rules: Vec<DndSchedule>,
    ) -> Result<Self, DeviceDndScheduleError> {
        validate_dnd_schedules(&rules)?;
        let compiled = rules
            .iter()
            .map(CompiledDndRule::new)
            .collect::<Result<Vec<_>, _>>()?;
        let pending_catchup = !rules.is_empty();
        Ok(Self {
            source,
            rules,
            compiled,
            last_phase: None,
            pending_catchup,
            last_failure_warning: None,
        })
    }

    fn phase(&self) -> DndSchedulePhase {
        phase(&self.compiled)
    }
}

#[derive(Default)]
pub struct DndScheduleRegistry {
    devices: BTreeMap<DeviceId, DeviceDndSchedule>,
}

impl DndScheduleRegistry {
    pub(super) fn load<S: PersistentStore>(
        config: &ModuleConfig,
        store: &DndScheduleStore<S>,
    ) -> Result<Self, DndScheduleRegistryError> {
        let mut devices = BTreeMap::new();
        let mut configured_devices = config.devices.iter().collect::<Vec<_>>();
        configured_devices.sort_by(|(left, _), (right, _)| left.cmp(right));
        for (device_id, device) in configured_devices {
            let (source, rules) = match store.load_override(device_id).map_err(|source| {
                DndScheduleRegistryError::Store {
                    device: device_id.clone(),
                    source,
                }
            })? {
                Some(rules) => (DndScheduleSource::Override, rules),
                None => (
                    DndScheduleSource::Configuration,
                    device.dnd_schedules.clone(),
                ),
            };
            let schedule = DeviceDndSchedule::compile(source, rules).map_err(|source| {
                DndScheduleRegistryError::InvalidSchedule {
                    device: device_id.clone(),
                    source,
                }
            })?;
            devices.insert(device_id.clone(), schedule);
        }
        Ok(Self { devices })
    }

    pub(super) fn from_configuration(
        config: &ModuleConfig,
    ) -> Result<Self, DndScheduleRegistryError> {
        let mut devices = BTreeMap::new();
        let mut configured_devices = config.devices.iter().collect::<Vec<_>>();
        configured_devices.sort_by(|(left, _), (right, _)| left.cmp(right));
        for (device_id, device) in configured_devices {
            let schedule = DeviceDndSchedule::compile(
                DndScheduleSource::Configuration,
                device.dnd_schedules.clone(),
            )
            .map_err(|source| DndScheduleRegistryError::InvalidSchedule {
                device: device_id.clone(),
                source,
            })?;
            devices.insert(device_id.clone(), schedule);
        }
        Ok(Self { devices })
    }

    fn reconcile_live_ownership(&mut self, configured: &Self, live: &Self) {
        for (device, candidate) in &mut self.devices {
            let Some(current) = live.devices.get(device) else {
                continue;
            };
            match current.source {
                DndScheduleSource::Configuration => {
                    if let Some(configured) = configured.devices.get(device) {
                        *candidate = configured.clone();
                    }
                }
                DndScheduleSource::Override => *candidate = current.clone(),
            }
        }
        self.preserve_unchanged_phases(live);
    }

    fn preserve_unchanged_phases(&mut self, previous: &Self) {
        for (device, current) in &mut self.devices {
            let Some(old) = previous.devices.get(device) else {
                continue;
            };
            if old.rules == current.rules {
                current.last_phase = old.last_phase;
                current.pending_catchup = old.pending_catchup;
                current.last_failure_warning = old.last_failure_warning;
            } else if current.rules.is_empty() {
                current.pending_catchup = true;
            }
        }
    }
}

#[derive(Debug, Error)]
pub(super) enum DeviceDndScheduleError {
    #[error(transparent)]
    Validation(#[from] DndScheduleValidationError),
    #[error(transparent)]
    Timing(#[from] AsteriskTimingError),
    #[error("DND schedule contains an empty timing segment")]
    EmptySegment,
}

#[derive(Debug, Error)]
pub(super) enum DndScheduleRegistryError {
    #[error("unable to load DND schedule override for device {device}")]
    Store {
        device: DeviceId,
        #[source]
        source: DndScheduleStoreError,
    },
    #[error("invalid DND schedule for device {device}")]
    InvalidSchedule {
        device: DeviceId,
        #[source]
        source: DeviceDndScheduleError,
    },
}

struct DndScheduleEvaluation {
    device: DeviceId,
    compiled: Vec<CompiledDndRule>,
    last_phase: Option<DndSchedulePhase>,
    pending_catchup: bool,
    has_rules: bool,
}

impl DndScheduleEvaluation {
    fn phase(&self) -> DndSchedulePhase {
        phase(&self.compiled)
    }
}

pub(super) fn install_reloaded_dnd_schedules(
    access: &Access,
    mut candidate: DndScheduleRegistry,
    configured: &DndScheduleRegistry,
) {
    let _guard = access.shared.dnd_schedule_mutations.lock_unpoisoned();
    {
        let mut current = access.shared.dnd_schedules.lock_unpoisoned();
        candidate.reconcile_live_ownership(configured, &current);
        *current = candidate;
    }
    run_dnd_schedule_tick_serialized(access);
}

pub(super) fn run_dnd_schedule_tick(access: &Access) {
    let _guard = access.shared.dnd_schedule_mutations.lock_unpoisoned();
    run_dnd_schedule_tick_serialized(access);
}

fn run_dnd_schedule_tick_serialized(access: &Access) {
    let evaluations = {
        let schedules = access.shared.dnd_schedules.lock_unpoisoned();
        schedules
            .devices
            .iter()
            .filter_map(|(device, schedule)| {
                if schedule.rules.is_empty() && !schedule.pending_catchup {
                    return None;
                }
                Some(DndScheduleEvaluation {
                    device: device.clone(),
                    compiled: schedule.compiled.clone(),
                    last_phase: schedule.last_phase,
                    pending_catchup: schedule.pending_catchup,
                    has_rules: !schedule.rules.is_empty(),
                })
            })
            .collect::<Vec<_>>()
    };
    let due = evaluations
        .into_iter()
        .filter_map(|evaluation| {
            let phase = evaluation.phase();
            (evaluation.pending_catchup || evaluation.last_phase != Some(phase)).then_some((
                evaluation.device,
                phase,
                evaluation.has_rules,
            ))
        })
        .collect::<Vec<_>>();

    for (device, phase, has_rules) in due {
        match execute_dnd_mutation_serialized(
            access,
            &device,
            RuntimeDndMutation::Scheduled(phase.runtime_mode()),
        ) {
            Ok(_) => {
                if let Some(schedule) = access
                    .shared
                    .dnd_schedules
                    .lock_unpoisoned()
                    .devices
                    .get_mut(&device)
                {
                    schedule.last_phase = has_rules.then_some(phase);
                    schedule.pending_catchup = false;
                    schedule.last_failure_warning = None;
                }
            }
            Err(error) => {
                let should_warn = access
                    .shared
                    .dnd_schedules
                    .lock_unpoisoned()
                    .devices
                    .get_mut(&device)
                    .is_some_and(|schedule| {
                        let now = Instant::now();
                        let due = schedule.last_failure_warning.is_none_or(|previous| {
                            now.duration_since(previous).as_secs()
                                >= FAILURE_WARNING_INTERVAL_SECONDS
                        });
                        if due {
                            schedule.last_failure_warning = Some(now);
                        }
                        due
                    });
                if should_warn {
                    ast_log(
                        LogLevel::Warning,
                        &format!(
                            "unable to apply scheduled DND transition for {device}: {}",
                            dnd_mutation_error_name(&error)
                        ),
                    );
                }
            }
        }
    }
}

fn phase(compiled: &[CompiledDndRule]) -> DndSchedulePhase {
    compiled
        .iter()
        .find(|rule| rule.matches_now())
        .map_or(DndSchedulePhase::Inactive, |rule| match rule.mode {
            DndScheduleMode::Silent => DndSchedulePhase::Silent,
            DndScheduleMode::Reject => DndSchedulePhase::Reject,
        })
}

fn asterisk_timing_spec(segment: DndScheduleSegment) -> Result<String, DeviceDndScheduleError> {
    let inclusive_end = segment
        .end_minute_exclusive()
        .checked_sub(1)
        .ok_or(DeviceDndScheduleError::EmptySegment)?;
    Ok(format!(
        "{}-{},{},*,*",
        format_minute(segment.start_minute()),
        format_minute(inclusive_end),
        segment.weekdays(),
    ))
}

fn format_minute(minute: u16) -> String {
    format!("{:02}:{:02}", minute / 60, minute % 60)
}

pub fn execute_dnd_schedule_cli(access: &Access, arguments: &[String]) -> String {
    let Some(device_text) = arguments.first() else {
        return "Invalid DND schedule command arguments\n".into();
    };
    let Ok(device) = DeviceId::new(device_text) else {
        return "Invalid device selector\n".into();
    };
    if !access.config().devices.contains_key(&device) {
        return format!("DND schedule command failed: device {device} is not configured\n");
    }
    match arguments {
        [_, operation] if operation.eq_ignore_ascii_case("show") => show_schedule(access, &device),
        [_, operation] if operation.eq_ignore_ascii_case("clear") => {
            mutate_schedule(access, &device, ScheduleMutation::Clear)
        }
        [_, operation] if operation.eq_ignore_ascii_case("reset") => {
            mutate_schedule(access, &device, ScheduleMutation::Reset)
        }
        [_, operation, index] if operation.eq_ignore_ascii_case("remove") => {
            let Ok(index) = index.parse::<usize>() else {
                return "DND schedule remove requires a one-based rule index\n".into();
            };
            mutate_schedule(access, &device, ScheduleMutation::Remove(index))
        }
        [_, operation, time, days, mode] if operation.eq_ignore_ascii_case("add") => {
            let Some(bytes) = time
                .len()
                .checked_add(days.len())
                .and_then(|bytes| bytes.checked_add(mode.len()))
                .and_then(|bytes| bytes.checked_add(4))
            else {
                return "DND schedule command failed: rule is too long\n".into();
            };
            if bytes > MAX_DND_SCHEDULE_BYTES {
                return format!(
                    "DND schedule command failed: rule exceeds {MAX_DND_SCHEDULE_BYTES} bytes\n"
                );
            }
            let raw = format!("{time}, {days}, {mode}");
            match DndSchedule::parse(&raw) {
                Ok(rule) => mutate_schedule(access, &device, ScheduleMutation::Add(rule)),
                Err(error) => format!("DND schedule command failed: {error}\n"),
            }
        }
        _ => "Invalid DND schedule command arguments\n".into(),
    }
}

enum ScheduleMutation {
    Add(DndSchedule),
    Remove(usize),
    Clear,
    Reset,
}

fn mutate_schedule(access: &Access, device: &DeviceId, mutation: ScheduleMutation) -> String {
    let _guard = access.shared.dnd_schedule_mutations.lock_unpoisoned();
    let config = access.config();
    let Some(configured) = config
        .devices
        .get(device)
        .map(|device| device.dnd_schedules.clone())
    else {
        return format!("DND schedule command failed: device {device} is not configured\n");
    };
    let mut rules = {
        let schedules = access.shared.dnd_schedules.lock_unpoisoned();
        let Some(current) = schedules.devices.get(device) else {
            return "DND schedule command failed: schedule state is unavailable\n".into();
        };
        current.rules.clone()
    };

    let reset = matches!(&mutation, ScheduleMutation::Reset);
    match mutation {
        ScheduleMutation::Add(rule) => {
            if rules.len() >= MAX_DND_SCHEDULES {
                return format!(
                    "DND schedule command failed: maximum is {MAX_DND_SCHEDULES} rules\n"
                );
            }
            rules.push(rule);
        }
        ScheduleMutation::Remove(index) => {
            if index == 0 || index > rules.len() {
                return format!(
                    "DND schedule command failed: rule index must be between 1 and {}\n",
                    rules.len()
                );
            }
            rules.remove(index - 1);
        }
        ScheduleMutation::Clear => rules.clear(),
        ScheduleMutation::Reset => rules = configured,
    }
    let next_source = if reset {
        DndScheduleSource::Configuration
    } else {
        DndScheduleSource::Override
    };
    let next = match DeviceDndSchedule::compile(next_source, rules) {
        Ok(next) => next,
        Err(error) => return format!("DND schedule command failed: {error}\n"),
    };
    let phase = next.phase();
    let count = next.rules.len();
    let snapshot = match access.shared.dnd_schedule_store.snapshot_raw(device) {
        Ok(snapshot) => snapshot,
        Err(error) => return format!("DND schedule command failed: {error}\n"),
    };
    let persist = if reset {
        access.shared.dnd_schedule_store.reset(device)
    } else {
        access
            .shared
            .dnd_schedule_store
            .put_override(device, &next.rules)
    };
    if let Err(error) = persist {
        return format!("DND schedule command failed: {error}\n");
    }

    let previous = access
        .shared
        .dnd_schedules
        .lock_unpoisoned()
        .devices
        .insert(device.clone(), next);
    let transition = execute_dnd_mutation_serialized(
        access,
        device,
        RuntimeDndMutation::Scheduled(phase.runtime_mode()),
    );
    if let Err(error) = transition {
        let persistence_rollback = access
            .shared
            .dnd_schedule_store
            .restore_raw(device, snapshot.as_deref());
        let mut schedules = access.shared.dnd_schedules.lock_unpoisoned();
        match previous {
            Some(previous) => {
                schedules.devices.insert(device.clone(), previous);
            }
            None => {
                schedules.devices.remove(device);
            }
        }
        return match persistence_rollback {
            Ok(()) => format!(
                "DND schedule command failed and was rolled back: {}\n",
                dnd_mutation_error_name(&error)
            ),
            Err(rollback) => format!(
                "DND schedule command failed: {}; override rollback also failed: {rollback}\n",
                dnd_mutation_error_name(&error)
            ),
        };
    }
    if let Some(installed) = access
        .shared
        .dnd_schedules
        .lock_unpoisoned()
        .devices
        .get_mut(device)
    {
        installed.last_phase = (!installed.rules.is_empty()).then_some(phase);
        installed.pending_catchup = false;
    }
    format!(
        "{device}: DND schedule {} ({count} rule{})\n",
        if reset {
            "reset to config"
        } else {
            "override updated"
        },
        if count == 1 { "" } else { "s" }
    )
}

fn show_schedule(access: &Access, device: &DeviceId) -> String {
    let schedule = {
        let schedules = access.shared.dnd_schedules.lock_unpoisoned();
        schedules.devices.get(device).cloned()
    };
    let Some(schedule) = schedule else {
        return "DND schedule command failed: schedule state is unavailable\n".into();
    };
    let phase = schedule.phase();
    let actual = controller_step(&access.shared.controller, |controller| {
        controller.feature_state(device).map(|state| state.dnd)
    });
    let mut output = format!(
        "Device: {device}\nSource: {}\nScheduled phase: {}\nActual DND: {}\nRules:\n",
        schedule.source.name(),
        if schedule.rules.is_empty() {
            "disabled"
        } else {
            phase.name()
        },
        actual.map_or("unknown", runtime_dnd_mode_name)
    );
    if schedule.rules.is_empty() {
        output.push_str("  (none)\n");
    } else {
        for (index, rule) in schedule.rules.iter().enumerate() {
            output.push_str(&format!("  {}. {rule}\n", index + 1));
        }
    }
    output
}

pub fn complete_configured_dnd_device(
    access: &Access,
    prefix: &str,
    ordinal: usize,
) -> Option<String> {
    if prefix.len() > 128 || prefix.chars().any(char::is_control) {
        return None;
    }
    let mut devices = access
        .config()
        .devices
        .keys()
        .map(ToString::to_string)
        .filter(|candidate| {
            candidate
                .get(..prefix.len())
                .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
        })
        .collect::<Vec<_>>();
    devices.sort_by_key(|candidate| candidate.to_ascii_lowercase());
    devices.into_iter().nth(ordinal)
}

fn runtime_dnd_mode_name(mode: DndMode) -> &'static str {
    match mode {
        DndMode::Off => "off",
        DndMode::Silent => "silent",
        DndMode::Reject => "reject",
    }
}

fn dnd_mutation_error_name(error: &RuntimeDndMutationError) -> &'static str {
    match error {
        RuntimeDndMutationError::Unavailable => "runtime unavailable",
        RuntimeDndMutationError::DeviceNotFound => "device not found",
        RuntimeDndMutationError::FeatureDisabled => "DND feature disabled",
        RuntimeDndMutationError::ButtonNotFound => "DND button not found",
        RuntimeDndMutationError::Store(_) => "feature-state persistence failed",
    }
}
