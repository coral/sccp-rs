use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Weak;

use serde_json::{Value, json};

use crate::asterisk::boundary::{MutexExt as _, RwLockExt as _};
use crate::asterisk::runtime::Shared;
use crate::call::metadata::{CallMetadata, ChannelVariable};
use crate::config::{
    DeviceConfig, DeviceFeatureDefaults, GeneralConfig, GuestHotlineConfig, LineConfig,
    LineFeatureConfig, ModuleConfig, TlsCredentials, TlsListener,
};
use crate::pbx::party::PartyIdentity;
use crate::runtime::controller::{CallSnapshot, RegisteredDevice};

use super::capture::PacketCaptureSnapshot;
use super::{LogEntry, bounded_text};

const MAX_REPORT_BODY_BYTES: usize = 900 * 1024;
const MAX_SNAPSHOT_ITEMS: usize = 64;
const MAX_SNAPSHOT_TEXT_BYTES: usize = 4 * 1024;

pub(super) fn diagnostic_body(
    trigger: &LogEntry,
    recent_logs: &[LogEntry],
    shared: &Weak<Shared>,
    packet_capture_event_id: Option<&str>,
) -> Vec<u8> {
    let state = shared.upgrade().map(|shared| state_snapshot(&shared));
    let complete = json!({
        "schema": "chan_sccp2.debug.diagnostic.v1",
        "trigger": trigger,
        "recent_module_logs": recent_logs,
        "state": state,
        "packet_capture_event_id": packet_capture_event_id,
    });
    let body = serde_json::to_vec(&complete).unwrap_or_default();
    if body.len() <= MAX_REPORT_BODY_BYTES {
        return body;
    }
    let mut bounded_logs = recent_logs.to_vec();
    loop {
        let body = serde_json::to_vec(&json!({
            "schema": "chan_sccp2.debug.diagnostic.v1",
            "trigger": trigger,
            "recent_module_logs": bounded_logs,
            "state_omitted": "report exceeded the local telemetry bound",
            "packet_capture_event_id": packet_capture_event_id,
        }))
        .unwrap_or_default();
        if body.len() <= MAX_REPORT_BODY_BYTES || bounded_logs.is_empty() {
            return body;
        }
        bounded_logs.remove(0);
    }
}

pub(super) fn packet_body(capture: &PacketCaptureSnapshot, diagnostic_event_id: &str) -> Vec<u8> {
    let mut bounded_capture = capture.clone();
    loop {
        let body = serde_json::to_vec(&json!({
            "schema": "chan_sccp2.debug.signaling.v1",
            "capture_layer": "application_decrypted_signaling",
            "rtp_payloads_included": false,
            "diagnostic_event_id": diagnostic_event_id,
            "capture": bounded_capture,
        }))
        .unwrap_or_default();
        if body.len() <= MAX_REPORT_BODY_BYTES || !bounded_capture.discard_oldest() {
            return body;
        }
    }
}

