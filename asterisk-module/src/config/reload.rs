//! Pure configuration-diff planning for transactional runtime reloads.
//!
//! [`ReloadPlan::build`] compares two fully normalized snapshots. Added devices
//! do not disturb existing sessions; removed devices disconnect; and a device
//! reconnects only when its own definition, a bound line/profile, or a global
//! station policy changes. MWI changes are keyed by exact line/mailbox pairs and
//! are ordered deterministically.
//!
//! Bind/advertised addresses, keepalive, server identity, listener/TLS policy,
//! ACL/NAT/network policy, QoS, realtime families, and dial-terminator policy
//! are restart-only. The runtime rejects such a reload before applying effects.
//! For an accepted reload it stages configuration, feature overlays, MWI and
//! server reconfiguration, rolls staged resources back on pre-commit failure,
//! and publishes the new snapshot only after all required steps succeed.
//! Existing calls and captured transaction deadlines keep the policy with which
//! they began; unchanged BLF/MWI subscriptions and sessions retain identity.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use sccp_protocol::DeviceId;
use thiserror::Error;

use crate::config::ModuleConfig;

pub(crate) const MAX_RELOAD_ARGUMENTS: usize = 2;
pub(crate) const MAX_RELOAD_ARGUMENT_BYTES: usize = 128;
const MAX_RELOAD_COMPLETIONS: usize = 40;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ReloadSelection {
    Complete,
    Device(DeviceId),
    Line(String),
    SoftKeyProfile(String),
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum ReloadSelectionError {
    #[error("invalid reload selector")]
    InvalidSelector,
    #[error("reload target does not exist")]
    UnknownTarget,
    #[error("complete candidate contains changes outside the selected reload target")]
    InconsistentCandidate,
}

impl ReloadSelection {
    pub(crate) fn parse(arguments: &[&str]) -> Result<Self, ReloadSelectionError> {
        if arguments.len() > MAX_RELOAD_ARGUMENTS
            || arguments.iter().any(|value| !valid_reload_argument(value))
        {
            return Err(ReloadSelectionError::InvalidSelector);
        }
        let [kind, value] = arguments else {
            return if arguments.is_empty() {
                Ok(Self::Complete)
            } else {
                Err(ReloadSelectionError::InvalidSelector)
            };
        };
        if kind.eq_ignore_ascii_case("device") {
            DeviceId::new(*value)
                .map(Self::Device)
                .map_err(|_| ReloadSelectionError::InvalidSelector)
        } else if kind.eq_ignore_ascii_case("line") {
            Ok(Self::Line((*value).to_owned()))
        } else if kind.eq_ignore_ascii_case("profile") {
            Ok(Self::SoftKeyProfile(super::canonical::profile_name(value)))
        } else {
            Err(ReloadSelectionError::InvalidSelector)
        }
    }

    pub(crate) fn validate(
        &self,
        previous: &ModuleConfig,
        next: &ModuleConfig,
        plan: &ReloadPlan,
    ) -> Result<(), ReloadSelectionError> {
        let changes = ConfigurationChanges::between(previous, next);
        match self {
            Self::Complete => Ok(()),
            Self::Device(device) => {
                if !previous.devices.contains_key(device) && !next.devices.contains_key(device) {
                    return Err(ReloadSelectionError::UnknownTarget);
                }
                let exact_object = !changes.general
                    && changes.lines.is_empty()
                    && changes.profiles.is_empty()
                    && changes.devices.iter().all(|changed| changed == device);
                let exact_effects = plan
                    .added
                    .iter()
                    .chain(&plan.changed)
                    .chain(&plan.removed)
                    .all(|changed| changed == device)
                    && plan.mwi_add.is_empty()
                    && plan.mwi_remove.is_empty();
                (exact_object && exact_effects)
                    .then_some(())
                    .ok_or(ReloadSelectionError::InconsistentCandidate)
            }
            Self::Line(line) => {
                if !previous.lines.contains_key(line) && !next.lines.contains_key(line) {
                    return Err(ReloadSelectionError::UnknownTarget);
                }
                let consumers = line_consumers(previous, next, line);
                let exact_object = !changes.general
                    && changes.devices.is_empty()
                    && changes.profiles.is_empty()
                    && changes.lines.iter().all(|changed| changed == line);
                let exact_effects = plan.added.is_empty()
                    && plan.removed.is_empty()
                    && plan.changed.iter().all(|device| consumers.contains(device))
                    && plan
                        .mwi_add
                        .iter()
                        .chain(&plan.mwi_remove)
                        .all(|change| change.line == *line);
                (exact_object && exact_effects)
                    .then_some(())
                    .ok_or(ReloadSelectionError::InconsistentCandidate)
            }
            Self::SoftKeyProfile(profile) => {
                if !previous.soft_key_profiles.contains_key(profile)
                    && !next.soft_key_profiles.contains_key(profile)
                {
                    return Err(ReloadSelectionError::UnknownTarget);
                }
                let consumers = profile_consumers(previous, next, profile);
                let exact_object = !changes.general
                    && changes.devices.is_empty()
                    && changes.lines.is_empty()
                    && changes.profiles.iter().all(|changed| changed == profile);
                let exact_effects = plan.added.is_empty()
                    && plan.removed.is_empty()
                    && plan.changed.iter().all(|device| consumers.contains(device))
                    && plan.mwi_add.is_empty()
                    && plan.mwi_remove.is_empty();
                (exact_object && exact_effects)
                    .then_some(())
                    .ok_or(ReloadSelectionError::InconsistentCandidate)
            }
        }
    }
}

pub(crate) fn complete_reload_selection(
    arguments: &[&str],
    prefix: &str,
    ordinal: usize,
    config: &ModuleConfig,
) -> Option<String> {
    if arguments.iter().any(|value| !valid_reload_argument(value))
        || prefix.len() > MAX_RELOAD_ARGUMENT_BYTES
        || prefix.chars().any(char::is_control)
    {
        return None;
    }
    let candidates = match arguments {
        [] => vec!["device".to_owned(), "line".to_owned(), "profile".to_owned()],
        [kind] if kind.eq_ignore_ascii_case("device") => {
            config.devices.keys().map(ToString::to_string).collect()
        }
        [kind] if kind.eq_ignore_ascii_case("line") => config.lines.keys().cloned().collect(),
        [kind] if kind.eq_ignore_ascii_case("profile") => {
            config.soft_key_profiles.keys().cloned().collect()
        }
        _ => return None,
    };
    let mut candidates = candidates;
    candidates.sort_by_key(|candidate| candidate.to_ascii_lowercase());
    candidates.dedup_by(|left, right| left.eq_ignore_ascii_case(right));
    candidates
        .into_iter()
        .filter(|candidate| valid_reload_argument(candidate))
        .filter(|candidate| starts_with_ignore_ascii_case(candidate, prefix))
        .take(MAX_RELOAD_COMPLETIONS)
        .nth(ordinal)
}

fn valid_reload_argument(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_RELOAD_ARGUMENT_BYTES
        && !value.chars().any(char::is_control)
}

fn starts_with_ignore_ascii_case(value: &str, prefix: &str) -> bool {
    value
        .get(..prefix.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
}

fn line_consumers(previous: &ModuleConfig, next: &ModuleConfig, line: &str) -> BTreeSet<DeviceId> {
    previous
        .appearances_for_line(line)
        .chain(next.appearances_for_line(line))
        .map(|binding| binding.device_id.clone())
        .collect()
}

fn profile_consumers(
    previous: &ModuleConfig,
    next: &ModuleConfig,
    profile: &str,
) -> BTreeSet<DeviceId> {
    previous
        .devices
        .values()
        .chain(next.devices.values())
        .filter(|device| device.soft_key_profile == profile)
        .map(|device| device.id.clone())
        .collect()
}

#[derive(Debug, Default, Eq, PartialEq)]
struct ConfigurationChanges {
    general: bool,
    devices: BTreeSet<DeviceId>,
    lines: BTreeSet<String>,
    profiles: BTreeSet<String>,
}

impl ConfigurationChanges {
    fn between(previous: &ModuleConfig, next: &ModuleConfig) -> Self {
        let mut devices = changed_keys(&previous.devices, &next.devices);
        devices.extend(
            previous
                .device_codec_overrides
                .symmetric_difference(&next.device_codec_overrides)
                .cloned(),
        );
        let mut lines = changed_keys(&previous.lines, &next.lines);
        lines.extend(changed_keys(&previous.line_features, &next.line_features));
        lines.extend(
            previous
                .line_codec_overrides
                .symmetric_difference(&next.line_codec_overrides)
                .cloned(),
        );
        Self {
            general: previous.general != next.general,
            devices,
            lines,
            profiles: changed_keys(&previous.soft_key_profiles, &next.soft_key_profiles),
        }
    }
}

fn changed_keys<K, V>(previous: &HashMap<K, V>, next: &HashMap<K, V>) -> BTreeSet<K>
where
    K: Clone + Eq + std::hash::Hash + Ord,
    V: Eq,
{
    previous
        .keys()
        .chain(next.keys())
        .filter(|key| previous.get(*key) != next.get(*key))
        .cloned()
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RestartRequiredChange {
    ConfigurationSource,
    Bind,
    AdvertisedAddress,
    Keepalive,
    RegistrationFailover,
    ServerName,
    ListenerPolicy,
    NetworkPolicy,
    QosPolicy,
    RealtimeTables,
    DialTerminator,
}

impl RestartRequiredChange {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::ConfigurationSource => "configuration source",
            Self::Bind => "bind",
            Self::AdvertisedAddress => "advertised_address",
            Self::Keepalive => "keepalive",
            Self::RegistrationFailover => "registration failover policy",
            Self::ServerName => "server_name",
            Self::ListenerPolicy => "listener/TLS policy",
            Self::NetworkPolicy => "ACL/NAT/network policy",
            Self::QosPolicy => "QoS policy",
            Self::RealtimeTables => "realtime table selection",
            Self::DialTerminator => "dial terminator policy",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MwiSubscriptionChange {
    pub(crate) line: String,
    pub(crate) mailbox: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ReloadPlan {
    pub(crate) added: Vec<DeviceId>,
    pub(crate) changed: Vec<DeviceId>,
    pub(crate) removed: Vec<DeviceId>,
    pub(crate) mwi_add: Vec<MwiSubscriptionChange>,
    pub(crate) mwi_remove: Vec<MwiSubscriptionChange>,
    pub(crate) restart_required: Vec<RestartRequiredChange>,
}

impl ReloadPlan {
    pub(crate) fn build(previous: &ModuleConfig, next: &ModuleConfig) -> Self {
        let previous_devices: BTreeSet<_> = previous.devices.keys().cloned().collect();
        let next_devices: BTreeSet<_> = next.devices.keys().cloned().collect();
        let added = next_devices
            .difference(&previous_devices)
            .cloned()
            .collect();
        let removed = previous_devices
            .difference(&next_devices)
            .cloned()
            .collect();
        let global_change = station_global_policy_changed(previous, next);
        let changed = previous_devices
            .intersection(&next_devices)
            .filter(|device| global_change || station_configuration_changed(previous, next, device))
            .cloned()
            .collect();
        let (mwi_add, mwi_remove) = mwi_changes(previous, next);
        let restart_required = restart_required_changes(previous, next);
        Self {
            added,
            changed,
            removed,
            mwi_add,
            mwi_remove,
            restart_required,
        }
    }

    pub(crate) fn affected_devices(&self) -> impl Iterator<Item = &DeviceId> {
        self.changed.iter().chain(&self.removed)
    }
}

fn station_configuration_changed(
    previous: &ModuleConfig,
    next: &ModuleConfig,
    device: &DeviceId,
) -> bool {
    let previous_device = previous
        .devices
        .get(device)
        .expect("device exists in both configurations");
    let next_device = next
        .devices
        .get(device)
        .expect("device exists in both configurations");
    // DND schedules are enforced by the module and do not alter the station
    // definition. A schedule-only reload must therefore leave the SCCP session
    // intact while the runtime scheduler reconciles the new calendar policy.
    let mut previous_station = previous_device.clone();
    let mut next_station = next_device.clone();
    previous_station.dnd_schedules.clear();
    next_station.dnd_schedules.clear();
    if previous_station != next_station {
        return true;
    }
    if previous
        .soft_key_profiles
        .get(&previous_device.soft_key_profile)
        != next.soft_key_profiles.get(&next_device.soft_key_profile)
    {
        return true;
    }
    let previous_bindings: Vec<_> = previous.appearances_for_device(device).collect();
    let next_bindings: Vec<_> = next.appearances_for_device(device).collect();
    if previous_bindings != next_bindings {
        return true;
    }
    let lines: BTreeSet<_> = previous_bindings
        .iter()
        .chain(&next_bindings)
        .map(|binding| binding.line.number.as_str())
        .collect();
    lines.into_iter().any(|line| {
        previous.features_for_line(line) != next.features_for_line(line)
            || previous.media_for_line(line) != next.media_for_line(line)
    })
}

fn station_global_policy_changed(previous: &ModuleConfig, next: &ModuleConfig) -> bool {
    previous.general.timing_policy().interdigit_timeout
        != next.general.timing_policy().interdigit_timeout
        || previous.general.simulate_enbloc != next.general.simulate_enbloc
        || previous.general.speed_dial_await_further_digits
            != next.general.speed_dial_await_further_digits
        || previous.general.codecs != next.general.codecs
        || previous.general.conference_dialing != next.general.conference_dialing
        || previous.general.auto_answer != next.general.auto_answer
        || previous.general.direct_media != next.general.direct_media
        || previous.general.early_media != next.general.early_media
        || previous.general.audio_processing != next.general.audio_processing
        || previous.general.jitter_buffer != next.general.jitter_buffer
}

fn restart_required_changes(
    previous: &ModuleConfig,
    next: &ModuleConfig,
) -> Vec<RestartRequiredChange> {
    let mut changes = Vec::new();
    if previous.general.configuration_source != next.general.configuration_source {
        changes.push(RestartRequiredChange::ConfigurationSource);
    }
    if previous.general.bind != next.general.bind {
        changes.push(RestartRequiredChange::Bind);
    }
    if previous.general.advertised_address != next.general.advertised_address {
        changes.push(RestartRequiredChange::AdvertisedAddress);
    }
    if previous.general.timing_policy().keepalive != next.general.timing_policy().keepalive
        || previous.general.timing_policy().secondary_keepalive
            != next.general.timing_policy().secondary_keepalive
    {
        changes.push(RestartRequiredChange::Keepalive);
    }
    if previous.general.fallback_registration != next.general.fallback_registration
        || previous.general.signaling_servers != next.general.signaling_servers
    {
        changes.push(RestartRequiredChange::RegistrationFailover);
    }
    if previous.general.server_name != next.general.server_name {
        changes.push(RestartRequiredChange::ServerName);
    }
    if previous.general.listeners != next.general.listeners {
        changes.push(RestartRequiredChange::ListenerPolicy);
    }
    if previous.general.network != next.general.network {
        changes.push(RestartRequiredChange::NetworkPolicy);
    }
    if previous.general.qos != next.general.qos {
        changes.push(RestartRequiredChange::QosPolicy);
    }
    if previous.general.realtime_tables != next.general.realtime_tables {
        changes.push(RestartRequiredChange::RealtimeTables);
    }
    if previous.general.dial_terminator != next.general.dial_terminator {
        changes.push(RestartRequiredChange::DialTerminator);
    }
    changes
}

fn configured_mailboxes(config: &ModuleConfig) -> BTreeMap<String, String> {
    config
        .lines
        .values()
        .filter_map(|line| {
            line.mailbox
                .as_ref()
                .map(|mailbox| (line.number.clone(), mailbox.clone()))
        })
        .collect()
}

fn mwi_changes(
    previous: &ModuleConfig,
    next: &ModuleConfig,
) -> (Vec<MwiSubscriptionChange>, Vec<MwiSubscriptionChange>) {
    let previous = configured_mailboxes(previous);
    let next = configured_mailboxes(next);
    let add = next
        .iter()
        .filter(|(line, mailbox)| previous.get(*line) != Some(*mailbox))
        .map(|(line, mailbox)| MwiSubscriptionChange {
            line: line.clone(),
            mailbox: mailbox.clone(),
        })
        .collect();
    let remove = previous
        .iter()
        .filter(|(line, mailbox)| next.get(*line) != Some(*mailbox))
        .map(|(line, mailbox)| MwiSubscriptionChange {
            line: line.clone(),
            mailbox: mailbox.clone(),
        })
        .collect();
    (add, remove)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(extra_general: &str, first_line: &str, devices: &str) -> ModuleConfig {
        ModuleConfig::parse(&format!(
            r#"
            [general]
            advertised_address = 192.0.2.10
            {extra_general}

            [1001]
            type = line
            label = Desk one
            mailbox = 1001@default
            {first_line}

            [1002]
            type = line
            label = Desk two
            mailbox = 1002@default

            {devices}
            "#
        ))
        .unwrap()
    }

    fn two_devices(extra_general: &str, first_line: &str) -> ModuleConfig {
        config(
            extra_general,
            first_line,
            r#"
            [SEP001122334455]
            type = device
            line = 1001

            [SEP112233445566]
            type = device
            line = 1002
            "#,
        )
    }

    #[test]
    fn dnd_schedule_only_change_does_not_reconnect_the_station() {
        let previous = two_devices("", "");
        let mut next = previous.clone();
        let device = DeviceId::new("SEP001122334455").unwrap();
        next.devices
            .get_mut(&device)
            .unwrap()
            .dnd_schedules
            .push(crate::config::DndSchedule::parse("22:00-07:00, *, reject").unwrap());

        let plan = ReloadPlan::build(&previous, &next);

        assert!(plan.changed.is_empty());
        assert!(plan.added.is_empty());
        assert!(plan.removed.is_empty());
    }

    fn one_line(mailbox: &str) -> ModuleConfig {
        ModuleConfig::parse(&format!(
            r#"
            [general]
            advertised_address = 192.0.2.10

            [1001]
            type = line
            label = Desk one
            mailbox = {mailbox}

            [SEP001122334455]
            type = device
            line = 1001
            "#
        ))
        .unwrap()
    }

    #[test]
    fn unchanged_configuration_preserves_every_station_and_subscription() {
        let config = two_devices("", "");
        let plan = ReloadPlan::build(&config, &config);

        assert_eq!(plan, ReloadPlan::default());
        assert_eq!(plan.affected_devices().count(), 0);
    }

    #[test]
    fn device_add_change_and_remove_are_classified_without_touching_peers() {
        let previous = two_devices("", "");
        let next = config(
            "",
            "",
            r#"
            [SEP001122334455]
            type = device
            description = Changed desk
            line = 1001

            [SEP223344556677]
            type = device
            line = 1002
            "#,
        );
        let plan = ReloadPlan::build(&previous, &next);

        assert_eq!(plan.added, [DeviceId::new("SEP223344556677").unwrap()]);
        assert_eq!(plan.changed, [DeviceId::new("SEP001122334455").unwrap()]);
        assert_eq!(plan.removed, [DeviceId::new("SEP112233445566").unwrap()]);
    }

    #[test]
    fn recording_button_changes_reconnect_only_the_owning_device() {
        let previous = two_devices("", "");
        let next = config(
            "",
            "",
            r#"
            [SEP001122334455]
            type = device
            line = 1001
            button = feature, Record calls, monitor

            [SEP112233445566]
            type = device
            line = 1002
            "#,
        );

        for plan in [
            ReloadPlan::build(&previous, &next),
            ReloadPlan::build(&next, &previous),
        ] {
            assert_eq!(plan.changed, [DeviceId::new("SEP001122334455").unwrap()]);
            assert!(plan.added.is_empty());
            assert!(plan.removed.is_empty());
            assert!(plan.restart_required.is_empty());
        }
    }

    #[test]
    fn line_change_reconnects_only_appearances_of_that_logical_line() {
        let previous = two_devices("", "context = before");
        let next = two_devices("", "context = after");

        assert_eq!(
            ReloadPlan::build(&previous, &next).changed,
            [DeviceId::new("SEP001122334455").unwrap()]
        );
    }

    #[test]
    fn secondary_dial_tone_change_reconnects_only_its_line_appearances() {
        let previous = two_devices("", "secondary_dialtone_digits = 9");
        let next = two_devices(
            "",
            "secondary_dialtone_digits = 8\nsecondary_dialtone_tone = Recall Dial Tone",
        );

        assert_eq!(
            ReloadPlan::build(&previous, &next).changed,
            [DeviceId::new("SEP001122334455").unwrap()]
        );
    }

    #[test]
    fn shared_line_change_reconnects_every_device_with_that_appearance() {
        let devices = r#"
            [SEP001122334455]
            type = device
            line = 1001

            [SEP112233445566]
            type = device
            line = 1001
            line = 1002
            "#;
        let previous = config("", "context = before", devices);
        let next = config("", "context = after", devices);

        assert_eq!(
            ReloadPlan::build(&previous, &next).changed,
            [
                DeviceId::new("SEP001122334455").unwrap(),
                DeviceId::new("SEP112233445566").unwrap(),
            ]
        );
    }

    #[test]
    fn global_station_policy_change_reconnects_every_existing_device() {
        let previous = two_devices("allow = ulaw", "");
        let next = two_devices("allow = alaw", "");

        assert_eq!(
            ReloadPlan::build(&previous, &next).changed,
            [
                DeviceId::new("SEP001122334455").unwrap(),
                DeviceId::new("SEP112233445566").unwrap(),
            ]
        );
    }

    #[test]
    fn guest_hotline_policy_change_does_not_reconnect_configured_devices() {
        let previous = two_devices("hotline_enabled = no", "");
        let next = two_devices(
            "hotline_enabled = yes\nhotline_extension = 9911\nhotline_context = guests\nhotline_label = Lobby",
            "",
        );

        let plan = ReloadPlan::build(&previous, &next);

        assert!(plan.added.is_empty());
        assert!(plan.changed.is_empty());
        assert!(plan.removed.is_empty());
        assert!(plan.restart_required.is_empty());
        assert_eq!(plan.affected_devices().count(), 0);
    }

    #[test]
    fn audio_processing_changes_reconnect_exact_consumers() {
        let previous = two_devices("echocancel = yes", "silencesuppression = no");
        let global = two_devices("echocancel = no", "silencesuppression = no");
        assert_eq!(
            ReloadPlan::build(&previous, &global).changed,
            [
                DeviceId::new("SEP001122334455").unwrap(),
                DeviceId::new("SEP112233445566").unwrap(),
            ]
        );

        let line = two_devices("echocancel = yes", "silencesuppression = yes");
        assert_eq!(
            ReloadPlan::build(&previous, &line).changed,
            [DeviceId::new("SEP001122334455").unwrap()]
        );
    }

    #[test]
    fn jitter_buffer_change_reconnects_every_existing_device() {
        let previous = two_devices("jbenable = no", "");
        let next = two_devices(
            "jbenable = yes\njbforce = yes\njbmaxsize = 320\njbimpl = adaptive",
            "",
        );

        assert_eq!(
            ReloadPlan::build(&previous, &next).changed,
            [
                DeviceId::new("SEP001122334455").unwrap(),
                DeviceId::new("SEP112233445566").unwrap(),
            ]
        );
    }

    #[test]
    fn speed_dial_further_digit_policy_change_reconnects_existing_devices() {
        let previous = two_devices("SpeedDialAwaitFurtherDigits = no", "");
        let next = two_devices("SpeedDialAwaitFurtherDigits = yes", "");

        assert_eq!(
            ReloadPlan::build(&previous, &next).changed,
            [
                DeviceId::new("SEP001122334455").unwrap(),
                DeviceId::new("SEP112233445566").unwrap(),
            ]
        );
    }

    #[test]
    fn overlap_policy_change_reconnects_only_devices_with_a_resolved_change() {
        let previous = two_devices("", "");
        let next = config(
            "allowoverlap = yes",
            "",
            r#"
            [SEP001122334455]
            type = device
            allowoverlap = no
            line = 1001

            [SEP112233445566]
            type = device
            line = 1002
            "#,
        );

        assert_eq!(
            ReloadPlan::build(&previous, &next).changed,
            [DeviceId::new("SEP112233445566").unwrap()]
        );
    }

    #[test]
    fn resolved_soft_key_profile_change_reconnects_only_its_consumers() {
        let previous = config(
            "",
            "",
            r#"
            [other]
            type = softkey_profile
            on_hook = new_call

            [SEP001122334455]
            type = device
            line = 1001

            [SEP112233445566]
            type = device
            softkey_profile = other
            line = 1002
            "#,
        );
        let next = config(
            "",
            "",
            r#"
            [default]
            type = softkey_profile
            on_hook = redial

            [other]
            type = softkey_profile
            on_hook = new_call

            [SEP001122334455]
            type = device
            line = 1001

            [SEP112233445566]
            type = device
            softkey_profile = other
            line = 1002
            "#,
        );

        assert_eq!(
            ReloadPlan::build(&previous, &next).changed,
            [DeviceId::new("SEP001122334455").unwrap()]
        );
    }

    #[test]
    fn mwi_plan_preserves_unchanged_and_replaces_only_changed_mailboxes() {
        let mut previous = two_devices("", "");
        let mut unchanged = previous.lines["1002"].clone();
        unchanged.number = "1004".into();
        unchanged.label = "Desk four".into();
        unchanged.mailbox = Some("1004@default".into());
        previous.lines.insert(unchanged.number.clone(), unchanged);
        let mut next = previous.clone();
        next.lines.get_mut("1001").unwrap().mailbox = Some("1001@new-context".into());
        next.lines.get_mut("1002").unwrap().mailbox = None;
        let mut added = next.lines["1002"].clone();
        added.number = "1003".into();
        added.label = "Desk three".into();
        added.mailbox = Some("1003@default".into());
        next.lines.insert(added.number.clone(), added);
        let plan = ReloadPlan::build(&previous, &next);

        assert_eq!(
            plan.mwi_remove,
            [
                MwiSubscriptionChange {
                    line: "1001".into(),
                    mailbox: "1001@default".into(),
                },
                MwiSubscriptionChange {
                    line: "1002".into(),
                    mailbox: "1002@default".into(),
                },
            ]
        );
        assert_eq!(
            plan.mwi_add,
            [
                MwiSubscriptionChange {
                    line: "1001".into(),
                    mailbox: "1001@new-context".into(),
                },
                MwiSubscriptionChange {
                    line: "1003".into(),
                    mailbox: "1003@default".into(),
                },
            ]
        );
    }

    #[test]
    fn listener_identity_changes_are_rejected_before_runtime_mutation() {
        let previous = two_devices("server_name = Before", "");
        let next = two_devices("server_name = After", "");

        assert_eq!(
            ReloadPlan::build(&previous, &next).restart_required,
            [RestartRequiredChange::ServerName]
        );
        assert_eq!(RestartRequiredChange::ServerName.name(), "server_name");
    }

    #[test]
    fn configuration_source_changes_require_a_module_restart() {
        let previous = two_devices("", "");
        let mut next = previous.clone();
        next.general.configuration_source = crate::config::ConfigurationSource::Sorcery;

        assert_eq!(
            ReloadPlan::build(&previous, &next).restart_required,
            [RestartRequiredChange::ConfigurationSource]
        );
        assert_eq!(
            RestartRequiredChange::ConfigurationSource.name(),
            "configuration source"
        );
    }

    #[test]
    fn failover_timing_and_routes_require_a_fresh_listener_generation() {
        let previous = two_devices("", "");
        let keepalive = two_devices("secondary_keepalive = 45", "");
        assert_eq!(
            ReloadPlan::build(&previous, &keepalive).restart_required,
            [RestartRequiredChange::Keepalive]
        );

        for settings in [
            "fallback = yes",
            "signaling_server = 1, primary, 192.0.2.10, 2000, 2443",
        ] {
            assert_eq!(
                ReloadPlan::build(&previous, &two_devices(settings, "")).restart_required,
                [RestartRequiredChange::RegistrationFailover]
            );
        }
        assert_eq!(
            RestartRequiredChange::RegistrationFailover.name(),
            "registration failover policy"
        );
    }

    #[test]
    fn nat_address_policy_changes_require_a_fresh_listener_generation() {
        let previous = two_devices("nat = off", "");
        let next = two_devices(
            "nat = auto\nlocalnet = 10.0.0.0/8\nexternip = 203.0.113.10",
            "",
        );

        assert_eq!(
            ReloadPlan::build(&previous, &next).restart_required,
            [RestartRequiredChange::NetworkPolicy]
        );
        assert_eq!(
            RestartRequiredChange::NetworkPolicy.name(),
            "ACL/NAT/network policy"
        );
    }

    #[test]
    fn dial_terminator_policy_change_requires_one_consistent_session_generation() {
        let previous = two_devices("", "");
        let next = two_devices("digittimeoutchar = *\nrecorddigittimeoutchar = yes", "");

        assert_eq!(
            ReloadPlan::build(&previous, &next).restart_required,
            [RestartRequiredChange::DialTerminator]
        );
        assert_eq!(
            RestartRequiredChange::DialTerminator.name(),
            "dial terminator policy"
        );
    }

    #[test]
    fn reload_selection_parser_is_exact_and_bounded() {
        assert_eq!(ReloadSelection::parse(&[]), Ok(ReloadSelection::Complete));
        assert_eq!(
            ReloadSelection::parse(&["DEVICE", "SEP001122334455"]),
            Ok(ReloadSelection::Device(
                DeviceId::new("SEP001122334455").unwrap()
            ))
        );
        assert_eq!(
            ReloadSelection::parse(&["line", "1001"]),
            Ok(ReloadSelection::Line("1001".into()))
        );
        assert_eq!(
            ReloadSelection::parse(&["profile", "Desk Keys"]),
            Ok(ReloadSelection::SoftKeyProfile("desk keys".into()))
        );
        for arguments in [
            vec!["device"],
            vec!["unknown", "1001"],
            vec!["line", "1001", "extra"],
            vec!["line", "bad\nline"],
        ] {
            assert_eq!(
                ReloadSelection::parse(&arguments),
                Err(ReloadSelectionError::InvalidSelector)
            );
        }
        let oversized = "x".repeat(MAX_RELOAD_ARGUMENT_BYTES + 1);
        assert_eq!(
            ReloadSelection::parse(&["line", &oversized]),
            Err(ReloadSelectionError::InvalidSelector)
        );
    }

    #[test]
    fn reload_completion_filters_the_complete_sorted_candidate_set() {
        let mut profiles = String::new();
        for index in 0..45 {
            profiles.push_str(&format!(
                "[profile{index:02}]\ntype = softkey_profile\non_hook = new_call\n"
            ));
        }
        profiles.push_str("[zz-late]\ntype = softkey_profile\non_hook = redial\n");
        let candidate = config(
            "",
            "",
            &format!(
                r#"
            {profiles}

            [SEP001122334455]
            type = device
            line = 1001

            [SEP112233445566]
            type = device
            line = 1002
            "#
            ),
        );

        assert_eq!(
            complete_reload_selection(&[], "p", 0, &candidate).as_deref(),
            Some("profile")
        );
        assert_eq!(
            complete_reload_selection(&["profile"], "ZZ", 0, &candidate).as_deref(),
            Some("zz-late")
        );
        assert_eq!(
            complete_reload_selection(&["line"], "10", 1, &candidate).as_deref(),
            Some("1002")
        );
        assert_eq!(
            complete_reload_selection(&["profile"], "bad\n", 0, &candidate),
            None
        );
    }

    #[test]
    fn targeted_device_rejects_every_collateral_object_change() {
        let previous = two_devices("", "");
        let mut next = previous.clone();
        let target = DeviceId::new("SEP001122334455").unwrap();
        next.devices.get_mut(&target).unwrap().description = "New desk".into();
        let plan = ReloadPlan::build(&previous, &next);
        let selection = ReloadSelection::Device(target.clone());
        assert_eq!(selection.validate(&previous, &next, &plan), Ok(()));

        next.lines.get_mut("1002").unwrap().label = "Collateral".into();
        let plan = ReloadPlan::build(&previous, &next);
        assert_eq!(
            selection.validate(&previous, &next, &plan),
            Err(ReloadSelectionError::InconsistentCandidate)
        );

        assert_eq!(
            ReloadSelection::Device(DeviceId::new("SEP998877665544").unwrap()).validate(
                &previous,
                &previous,
                &ReloadPlan::default()
            ),
            Err(ReloadSelectionError::UnknownTarget)
        );
    }

    #[test]
    fn targeted_device_tracks_explicit_override_provenance() {
        let previous = config(
            "allow = ulaw",
            "",
            r#"
            [SEP001122334455]
            type = device
            line = 1001

            [SEP112233445566]
            type = device
            line = 1002
            "#,
        );
        let next = config(
            "allow = ulaw",
            "",
            r#"
            [SEP001122334455]
            type = device
            allow = ulaw
            line = 1001

            [SEP112233445566]
            type = device
            line = 1002
            "#,
        );
        let plan = ReloadPlan::build(&previous, &next);
        assert_eq!(plan, ReloadPlan::default());
        assert_eq!(
            ReloadSelection::Device(DeviceId::new("SEP001122334455").unwrap())
                .validate(&previous, &next, &plan),
            Ok(())
        );
        assert_eq!(
            ReloadSelection::Device(DeviceId::new("SEP112233445566").unwrap())
                .validate(&previous, &next, &plan),
            Err(ReloadSelectionError::InconsistentCandidate)
        );
    }

    #[test]
    fn targeted_line_allows_only_its_complete_runtime_dependency_set() {
        let devices = r#"
            [SEP001122334455]
            type = device
            line = 1001

            [SEP112233445566]
            type = device
            line = 1001
            line = 1002
            "#;
        let previous = config("", "context = before", devices);
        let next = config("", "context = after", devices);
        let plan = ReloadPlan::build(&previous, &next);
        assert_eq!(
            ReloadSelection::Line("1001".into()).validate(&previous, &next, &plan),
            Ok(())
        );
        assert_eq!(
            plan.changed,
            [
                DeviceId::new("SEP001122334455").unwrap(),
                DeviceId::new("SEP112233445566").unwrap(),
            ]
        );

        let collateral = config("allow = alaw", "context = after", devices);
        let collateral_plan = ReloadPlan::build(&previous, &collateral);
        assert_eq!(
            ReloadSelection::Line("1001".into()).validate(&previous, &collateral, &collateral_plan),
            Err(ReloadSelectionError::InconsistentCandidate)
        );
    }

    #[test]
    fn targeted_line_carries_its_mwi_replacement_through_the_shared_plan() {
        let previous = one_line("1001@before");
        let next = one_line("1001@after");
        let plan = ReloadPlan::build(&previous, &next);
        assert_eq!(
            ReloadSelection::Line("1001".into()).validate(&previous, &next, &plan),
            Ok(())
        );
        assert_eq!(plan.mwi_remove[0].line, "1001");
        assert_eq!(plan.mwi_add[0].line, "1001");
    }

    #[test]
    fn targeted_profile_reconnects_consumers_without_authorizing_device_changes() {
        let previous = config(
            "",
            "",
            r#"
            [desk]
            type = softkey_profile
            on_hook = new_call

            [SEP001122334455]
            type = device
            softkey_profile = desk
            line = 1001

            [SEP112233445566]
            type = device
            line = 1002
            "#,
        );
        let next = config(
            "",
            "",
            r#"
            [desk]
            type = softkey_profile
            on_hook = redial

            [SEP001122334455]
            type = device
            softkey_profile = desk
            line = 1001

            [SEP112233445566]
            type = device
            line = 1002
            "#,
        );
        let plan = ReloadPlan::build(&previous, &next);
        assert_eq!(
            ReloadSelection::SoftKeyProfile("desk".into()).validate(&previous, &next, &plan),
            Ok(())
        );
        assert_eq!(plan.changed, [DeviceId::new("SEP001122334455").unwrap()]);

        let mut collateral = next;
        collateral
            .devices
            .get_mut(&DeviceId::new("SEP001122334455").unwrap())
            .unwrap()
            .description = "Collateral".into();
        let collateral_plan = ReloadPlan::build(&previous, &collateral);
        assert_eq!(
            ReloadSelection::SoftKeyProfile("desk".into()).validate(
                &previous,
                &collateral,
                &collateral_plan
            ),
            Err(ReloadSelectionError::InconsistentCandidate)
        );
    }
}
