//! Typed persistence for device feature state.

use std::collections::{BTreeSet, HashMap};

use sccp_protocol::DeviceId;
use thiserror::Error;

use crate::call::forwarding::ForwardingDestination;
use crate::config::{DndMode as ConfigDndMode, ModuleConfig};
use crate::runtime::controller::{DeviceFeatureState, DndMode, ForwardingState};
use crate::state::persistence::{PersistenceError, PersistentStore};

/// AstDB family used for mutable device feature state.
pub const FEATURE_FAMILY: &str = "SCCP";

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) fn registration_state_or_fallback<E>(
    loaded: Result<Option<DeviceFeatureState>, E>,
    previous: Option<DeviceFeatureState>,
    defaults: DeviceFeatureState,
) -> (DeviceFeatureState, Option<E>) {
    match loaded {
        Ok(Some(state)) => (state, None),
        Ok(None) => (defaults, None),
        Err(error) => (previous.unwrap_or(defaults), Some(error)),
    }
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
#[derive(Clone)]
pub(crate) enum FeatureMutation {
    Dnd(crate::call::dnd::DndMutation),
    TogglePrivacy,
    SetForwarding {
        kind: crate::call::forwarding::ForwardingKind,
        destination: Option<ForwardingDestination>,
    },
    ToggleForwarding {
        kind: crate::call::forwarding::ForwardingKind,
        default: Option<ForwardingDestination>,
    },
    ToggleRecording,
    ToggleButton(u32),
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
impl FeatureMutation {
    pub(crate) fn apply(self, state: &mut DeviceFeatureState) {
        use crate::call::forwarding::ForwardingKind;
        match self {
            Self::Dnd(mutation) => state.dnd = mutation.apply(state.dnd),
            Self::TogglePrivacy => state.privacy = !state.privacy,
            Self::ToggleRecording => state.recording_armed = !state.recording_armed,
            Self::ToggleButton(instance) => {
                if let Some(enabled) = state.buttons.get_mut(&instance) {
                    *enabled = !*enabled;
                }
            }
            Self::SetForwarding { kind, destination } => {
                *match kind {
                    ForwardingKind::All => &mut state.forwarding.all,
                    ForwardingKind::Busy => &mut state.forwarding.busy,
                    ForwardingKind::NoAnswer => &mut state.forwarding.no_answer,
                } = destination;
            }
            Self::ToggleForwarding { kind, default } => {
                let value = match kind {
                    ForwardingKind::All => &mut state.forwarding.all,
                    ForwardingKind::Busy => &mut state.forwarding.busy,
                    ForwardingKind::NoAnswer => &mut state.forwarding.no_answer,
                };
                *value = if value.is_some() { None } else { default };
            }
        }
    }
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) struct DeviceFeaturePlan {
    pub expected: Option<DeviceFeatureState>,
    pub previous: DeviceFeatureState,
    pub next: DeviceFeatureState,
}

/// Stores feature overrides relative to configured device defaults.
///
/// Keys use the stable layout `device/<device-id>/<feature>`. Callers must
/// include every configured feature-button instance in `defaults.buttons`,
/// including buttons whose default value is `false`, so they can be restored.
pub struct FeatureStore<S> {
    storage: S,
}

impl<S> FeatureStore<S> {
    pub const fn new(storage: S) -> Self {
        Self { storage }
    }

    pub fn storage(&self) -> &S {
        &self.storage
    }

    pub fn into_inner(self) -> S {
        self.storage
    }
}

impl<S: PersistentStore> FeatureStore<S> {
    /// Loads one configured device, or returns `None` for an unknown device.
    pub fn load_configured_device(
        &self,
        config: &ModuleConfig,
        device: &DeviceId,
    ) -> Result<Option<DeviceFeatureState>, FeatureStoreError> {
        configured_feature_state(config, device)
            .map(|defaults| self.load_configured(config, device, &defaults))
            .transpose()
    }

    /// Builds a complete candidate state map without mutating controller
    /// state. A failure for any device rejects the entire candidate.
    pub fn load_configuration(
        &self,
        config: &ModuleConfig,
    ) -> Result<HashMap<DeviceId, DeviceFeatureState>, FeatureStoreError> {
        let mut devices: Vec<_> = config.devices.keys().cloned().collect();
        devices.sort();
        devices
            .into_iter()
            .map(|device| {
                let defaults = configured_feature_state(config, &device)
                    .expect("device came from the same configuration");
                self.load_configured(config, &device, &defaults)
                    .map(|state| (device, state))
            })
            .collect()
    }

    /// Removes redundant overrides after a configuration/state candidate has
    /// become live. Stored values loaded into `states` remain semantically
    /// unchanged; values equal to the new defaults are deleted.
    pub fn reconcile_configuration(
        &self,
        config: &ModuleConfig,
        states: &HashMap<DeviceId, DeviceFeatureState>,
    ) -> Result<(), FeatureStoreError> {
        let mut devices: Vec<_> = config.devices.keys().cloned().collect();
        devices.sort();
        for device in devices {
            let defaults = configured_feature_state(config, &device)
                .expect("device came from the same configuration");
            let state = states
                .get(&device)
                .expect("state candidate covers every configured device");
            self.save(&device, state, &defaults)?;
        }
        Ok(())
    }

    /// Loads persisted overrides on top of the configured defaults.
    pub fn load(
        &self,
        device: &DeviceId,
        defaults: &DeviceFeatureState,
    ) -> Result<DeviceFeatureState, FeatureStoreError> {
        self.load_fields(device, defaults, true)
    }

    fn load_configured(
        &self,
        config: &ModuleConfig,
        device: &DeviceId,
        defaults: &DeviceFeatureState,
    ) -> Result<DeviceFeatureState, FeatureStoreError> {
        let recording_configured = config.recording_buttons_for_device(device).next().is_some();
        self.load_fields(device, defaults, recording_configured)
    }

    fn load_fields(
        &self,
        device: &DeviceId,
        defaults: &DeviceFeatureState,
        recording_configured: bool,
    ) -> Result<DeviceFeatureState, FeatureStoreError> {
        let mut state = defaults.clone();

        if let Some(value) = self.get(device, "dnd")? {
            state.dnd = parse_dnd(device, "dnd", &value)?;
        }
        if let Some(value) = self.get(device, "privacy")? {
            state.privacy = parse_bool(device, "privacy", &value)?;
        }
        if recording_configured && let Some(value) = self.get(device, "recording/armed")? {
            state.recording_armed = parse_bool(device, "recording/armed", &value)?;
        }
        state.forwarding.all = self.load_forwarding(device, "forward/all", state.forwarding.all)?;
        state.forwarding.busy =
            self.load_forwarding(device, "forward/busy", state.forwarding.busy)?;
        state.forwarding.no_answer =
            self.load_forwarding(device, "forward/no-answer", state.forwarding.no_answer)?;

        let mut button_instances: Vec<_> = defaults.buttons.keys().copied().collect();
        button_instances.sort_unstable();
        for instance in button_instances {
            let suffix = format!("button/{instance}");
            if let Some(value) = self.get(device, &suffix)? {
                state
                    .buttons
                    .insert(instance, parse_bool(device, &suffix, &value)?);
            }
        }

        Ok(state)
    }

    /// Persists differences from configured defaults and deletes redundant keys.
    pub fn save(
        &self,
        device: &DeviceId,
        state: &DeviceFeatureState,
        defaults: &DeviceFeatureState,
    ) -> Result<(), FeatureStoreError> {
        if let Some(instance) = state
            .buttons
            .keys()
            .find(|instance| !defaults.buttons.contains_key(instance))
        {
            return Err(FeatureStoreError::UnknownFeatureButton {
                device: device.clone(),
                instance: *instance,
            });
        }
        let mut writes = vec![
            Self::scalar_write(
                device,
                "dnd",
                state.dnd != defaults.dnd,
                encode_dnd(state.dnd),
            ),
            Self::scalar_write(
                device,
                "privacy",
                state.privacy != defaults.privacy,
                encode_bool(state.privacy),
            ),
            Self::scalar_write(
                device,
                "recording/armed",
                state.recording_armed != defaults.recording_armed,
                encode_bool(state.recording_armed),
            ),
            Self::forwarding_write(
                device,
                "forward/all",
                state.forwarding.all.as_ref(),
                defaults.forwarding.all.as_ref(),
            ),
            Self::forwarding_write(
                device,
                "forward/busy",
                state.forwarding.busy.as_ref(),
                defaults.forwarding.busy.as_ref(),
            ),
            Self::forwarding_write(
                device,
                "forward/no-answer",
                state.forwarding.no_answer.as_ref(),
                defaults.forwarding.no_answer.as_ref(),
            ),
        ];

        let button_instances: BTreeSet<_> = state
            .buttons
            .keys()
            .chain(defaults.buttons.keys())
            .copied()
            .collect();
        for instance in button_instances {
            let suffix = format!("button/{instance}");
            let enabled = state.buttons.get(&instance).copied().unwrap_or(false);
            let default = defaults.buttons.get(&instance).copied().unwrap_or(false);
            writes.push(Self::scalar_write(
                device,
                &suffix,
                enabled != default,
                encode_bool(enabled),
            ));
        }

        self.apply_transaction(writes)
    }

    /// Persist a proposed state before returning it to the caller for an
    /// in-memory commit. On any storage failure the caller retains `current`
    /// and the persisted keys are restored transactionally.
    pub fn update(
        &self,
        device: &DeviceId,
        current: &DeviceFeatureState,
        defaults: &DeviceFeatureState,
        mutation: impl FnOnce(&mut DeviceFeatureState),
    ) -> Result<DeviceFeatureState, FeatureStoreError> {
        let mut next = current.clone();
        mutation(&mut next);
        if next == *current {
            return Ok(next);
        }
        self.save(device, &next, defaults)?;
        Ok(next)
    }

    fn load_forwarding(
        &self,
        device: &DeviceId,
        suffix: &str,
        default: Option<ForwardingDestination>,
    ) -> Result<Option<ForwardingDestination>, FeatureStoreError> {
        self.get(device, suffix)?
            .map(|value| parse_forwarding(device, suffix, &value))
            .transpose()
            .map(|value| value.unwrap_or(default))
    }

    fn forwarding_write(
        device: &DeviceId,
        suffix: &str,
        value: Option<&ForwardingDestination>,
        default: Option<&ForwardingDestination>,
    ) -> FeatureWrite {
        Self::scalar_write(
            device,
            suffix,
            value != default,
            encode_forwarding(value.map(ForwardingDestination::as_str)),
        )
    }

    fn get(&self, device: &DeviceId, suffix: &str) -> Result<Option<String>, FeatureStoreError> {
        Ok(self.storage.get(FEATURE_FAMILY, &key(device, suffix))?)
    }

    fn scalar_write(
        device: &DeviceId,
        suffix: &str,
        differs_from_default: bool,
        encoded: String,
    ) -> FeatureWrite {
        FeatureWrite {
            key: key(device, suffix),
            value: differs_from_default.then_some(encoded),
        }
    }

    fn apply_transaction(&self, writes: Vec<FeatureWrite>) -> Result<(), FeatureStoreError> {
        let previous = writes
            .iter()
            .map(|write| self.storage.get(FEATURE_FAMILY, &write.key))
            .collect::<Result<Vec<_>, _>>()?;

        for (index, write) in writes.iter().enumerate() {
            let result = match &write.value {
                Some(value) => self.storage.put(FEATURE_FAMILY, &write.key, value),
                None => self.storage.delete(FEATURE_FAMILY, &write.key),
            };
            if let Err(source) = result {
                let mut rollback_failure = None;
                for rollback_index in (0..=index).rev() {
                    let rollback = match &previous[rollback_index] {
                        Some(value) => {
                            self.storage
                                .put(FEATURE_FAMILY, &writes[rollback_index].key, value)
                        }
                        None => self
                            .storage
                            .delete(FEATURE_FAMILY, &writes[rollback_index].key),
                    };
                    if rollback_failure.is_none() {
                        rollback_failure = rollback.err();
                    }
                }
                return match rollback_failure {
                    Some(rollback) => Err(FeatureStoreError::Rollback { source, rollback }),
                    None => Err(source.into()),
                };
            }
        }
        Ok(())
    }
}

/// Derives every mutable default, including all configured feature-button
/// instances, from one normalized configuration snapshot.
pub fn configured_feature_state(
    config: &ModuleConfig,
    device: &DeviceId,
) -> Option<DeviceFeatureState> {
    let defaults = &config.devices.get(device)?.feature_defaults;
    Some(DeviceFeatureState {
        dnd: match defaults.dnd {
            ConfigDndMode::Off => DndMode::Off,
            ConfigDndMode::Silent => DndMode::Silent,
            ConfigDndMode::Reject => DndMode::Reject,
        },
        privacy: defaults.privacy,
        recording_armed: false,
        forwarding: ForwardingState {
            all: defaults.forwarding.all.clone(),
            busy: defaults.forwarding.busy.clone(),
            no_answer: defaults.forwarding.no_answer.clone(),
        },
        buttons: defaults.buttons.clone(),
    })
}

struct FeatureWrite {
    key: String,
    value: Option<String>,
}

#[derive(Debug, Error)]
pub enum FeatureStoreError {
    #[error(transparent)]
    Storage(#[from] PersistenceError),

    #[error("feature-state persistence failed and rollback also failed")]
    Rollback {
        source: PersistenceError,
        rollback: PersistenceError,
    },

    #[error("invalid persisted value at {family}/{key}; expected {expected}")]
    CorruptValue {
        family: &'static str,
        key: String,
        expected: &'static str,
    },

    #[error("device {device} has no configured mutable feature button {instance}")]
    UnknownFeatureButton { device: DeviceId, instance: u32 },
}

fn key(device: &DeviceId, suffix: &str) -> String {
    format!("device/{}/{suffix}", device.as_str())
}

fn corrupt(device: &DeviceId, suffix: &str, expected: &'static str) -> FeatureStoreError {
    FeatureStoreError::CorruptValue {
        family: FEATURE_FAMILY,
        key: key(device, suffix),
        expected,
    }
}

fn encode_dnd(mode: DndMode) -> String {
    match mode {
        DndMode::Off => "off",
        DndMode::Silent => "silent",
        DndMode::Reject => "reject",
    }
    .into()
}

fn parse_dnd(device: &DeviceId, suffix: &str, value: &str) -> Result<DndMode, FeatureStoreError> {
    match value {
        "off" => Ok(DndMode::Off),
        "silent" => Ok(DndMode::Silent),
        "reject" => Ok(DndMode::Reject),
        _ => Err(corrupt(device, suffix, "off, silent, or reject")),
    }
}

fn encode_bool(value: bool) -> String {
    value.to_string()
}

fn parse_bool(device: &DeviceId, suffix: &str, value: &str) -> Result<bool, FeatureStoreError> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(corrupt(device, suffix, "true or false")),
    }
}

fn encode_forwarding(value: Option<&str>) -> String {
    match value {
        Some(destination) => format!("destination:{destination}"),
        None => "none".into(),
    }
}

fn parse_forwarding(
    device: &DeviceId,
    suffix: &str,
    value: &str,
) -> Result<Option<ForwardingDestination>, FeatureStoreError> {
    if value == "none" {
        return Ok(None);
    }
    if let Some(destination) = value.strip_prefix("destination:")
        && !destination.is_empty()
    {
        return ForwardingDestination::new(destination)
            .map(Some)
            .map_err(|_| corrupt_forwarding(device, suffix));
    }
    Err(corrupt_forwarding(device, suffix))
}

fn corrupt_forwarding(device: &DeviceId, suffix: &str) -> FeatureStoreError {
    corrupt(
        device,
        suffix,
        "none or destination:<bounded printable destination>",
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;
    use crate::runtime::controller::ForwardingState;

    #[derive(Default)]
    struct MemoryStore {
        entries: Mutex<HashMap<(String, String), String>>,
        writes: Mutex<usize>,
        fail_on_writes: Mutex<BTreeSet<usize>>,
    }

    impl MemoryStore {
        fn insert(&self, family: &str, key: &str, value: &str) {
            self.entries
                .lock()
                .unwrap()
                .insert((family.to_owned(), key.to_owned()), value.to_owned());
        }

        fn snapshot(&self) -> HashMap<(String, String), String> {
            self.entries.lock().unwrap().clone()
        }

        fn fail_on_write(&self, write: usize) {
            self.fail_on_writes.lock().unwrap().insert(write);
        }

        fn write_allowed(&self) -> Result<(), PersistenceError> {
            let mut writes = self.writes.lock().unwrap();
            *writes += 1;
            if self.fail_on_writes.lock().unwrap().contains(&*writes) {
                Err(PersistenceError::Backend {
                    operation: "test write",
                })
            } else {
                Ok(())
            }
        }
    }

    impl PersistentStore for MemoryStore {
        fn get(&self, family: &str, key: &str) -> Result<Option<String>, PersistenceError> {
            Ok(self
                .entries
                .lock()
                .unwrap()
                .get(&(family.to_owned(), key.to_owned()))
                .cloned())
        }

        fn put(&self, family: &str, key: &str, value: &str) -> Result<(), PersistenceError> {
            self.write_allowed()?;
            self.insert(family, key, value);
            Ok(())
        }

        fn delete(&self, family: &str, key: &str) -> Result<(), PersistenceError> {
            self.write_allowed()?;
            self.entries
                .lock()
                .unwrap()
                .remove(&(family.to_owned(), key.to_owned()));
            Ok(())
        }
    }

    fn device() -> DeviceId {
        DeviceId::new("SEP001122334455").unwrap()
    }

    fn forwarding(value: &str) -> ForwardingDestination {
        ForwardingDestination::new(value).unwrap()
    }

    fn defaults() -> DeviceFeatureState {
        DeviceFeatureState {
            dnd: DndMode::Off,
            privacy: false,
            recording_armed: false,
            forwarding: ForwardingState {
                all: None,
                busy: Some(forwarding("9000")),
                no_answer: None,
            },
            buttons: HashMap::from([(4, false), (5, true)]),
        }
    }

    fn configured(default_enabled: bool, include_second: bool) -> ModuleConfig {
        let second = if include_second {
            "[SEP112233445566]\ntype=device\nbutton=feature, Night service, feature\nline=1001\n"
        } else {
            ""
        };
        ModuleConfig::parse(&format!(
            r#"
            [general]
            advertised_address = 192.0.2.10

            [SEP001122334455]
            type = device
            dnd = off
            privacy = no
            button = feature, Night service, feature
            feature_default = 1, {}
            line = 1001

            [1001]
            type = line
            label = Desk

            {second}
            "#,
            if default_enabled { "yes" } else { "no" }
        ))
        .unwrap()
    }

    fn configured_dnd_buttons(include_buttons: bool) -> ModuleConfig {
        let buttons = if include_buttons {
            r#"
            button = feature, Cycle DND, dnd
            button = feature, Silent DND, dnd, silent
            button = feature, Reject DND, dnd, reject
            "#
        } else {
            ""
        };
        ModuleConfig::parse(&format!(
            r#"
            [general]
            advertised_address = 192.0.2.10

            [SEP001122334455]
            type = device
            dnd_feature = yes
            dnd = off
            {buttons}
            line = 1001

            [1001]
            type = line
            label = Desk
            "#
        ))
        .unwrap()
    }

    fn configured_recording_button(include_button: bool) -> ModuleConfig {
        let button = if include_button {
            "button = feature, Record calls, monitor"
        } else {
            ""
        };
        ModuleConfig::parse(&format!(
            r#"
            [general]
            advertised_address = 192.0.2.10

            [SEP001122334455]
            type = device
            {button}
            line = 1001

            [1001]
            type = line
            label = Desk
            "#
        ))
        .unwrap()
    }

    #[test]
    fn round_trips_typed_overrides_with_stable_keys() {
        let repository = FeatureStore::new(MemoryStore::default());
        let defaults = defaults();
        let state = DeviceFeatureState {
            dnd: DndMode::Reject,
            privacy: true,
            recording_armed: true,
            forwarding: ForwardingState {
                all: Some(forwarding("2000")),
                busy: None,
                no_answer: Some(forwarding("2001")),
            },
            buttons: HashMap::from([(4, true), (5, false)]),
        };

        repository.save(&device(), &state, &defaults).unwrap();

        assert_eq!(repository.load(&device(), &defaults).unwrap(), state);
        assert_eq!(
            repository.storage().snapshot(),
            HashMap::from([
                (
                    (FEATURE_FAMILY.into(), "device/SEP001122334455/dnd".into()),
                    "reject".into()
                ),
                (
                    (
                        FEATURE_FAMILY.into(),
                        "device/SEP001122334455/privacy".into()
                    ),
                    "true".into()
                ),
                (
                    (
                        FEATURE_FAMILY.into(),
                        "device/SEP001122334455/recording/armed".into()
                    ),
                    "true".into()
                ),
                (
                    (
                        FEATURE_FAMILY.into(),
                        "device/SEP001122334455/forward/all".into()
                    ),
                    "destination:2000".into()
                ),
                (
                    (
                        FEATURE_FAMILY.into(),
                        "device/SEP001122334455/forward/busy".into()
                    ),
                    "none".into()
                ),
                (
                    (
                        FEATURE_FAMILY.into(),
                        "device/SEP001122334455/forward/no-answer".into()
                    ),
                    "destination:2001".into()
                ),
                (
                    (
                        FEATURE_FAMILY.into(),
                        "device/SEP001122334455/button/4".into()
                    ),
                    "true".into()
                ),
                (
                    (
                        FEATURE_FAMILY.into(),
                        "device/SEP001122334455/button/5".into()
                    ),
                    "false".into()
                ),
            ])
        );
    }

    #[test]
    fn returning_to_defaults_deletes_every_override() {
        let repository = FeatureStore::new(MemoryStore::default());
        let defaults = defaults();
        let changed = DeviceFeatureState {
            dnd: DndMode::Silent,
            privacy: true,
            recording_armed: true,
            forwarding: ForwardingState {
                all: Some(forwarding("2000")),
                busy: None,
                no_answer: Some(forwarding("2001")),
            },
            buttons: HashMap::from([(4, true), (5, false)]),
        };
        repository.save(&device(), &changed, &defaults).unwrap();

        repository.save(&device(), &defaults, &defaults).unwrap();

        assert!(repository.storage().snapshot().is_empty());
    }

    #[test]
    fn idempotent_mutation_performs_no_storage_writes() {
        let repository = FeatureStore::new(MemoryStore::default());
        let current = defaults();

        assert_eq!(
            repository
                .update(&device(), &current, &current, |_| {})
                .unwrap(),
            current
        );
        assert_eq!(*repository.storage().writes.lock().unwrap(), 0);
        assert!(repository.storage().snapshot().is_empty());
    }

    #[test]
    fn recording_armed_restores_semantically_and_removal_cleans_the_override() {
        let storage = MemoryStore::default();
        storage.insert(
            FEATURE_FAMILY,
            "device/SEP001122334455/recording/armed",
            "true",
        );
        let repository = FeatureStore::new(storage);
        let with_button = configured_recording_button(true);
        let armed = repository
            .load_configured_device(&with_button, &device())
            .unwrap()
            .unwrap();
        assert!(armed.recording_armed);

        repository.storage().insert(
            FEATURE_FAMILY,
            "device/SEP001122334455/recording/armed",
            "stale-invalid-value",
        );
        let without_button = configured_recording_button(false);
        let disarmed = repository.load_configuration(&without_button).unwrap();
        assert!(!disarmed[&device()].recording_armed);
        repository
            .reconcile_configuration(&without_button, &disarmed)
            .unwrap();

        assert!(!repository.storage().snapshot().contains_key(&(
            FEATURE_FAMILY.into(),
            "device/SEP001122334455/recording/armed".into()
        )));
    }

    #[test]
    fn failed_multi_key_mutation_restores_the_previous_persisted_state() {
        let storage = MemoryStore::default();
        storage.insert(FEATURE_FAMILY, "device/SEP001122334455/privacy", "true");
        let before = storage.snapshot();
        storage.fail_on_write(2);
        let repository = FeatureStore::new(storage);
        let changed = DeviceFeatureState {
            dnd: DndMode::Reject,
            privacy: false,
            recording_armed: true,
            forwarding: ForwardingState {
                all: Some(forwarding("2000")),
                busy: None,
                no_answer: Some(forwarding("2001")),
            },
            buttons: HashMap::from([(4, true), (5, false)]),
        };

        let current = defaults();
        assert!(
            repository
                .update(&device(), &current, &current, |state| *state = changed)
                .is_err()
        );
        assert_eq!(current, defaults());
        assert_eq!(repository.storage().snapshot(), before);
    }

    #[test]
    fn corrupt_values_report_the_exact_key_and_expected_type() {
        let storage = MemoryStore::default();
        storage.insert(
            FEATURE_FAMILY,
            "device/SEP001122334455/privacy",
            "private-invalid-value",
        );
        let repository = FeatureStore::new(storage);

        let error = repository.load(&device(), &defaults()).unwrap_err();

        assert!(matches!(
            error,
            FeatureStoreError::CorruptValue {
                family: FEATURE_FAMILY,
                ref key,
                expected: "true or false",
            } if key == "device/SEP001122334455/privacy"
        ));
        assert!(!format!("{error:?}").contains("private-invalid-value"));
    }

    #[test]
    fn corrupt_forwarding_and_button_values_are_rejected() {
        for (suffix, value, expected) in [
            (
                "forward/all",
                "private-invalid-forward",
                "none or destination:<bounded printable destination>",
            ),
            ("button/4", "private-invalid-button", "true or false"),
            ("dnd", "private-invalid-dnd", "off, silent, or reject"),
        ] {
            let storage = MemoryStore::default();
            storage.insert(
                FEATURE_FAMILY,
                &format!("device/SEP001122334455/{suffix}"),
                value,
            );
            let repository = FeatureStore::new(storage);

            let error = repository.load(&device(), &defaults()).unwrap_err();
            let diagnostic = format!("{error:?}");

            assert!(matches!(
                &error,
                FeatureStoreError::CorruptValue { expected: actual, .. } if *actual == expected
            ));
            assert!(!diagnostic.contains(value));
        }
    }

    #[test]
    fn configuration_lifecycle_loads_restart_and_registration_state() {
        let storage = MemoryStore::default();
        storage.insert(FEATURE_FAMILY, "device/SEP001122334455/dnd", "reject");
        storage.insert(
            FEATURE_FAMILY,
            "device/SEP001122334455/forward/all",
            "destination:7000",
        );
        storage.insert(
            FEATURE_FAMILY,
            "device/SEP001122334455/forward/busy",
            "destination:7001",
        );
        storage.insert(
            FEATURE_FAMILY,
            "device/SEP001122334455/forward/no-answer",
            "destination:7002",
        );
        storage.insert(FEATURE_FAMILY, "device/SEP001122334455/button/1", "true");
        let repository = FeatureStore::new(storage);
        let config = configured(false, false);
        let device = device();

        let startup = repository.load_configuration(&config).unwrap();
        let restart = repository.load_configuration(&config).unwrap();
        let registration = repository
            .load_configured_device(&config, &device)
            .unwrap()
            .unwrap();

        assert_eq!(startup, restart);
        assert_eq!(startup[&device], registration);
        assert_eq!(registration.dnd, DndMode::Reject);
        assert_eq!(
            registration
                .forwarding
                .all
                .as_ref()
                .map(ForwardingDestination::as_str),
            Some("7000")
        );
        assert_eq!(
            registration
                .forwarding
                .busy
                .as_ref()
                .map(ForwardingDestination::as_str),
            Some("7001")
        );
        assert_eq!(
            registration
                .forwarding
                .no_answer
                .as_ref()
                .map(ForwardingDestination::as_str),
            Some("7002")
        );
        assert_eq!(registration.buttons, HashMap::from([(1, true)]));
        assert!(
            repository
                .load_configured_device(&config, &DeviceId::new("SEPFFEEDDCCBBAA").unwrap())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn registration_restore_retains_last_committed_state_on_read_failure() {
        let configured = defaults();
        let mut committed = configured.clone();
        committed.dnd = DndMode::Silent;
        let mut restored = configured.clone();
        restored.dnd = DndMode::Reject;

        assert_eq!(
            registration_state_or_fallback::<&str>(
                Ok(Some(restored.clone())),
                Some(committed.clone()),
                configured.clone(),
            ),
            (restored, None)
        );
        assert_eq!(
            registration_state_or_fallback::<&str>(
                Ok(None),
                Some(committed.clone()),
                configured.clone(),
            ),
            (configured.clone(), None)
        );
        assert_eq!(
            registration_state_or_fallback(
                Err("read failed"),
                Some(committed.clone()),
                configured.clone(),
            ),
            (committed, Some("read failed"))
        );
        assert_eq!(
            registration_state_or_fallback(Err("read failed"), None, configured.clone()),
            (configured, Some("read failed"))
        );
    }

    #[test]
    fn every_dnd_mode_survives_the_persisted_restart_boundary() {
        for mode in [DndMode::Off, DndMode::Silent, DndMode::Reject] {
            let repository = FeatureStore::new(MemoryStore::default());
            let mut configured_defaults = defaults();
            configured_defaults.dnd = DndMode::Off;
            let mut expected = configured_defaults.clone();
            expected.dnd = mode;

            repository
                .save(&device(), &expected, &configured_defaults)
                .unwrap();

            assert_eq!(
                repository
                    .load(&device(), &configured_defaults)
                    .unwrap()
                    .dnd,
                mode
            );
        }
    }

    #[test]
    fn returning_dnd_to_the_off_default_removes_its_override() {
        let repository = FeatureStore::new(MemoryStore::default());
        let defaults = defaults();
        let enabled = repository
            .update(&device(), &defaults, &defaults, |state| {
                state.dnd = DndMode::Silent;
            })
            .unwrap();
        assert_eq!(enabled.dnd, DndMode::Silent);
        assert_eq!(
            repository
                .storage()
                .snapshot()
                .get(&(FEATURE_FAMILY.into(), "device/SEP001122334455/dnd".into())),
            Some(&"silent".into())
        );

        let restored = repository
            .update(&device(), &enabled, &defaults, |state| {
                state.dnd = DndMode::Off;
            })
            .unwrap();

        assert_eq!(restored, defaults);
        assert!(repository.storage().snapshot().is_empty());
    }

    #[test]
    fn failed_dnd_write_returns_no_false_mode_and_leaves_no_override() {
        let storage = MemoryStore::default();
        storage.fail_on_write(1);
        let repository = FeatureStore::new(storage);
        let current = defaults();

        assert!(
            repository
                .update(&device(), &current, &current, |state| {
                    state.dnd = DndMode::Reject;
                })
                .is_err()
        );
        assert_eq!(current.dnd, DndMode::Off);
        assert!(repository.storage().snapshot().is_empty());
    }

    #[test]
    fn removed_and_readded_dnd_buttons_restore_one_exact_device_mode() {
        let storage = MemoryStore::default();
        storage.insert(FEATURE_FAMILY, "device/SEP001122334455/dnd", "silent");
        let repository = FeatureStore::new(storage);
        let device = device();
        let configured = configured_dnd_buttons(true);
        let removed = configured_dnd_buttons(false);

        assert_eq!(
            repository.load_configuration(&configured).unwrap()[&device].dnd,
            DndMode::Silent
        );
        assert_eq!(
            configured
                .dnd_buttons_for_device(&device)
                .collect::<Vec<_>>(),
            [
                (1, crate::config::DndButtonMode::Cycle),
                (2, crate::config::DndButtonMode::Silent),
                (3, crate::config::DndButtonMode::Reject),
            ]
        );
        assert_eq!(
            repository.load_configuration(&removed).unwrap()[&device].dnd,
            DndMode::Silent
        );
        assert_eq!(removed.dnd_buttons_for_device(&device).count(), 0);
        assert_eq!(
            repository.load_configuration(&configured).unwrap()[&device].dnd,
            DndMode::Silent
        );
        assert_eq!(*repository.storage().writes.lock().unwrap(), 0);
    }

    #[test]
    fn failed_reload_candidate_does_not_partially_replace_live_states() {
        let repository = FeatureStore::new(MemoryStore::default());
        let original = configured(false, false);
        let live = repository.load_configuration(&original).unwrap();
        repository
            .storage()
            .insert(FEATURE_FAMILY, "device/SEP112233445566/privacy", "corrupt");

        let error = repository
            .load_configuration(&configured(false, true))
            .unwrap_err();

        assert!(matches!(error, FeatureStoreError::CorruptValue { .. }));
        assert_eq!(live.len(), 1);
        assert_eq!(live[&device()].buttons, HashMap::from([(1, false)]));
    }

    #[test]
    fn reload_reconciliation_deletes_values_equal_to_new_defaults() {
        let storage = MemoryStore::default();
        storage.insert(FEATURE_FAMILY, "device/SEP001122334455/button/1", "true");
        let repository = FeatureStore::new(storage);
        let next = configured(true, false);
        let states = repository.load_configuration(&next).unwrap();

        repository.reconcile_configuration(&next, &states).unwrap();

        assert_eq!(states[&device()].buttons, HashMap::from([(1, true)]));
        assert!(repository.storage().snapshot().is_empty());
    }

    #[test]
    fn unconfigured_feature_instances_never_create_persisted_keys() {
        let repository = FeatureStore::new(MemoryStore::default());
        let defaults = configured_feature_state(&configured(false, false), &device()).unwrap();
        let current = defaults.clone();

        let error = repository
            .update(&device(), &current, &defaults, |state| {
                state.buttons.insert(99, true);
            })
            .unwrap_err();

        assert!(matches!(
            error,
            FeatureStoreError::UnknownFeatureButton { instance: 99, .. }
        ));
        assert!(repository.storage().snapshot().is_empty());
    }

    #[test]
    fn handset_style_toggle_persists_then_deletes_the_default_override() {
        let repository = FeatureStore::new(MemoryStore::default());
        let defaults = configured_feature_state(&configured(false, false), &device()).unwrap();
        let enabled = repository
            .update(&device(), &defaults, &defaults, |state| {
                *state.buttons.get_mut(&1).unwrap() = true;
            })
            .unwrap();
        assert_eq!(
            repository.storage().snapshot().get(&(
                FEATURE_FAMILY.into(),
                "device/SEP001122334455/button/1".into()
            )),
            Some(&"true".into())
        );

        let restored = repository
            .update(&device(), &enabled, &defaults, |state| {
                *state.buttons.get_mut(&1).unwrap() = false;
            })
            .unwrap();

        assert_eq!(restored, defaults);
        assert!(repository.storage().snapshot().is_empty());
    }

    #[test]
    fn rollback_failure_is_reported_as_possible_runtime_storage_divergence() {
        let storage = MemoryStore::default();
        storage.fail_on_write(2);
        storage.fail_on_write(3);
        let repository = FeatureStore::new(storage);
        let defaults = defaults();
        let mut changed = defaults.clone();
        changed.dnd = DndMode::Reject;
        changed.privacy = true;

        let error = repository.save(&device(), &changed, &defaults).unwrap_err();

        assert!(matches!(error, FeatureStoreError::Rollback { .. }));
    }
}