fn state_snapshot(shared: &Shared) -> Value {
    let configuration = config_snapshot(&shared.config.read_unpoisoned());
    let (devices, registered_devices_total, calls, calls_total) = {
        let controller = shared.controller.snapshot();
        let mut devices = BTreeMap::new();
        let mut registered_devices_total = 0_usize;
        for (device_id, device) in controller.registered_devices() {
            registered_devices_total = registered_devices_total.saturating_add(1);
            retain_smallest(
                &mut devices,
                device_id.clone(),
                device.clone(),
                MAX_SNAPSHOT_ITEMS,
            );
        }
        let mut calls = BTreeMap::new();
        let mut calls_total = 0_usize;
        for call in controller.calls() {
            calls_total = calls_total.saturating_add(1);
            retain_smallest(
                &mut calls,
                (call.pbx_id.get(), call.sccp_id.get()),
                call,
                MAX_SNAPSHOT_ITEMS,
            );
        }
        (devices, registered_devices_total, calls, calls_total)
    };
    let devices = devices
        .iter()
        .map(|(device_id, device)| registered_device_snapshot(device_id.as_str(), device))
        .collect::<Vec<_>>();
    let calls = calls.values().map(call_snapshot).collect::<Vec<_>>();
    let controller = shared.controller.snapshot();
    json!({
        "configuration": configuration,
        "registered_devices": devices,
        "registered_devices_total": registered_devices_total,
        "registered_devices_truncated": registered_devices_total.saturating_sub(devices.len()),
        "calls": calls,
        "calls_total": calls_total,
        "calls_truncated": calls_total.saturating_sub(calls.len()),
        "native_channels": shared.channels.lock_unpoisoned().len(),
        "assigned_channel_ids": sorted_debug_map(&controller.call_runtime_records().filter_map(|(id, record)| record.assigned_channel_id.as_ref().map(|value| (*id, value))).collect()),
        "audio_packet_ms": sorted_debug_map(&controller.call_runtime_records().filter_map(|(id, record)| record.audio_packet_ms.as_ref().map(|value| (*id, value))).collect()),
        "audio_preferences": sorted_debug_map(&controller.call_runtime_records().filter_map(|(id, record)| record.audio_preferences.as_ref().map(|value| (*id, value))).collect()),
        "bridges": shared.bridge_snapshot().0.iter().map(|bridge| format!("{bridge:?}")).collect::<Vec<_>>(),
        "barge_bridges": shared.bridge_snapshot().1.iter().map(|bridge| format!("{bridge:?}")).collect::<Vec<_>>(),
        "forwarded_calls": sorted_debug_map(&controller.call_runtime_records().filter_map(|(id, record)| record.forwarding.as_ref().map(|value| (*id, value))).collect()),
        "no_answer_plans": sorted_debug_map(&controller.call_runtime_records().filter_map(|(id, record)| record.no_answer.as_ref().map(|value| (*id, value))).collect()),
        "pending_parks": sorted_debug_map(controller.pending_parks()),
        "pending_retrievals": sorted_debug_map(controller.pending_retrievals()),
    })
}

fn registered_device_snapshot(device_id: &str, device: &RegisteredDevice) -> Value {
    json!({
        "device_id": device_id,
        "session_generation": device.session_generation.get(),
        "registration": {
            "peer": device.registration.peer.to_string(),
            "transport": format!("{:?}", device.registration.transport),
            "reported_ipv4_address": device.registration.reported_address,
            "reported_ipv6_address": device.registration.reported_ipv6_address,
            "device_type": format!("{:?}", device.registration.device_type),
            "protocol": format!("{:?}", device.registration.protocol),
            "firmware": device.registration.firmware,
        },
        "capabilities": format!("{:?}", device.capabilities),
        "audio_encryption": format!("{:?}", device.audio_encryption),
        "selected_line": device.selected_line,
    })
}

fn config_snapshot(config: &ModuleConfig) -> Value {
    let devices = sorted_entries(&config.devices, device_snapshot);
    let lines = sorted_entries(&config.lines, line_snapshot);
    let line_features = sorted_entries(&config.line_features, line_feature_snapshot);
    let soft_key_profiles = sorted_entries(&config.soft_key_profiles, |profile| {
        json!({
            "name": profile.name,
            "sets": sorted_debug_map(&profile.sets),
        })
    });
    json!({
        "general": general_snapshot(&config.general),
        "devices": devices,
        "lines": lines,
        "line_features": line_features,
        "soft_key_profiles": soft_key_profiles,
    })
}

fn general_snapshot(general: &GeneralConfig) -> Value {
    json!({
        "configuration_source": format!("{:?}", general.configuration_source),
        "bind": general.bind.to_string(),
        "advertised_address": general.advertised_address,
        "server_name": general.server_name,
        "language": general.language,
        "account_code": general.account_code,
        "keepalive_seconds": general.keepalive_seconds,
        "secondary_keepalive_seconds": general.secondary_keepalive_seconds,
        "signaling_servers": format!("{:?}", general.signaling_servers),
        "first_digit_timeout_ms": general.first_digit_timeout_ms,
        "interdigit_timeout_ms": general.interdigit_timeout_ms,
        "dial_terminator": format!("{:?}", general.dial_terminator),
        "simulate_enbloc": general.simulate_enbloc,
        "speed_dial_await_further_digits": general.speed_dial_await_further_digits,
        "allow_overlap": general.allow_overlap,
        "transfer_on_hangup": general.transfer_on_hangup,
        "call_answer_order": format!("{:?}", general.call_answer_order),
        "timezone_offset_minutes": general.timezone_offset_minutes,
        "timezone": general.timezone.map(|zone| zone.name()),
        "date_template": format!("{:?}", general.date_template),
        "ring_type": format!("{:?}", general.ring_type),
        "call_waiting_tone": format!("{:?}", general.call_waiting_tone),
        "call_waiting_interval_seconds": general.call_waiting_interval_seconds,
        "codecs": format!("{:?}", general.codecs),
        "audio_encryption": format!("{:?}", general.audio_encryption),
        "conference_dialing": format!("{:?}", general.conference_dialing),
        "auto_answer": format!("{:?}", general.auto_answer),
        "remote_hangup_tone": format!("{:?}", general.remote_hangup_tone),
        "guest_hotline": guest_hotline_snapshot(&general.guest_hotline),
        "direct_media": general.direct_media,
        "early_media": general.early_media,
        "audio_processing": format!("{:?}", general.audio_processing),
        "jitter_buffer": format!("{:?}", general.jitter_buffer),
        "registration": format!("{:?}", general.registration),
        "fallback_registration": format!("{:?}", general.fallback_registration),
        "network": format!("{:?}", general.network),
        "qos": format!("{:?}", general.qos),
        "listeners": listener_snapshot(&general.listeners.tls, general.listeners.clear),
        "realtime_tables": format!("{:?}", general.realtime_tables),
    })
}

fn listener_snapshot(tls: &Option<TlsListener>, clear: std::net::SocketAddr) -> Value {
    let tls = tls.as_ref().map(|listener| {
        let credentials = match &listener.credentials {
            TlsCredentials::CombinedPem(path) => json!({
                "kind": "combined_pem",
                "path": path_value(path),
                "credential_contents_included": false,
            }),
            TlsCredentials::SplitPem {
                certificate,
                private_key,
                trust_store,
            } => json!({
                "kind": "split_pem",
                "certificate_path": path_value(certificate),
                "private_key_path": path_value(private_key),
                "trust_store_path": trust_store.as_deref().map(path_value),
                "credential_contents_included": false,
            }),
        };
        json!({
            "bind": listener.bind.to_string(),
            "credentials": credentials,
        })
    });
    json!({
        "clear": clear.to_string(),
        "tls": tls,
    })
}

fn device_snapshot(device: &DeviceConfig) -> Value {
    let feature_arguments = sorted_entries(&device.feature_arguments, |value| {
        Value::String(bounded_text(value, MAX_SNAPSHOT_TEXT_BYTES))
    });
    let blf_targets = bounded_string_entries(&device.blf_targets, ToString::to_string);
    json!({
        "id": device.id.as_str(),
        "description": device.description,
        "lines": device.lines,
        "buttons": format!("{:?}", device.buttons),
        "feature_arguments": feature_arguments,
        "blf_targets": blf_targets,
        "channel_variables": variables_snapshot(&device.channel_variables),
        "soft_key_profile": device.soft_key_profile,
        "feature_defaults": device_feature_defaults_snapshot(&device.feature_defaults),
        "background_configured": device.background.is_some(),
        "parking": format!("{:?}", device.parking),
        "conference": format!("{:?}", device.conference),
        "call_ui": format!("{:?}", device.call_ui),
        "allow_overlap": device.allow_overlap,
        "media": format!("{:?}", device.media),
        "network": format!("{:?}", device.network),
    })
}

fn guest_hotline_snapshot(hotline: &GuestHotlineConfig) -> Value {
    json!({
        "enabled": hotline.enabled,
        "extension": hotline.extension.as_ref().map(|destination| destination.as_str()),
        "context": hotline.context,
        "label": hotline.label,
    })
}

fn device_feature_defaults_snapshot(defaults: &DeviceFeatureDefaults) -> Value {
    json!({
        "forwarding": {
            "all_enabled": defaults.forwarding.all_enabled,
            "busy_enabled": defaults.forwarding.busy_enabled,
            "no_answer_enabled": defaults.forwarding.no_answer_enabled,
            "no_answer_timeout_seconds": defaults.forwarding.no_answer_timeout_seconds,
            "all": defaults.forwarding.all.as_ref().map(|destination| destination.as_str()),
            "busy": defaults.forwarding.busy.as_ref().map(|destination| destination.as_str()),
            "no_answer": defaults.forwarding.no_answer.as_ref().map(|destination| destination.as_str()),
        },
        "dnd_enabled": defaults.dnd_enabled,
        "dnd": format!("{:?}", defaults.dnd),
        "privacy_enabled": defaults.privacy_enabled,
        "privacy": defaults.privacy,
        "buttons": sorted_debug_map(&defaults.buttons),
    })
}

fn line_feature_snapshot(features: &LineFeatureConfig) -> Value {
    let registration_extensions = features
        .registration
        .extensions
        .iter()
        .map(|extension| {
            json!({
                "extension": extension.extension,
                "context": extension.context,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "incoming_limit": features.incoming_limit,
        "voicemail": {
            "number": features.voicemail.number.as_ref().map(|destination| destination.as_str()),
            "transfer_destination": features.voicemail.transfer_destination.as_ref().map(|destination| destination.as_str()),
        },
        "pickup": {
            "call_groups": features.pickup.call_groups,
            "pickup_groups": features.pickup.pickup_groups,
            "named_call_groups": features.pickup.named_call_groups,
            "named_pickup_groups": features.pickup.named_pickup_groups,
            "directed": features.pickup.directed,
            "directed_context": features.pickup.directed_context,
            "answer_directed": features.pickup.answer_directed,
        },
        "parking_lot": features.parking.lot,
        "conference": {
            "enabled": features.conference.enabled,
            "destination": features.conference.destination,
            "application_options": features.conference.application_options,
        },
        "hotline_destination": features.hotline.destination.as_ref().map(|destination| destination.as_str()),
        "dial_tones": format!("{:?}", features.dial_tones),
        "mobility_pin": features.mobility.pin.as_ref().map(|pin| json!({
            "redacted": true,
            "digits": pin.digits(),
        })),
        "registration_extensions": registration_extensions,
        "media": format!("{:?}", features.media),
    })
}

fn line_snapshot(line: &LineConfig) -> Value {
    json!({
        "number": line.number,
        "label": line.label,
        "context": line.context,
        "caller_name": line.caller_name,
        "caller_number": line.caller_number,
        "mailbox": line.mailbox,
        "language": line.language,
        "account_code": line.account_code,
        "channel_variables": variables_snapshot(&line.channel_variables),
    })
}

fn call_snapshot(call: &CallSnapshot) -> Value {
    json!({
        "sccp_call_id": call.sccp_id.get(),
        "pbx_call_id": call.pbx_id.get(),
        "device_id": call.device_id.as_str(),
        "line_instance": call.line_instance,
        "line": call.line,
        "direction": format!("{:?}", call.direction),
        "state": format!("{:?}", call.state),
        "digits": call.digits,
        "call_info": {
            "direction": format!("{:?}", call.info.direction),
            "calling_name": call.info.calling_name,
            "calling_number": call.info.calling_number,
            "called_name": call.info.called_name,
            "called_number": call.info.called_number,
            "original_called_name": call.info.original_called_name,
            "original_called_number": call.info.original_called_number,
            "last_redirecting_name": call.info.last_redirecting_name,
            "last_redirecting_number": call.info.last_redirecting_number,
            "original_redirect_reason": call.info.original_redirect_reason,
            "last_redirect_reason": call.info.last_redirect_reason,
            "party_restrictions": call.info.party_restrictions,
        },
        "metadata": metadata_snapshot(&call.metadata),
        "codec": format!("{:?}", call.codec),
        "audio": format!("{:?}", call.audio),
        "audio_transmit": format!("{:?}", call.audio_transmit),
        "video": format!("{:?}", call.video),
    })
}

fn metadata_snapshot(metadata: &CallMetadata) -> Value {
    json!({
        "ani": party_snapshot(&metadata.ani),
        "dnid": metadata.dnid,
        "dnid_plan": format!("{:?}", metadata.dnid_plan),
        "rdnis": party_snapshot(&metadata.rdnis),
        "account_code": metadata.account_code,
        "language": metadata.language,
        "variables": variables_snapshot(&metadata.variables),
    })
}

fn party_snapshot(party: &PartyIdentity) -> Value {
    json!({
        "name": party.name,
        "number": party.number,
        "name_charset": format!("{:?}", party.name_charset),
        "name_presentation": party.name_presentation.raw(),
        "number_plan": party.number_plan.raw(),
        "number_presentation": party.number_presentation.raw(),
    })
}

fn variables_snapshot(variables: &[ChannelVariable]) -> Vec<Value> {
    variables
        .iter()
        .map(|variable| {
            json!({
                "name": variable.name(),
                "value_bytes": variable.value().len(),
                "value_included": false,
            })
        })
        .collect()
}

fn sorted_entries<Key, Entry>(
    values: &std::collections::HashMap<Key, Entry>,
    snapshot: impl Fn(&Entry) -> Value,
) -> Value
where
    Key: std::fmt::Display,
{
    let mut selected = BTreeMap::new();
    for (key, value) in values {
        retain_smallest(&mut selected, key.to_string(), value, MAX_SNAPSHOT_ITEMS);
    }
    let items = selected
        .into_iter()
        .map(|(key, value)| (key, snapshot(value)))
        .collect();
    bounded_items(values.len(), items)
}

fn bounded_string_entries<Key, Entry>(
    values: &std::collections::HashMap<Key, Entry>,
    render: impl Fn(&Entry) -> String,
) -> Value
where
    Key: std::fmt::Display,
{
    let mut selected = BTreeMap::new();
    for (key, value) in values {
        retain_smallest(&mut selected, key.to_string(), value, MAX_SNAPSHOT_ITEMS);
    }
    let items = selected
        .into_iter()
        .map(|(key, value)| {
            let rendered = render(value);
            (
                key,
                Value::String(bounded_text(&rendered, MAX_SNAPSHOT_TEXT_BYTES)),
            )
        })
        .collect();
    bounded_items(values.len(), items)
}

fn sorted_debug_map<Key, Entry>(values: &std::collections::HashMap<Key, Entry>) -> Value
where
    Key: std::fmt::Debug,
    Entry: std::fmt::Debug,
{
    let mut items = std::collections::BTreeSet::new();
    for (key, value) in values {
        retain_smallest_string(&mut items, format!("{key:?}={value:?}"));
    }
    bounded_strings(values.len(), items)
}

fn retain_smallest<Key, Entry>(
    values: &mut BTreeMap<Key, Entry>,
    key: Key,
    value: Entry,
    maximum_items: usize,
) where
    Key: Ord,
{
    if values.len() < maximum_items
        || values
            .last_key_value()
            .is_some_and(|(largest, _)| key < *largest)
    {
        if values.len() == maximum_items {
            values.pop_last();
        }
        values.insert(key, value);
    }
}

fn bounded_items(total: usize, items: BTreeMap<String, Value>) -> Value {
    json!({
        "total": total,
        "truncated": total.saturating_sub(items.len()),
        "items": items,
    })
}

fn retain_smallest_string(values: &mut std::collections::BTreeSet<String>, value: String) {
    if values.len() < MAX_SNAPSHOT_ITEMS || values.last().is_some_and(|largest| value < *largest) {
        if values.len() == MAX_SNAPSHOT_ITEMS {
            values.pop_last();
        }
        values.insert(value);
    }
}

fn bounded_strings(total: usize, items: std::collections::BTreeSet<String>) -> Value {
    json!({
        "total": total,
        "truncated": total.saturating_sub(items.len()),
        "items": items,
    })
}

fn path_value(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arbitrary_channel_variable_values_are_never_included() {
        let variable = ChannelVariable::new("PUBLIC_VALUE", "could-be-a-credential").unwrap();
        let snapshot = serde_json::to_string(&variables_snapshot(&[variable])).unwrap();
        assert!(snapshot.contains("PUBLIC_VALUE"));
        assert!(!snapshot.contains("could-be-a-credential"));
    }

    #[test]
    fn sorted_entries_retain_a_deterministic_bounded_key_set() {
        let values = (0..MAX_SNAPSHOT_ITEMS + 8)
            .map(|value| (value, value))
            .collect::<std::collections::HashMap<_, _>>();
        let snapshot = sorted_entries(&values, |value| json!(value));
        assert_eq!(snapshot["total"], MAX_SNAPSHOT_ITEMS + 8);
        assert_eq!(snapshot["truncated"], 8);
        let actual = snapshot["items"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let expected = values
            .keys()
            .map(ToString::to_string)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .take(MAX_SNAPSHOT_ITEMS)
            .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn worst_case_log_escaping_stays_below_the_local_body_bound() {
        let log = LogEntry {
            observed_at_unix_ms: 1,
            level: "warning",
            message: "\\\"\n".repeat(2730),
        };
        let recent_logs = std::iter::repeat_n(log.clone(), 128).collect::<Vec<_>>();
        let body = diagnostic_body(&log, &recent_logs, &Weak::new(), None);
        assert!(body.len() <= MAX_REPORT_BODY_BYTES);
    }
}
