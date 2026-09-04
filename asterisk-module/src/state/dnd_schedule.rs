//! Persistent per-device overrides for recurring DND schedules.

use sccp_protocol::DeviceId;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::{
    DndSchedule, DndScheduleParseError, DndScheduleValidationError, MAX_DND_SCHEDULES,
    validate_dnd_schedules,
};
use crate::state::persistence::{PersistenceError, PersistentStore};

pub const DND_SCHEDULE_FAMILY: &str = "SCCP";

const STORED_FORMAT_VERSION: u8 = 1;
const MAX_STORED_BYTES: usize = 8 * 1024;

/// Stores the optional CLI-owned DND schedule for a configured device.
///
/// An absent key delegates to the configuration. A present document with an
/// empty `rules` array deliberately overrides the configuration with no rules.
pub struct DndScheduleStore<S> {
    storage: S,
}

impl<S> DndScheduleStore<S> {
    pub const fn new(storage: S) -> Self {
        Self { storage }
    }
}

impl<S: PersistentStore> DndScheduleStore<S> {
    pub fn load_override(
        &self,
        device: &DeviceId,
    ) -> Result<Option<Vec<DndSchedule>>, DndScheduleStoreError> {
        let Some(raw) = self.snapshot_raw(device)? else {
            return Ok(None);
        };
        if raw.len() > MAX_STORED_BYTES {
            return Err(DndScheduleStoreError::DocumentTooLarge {
                device: device.clone(),
                bytes: raw.len(),
                maximum: MAX_STORED_BYTES,
            });
        }

        let stored: StoredDndSchedule = serde_json::from_str(&raw).map_err(|source| {
            DndScheduleStoreError::InvalidDocument {
                device: device.clone(),
                source,
            }
        })?;
        if stored.version != STORED_FORMAT_VERSION {
            return Err(DndScheduleStoreError::UnsupportedVersion {
                device: device.clone(),
                version: stored.version,
            });
        }
        if stored.rules.len() > MAX_DND_SCHEDULES {
            return Err(DndScheduleStoreError::TooManyRules {
                device: device.clone(),
                count: stored.rules.len(),
                maximum: MAX_DND_SCHEDULES,
            });
        }

        let schedules = stored
            .rules
            .iter()
            .enumerate()
            .map(|(index, value)| {
                DndSchedule::parse(value).map_err(|source| DndScheduleStoreError::InvalidRule {
                    device: device.clone(),
                    index: index + 1,
                    source,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        validate_dnd_schedules(&schedules).map_err(|source| {
            DndScheduleStoreError::InvalidRuleSet {
                device: device.clone(),
                source,
            }
        })?;
        Ok(Some(schedules))
    }

    pub fn put_override(
        &self,
        device: &DeviceId,
        schedules: &[DndSchedule],
    ) -> Result<(), DndScheduleStoreError> {
        validate_dnd_schedules(schedules).map_err(|source| {
            DndScheduleStoreError::InvalidRuleSet {
                device: device.clone(),
                source,
            }
        })?;

        let stored = StoredDndSchedule {
            version: STORED_FORMAT_VERSION,
            rules: schedules.iter().map(ToString::to_string).collect(),
        };
        let raw =
            serde_json::to_string(&stored).map_err(|source| DndScheduleStoreError::Encode {
                device: device.clone(),
                source,
            })?;
        if raw.len() > MAX_STORED_BYTES {
            return Err(DndScheduleStoreError::EncodedDocumentTooLarge {
                device: device.clone(),
                bytes: raw.len(),
                maximum: MAX_STORED_BYTES,
            });
        }
        self.storage.put(DND_SCHEDULE_FAMILY, &key(device), &raw)?;
        Ok(())
    }

    pub fn reset(&self, device: &DeviceId) -> Result<(), DndScheduleStoreError> {
        self.storage.delete(DND_SCHEDULE_FAMILY, &key(device))?;
        Ok(())
    }

    pub fn snapshot_raw(&self, device: &DeviceId) -> Result<Option<String>, DndScheduleStoreError> {
        Ok(self.storage.get(DND_SCHEDULE_FAMILY, &key(device))?)
    }

    /// Restores an exact snapshot, including a previously corrupt value.
    ///
    /// This intentionally performs no validation: rollback must restore the
    /// byte-for-byte state that existed before a failed higher-level mutation.
    pub fn restore_raw(
        &self,
        device: &DeviceId,
        snapshot: Option<&str>,
    ) -> Result<(), DndScheduleStoreError> {
        match snapshot {
            Some(raw) => self.storage.put(DND_SCHEDULE_FAMILY, &key(device), raw)?,
            None => self.storage.delete(DND_SCHEDULE_FAMILY, &key(device))?,
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredDndSchedule {
    version: u8,
    rules: Vec<String>,
}

#[derive(Debug, Error)]
pub enum DndScheduleStoreError {
    #[error(transparent)]
    Storage(#[from] PersistenceError),

    #[error("persisted DND schedule for device {device} is {bytes} bytes; maximum is {maximum}")]
    DocumentTooLarge {
        device: DeviceId,
        bytes: usize,
        maximum: usize,
    },

    #[error("persisted DND schedule for device {device} is not a valid document")]
    InvalidDocument {
        device: DeviceId,
        #[source]
        source: serde_json::Error,
    },

    #[error("persisted DND schedule for device {device} has unsupported version {version}")]
    UnsupportedVersion { device: DeviceId, version: u8 },

    #[error("persisted DND schedule for device {device} has {count} rules; maximum is {maximum}")]
    TooManyRules {
        device: DeviceId,
        count: usize,
        maximum: usize,
    },

    #[error("persisted DND schedule rule {index} for device {device} is invalid")]
    InvalidRule {
        device: DeviceId,
        index: usize,
        #[source]
        source: DndScheduleParseError,
    },

    #[error("DND schedule override for device {device} is invalid")]
    InvalidRuleSet {
        device: DeviceId,
        #[source]
        source: DndScheduleValidationError,
    },

    #[error("unable to encode DND schedule override for device {device}")]
    Encode {
        device: DeviceId,
        #[source]
        source: serde_json::Error,
    },

    #[error("encoded DND schedule for device {device} is {bytes} bytes; maximum is {maximum}")]
    EncodedDocumentTooLarge {
        device: DeviceId,
        bytes: usize,
        maximum: usize,
    },
}

fn key(device: &DeviceId) -> String {
    format!("device/{}/dnd-schedule", device.as_str())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct MemoryStore {
        entries: Mutex<HashMap<(String, String), String>>,
    }

    impl MemoryStore {
        fn insert(&self, family: &str, key: &str, value: &str) {
            self.entries
                .lock()
                .unwrap()
                .insert((family.to_owned(), key.to_owned()), value.to_owned());
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
            self.insert(family, key, value);
            Ok(())
        }

        fn delete(&self, family: &str, key: &str) -> Result<(), PersistenceError> {
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

    fn schedule(value: &str) -> DndSchedule {
        DndSchedule::parse(value).unwrap()
    }

    #[test]
    fn absent_key_delegates_to_configuration() {
        let store = DndScheduleStore::new(MemoryStore::default());
        assert!(store.load_override(&device()).unwrap().is_none());
    }

    #[test]
    fn round_trips_canonical_rules() {
        let store = DndScheduleStore::new(MemoryStore::default());
        let rules = vec![
            schedule("22:00-07:00, mon-thu, reject"),
            schedule("23:00-09:00, fri-sun, silent"),
        ];

        store.put_override(&device(), &rules).unwrap();
        let loaded = store.load_override(&device()).unwrap().unwrap();

        assert_eq!(
            loaded.iter().map(ToString::to_string).collect::<Vec<_>>(),
            rules.iter().map(ToString::to_string).collect::<Vec<_>>()
        );
        let raw = store.snapshot_raw(&device()).unwrap().unwrap();
        assert!(raw.contains("\"version\":1"));
    }

    #[test]
    fn empty_document_is_an_explicit_override() {
        let store = DndScheduleStore::new(MemoryStore::default());
        store.put_override(&device(), &[]).unwrap();
        assert_eq!(store.load_override(&device()).unwrap().unwrap().len(), 0);

        store.reset(&device()).unwrap();
        assert!(store.load_override(&device()).unwrap().is_none());
    }

    #[test]
    fn refuses_to_persist_an_overlapping_rule_set() {
        let store = DndScheduleStore::new(MemoryStore::default());
        let error = store
            .put_override(
                &device(),
                &[
                    schedule("22:00-07:00, *, reject"),
                    schedule("23:00-06:00, *, silent"),
                ],
            )
            .unwrap_err();

        assert!(matches!(
            error,
            DndScheduleStoreError::InvalidRuleSet { .. }
        ));
        assert!(store.snapshot_raw(&device()).unwrap().is_none());
    }

    #[test]
    fn rejects_corrupt_json_version_shape_and_rules() {
        let cases = [
            "not-json",
            r#"{"version":2,"rules":[]}"#,
            r#"{"version":1,"rules":[],"extra":true}"#,
            r#"{"version":1,"rules":["not a schedule"]}"#,
            r#"{"version":1,"rules":["22:00-07:00, *, reject","23:00-06:00, *, silent"]}"#,
        ];

        for raw in cases {
            let storage = MemoryStore::default();
            storage.insert(DND_SCHEDULE_FAMILY, &key(&device()), raw);
            let error = DndScheduleStore::new(storage)
                .load_override(&device())
                .unwrap_err();
            assert!(matches!(
                error,
                DndScheduleStoreError::InvalidDocument { .. }
                    | DndScheduleStoreError::UnsupportedVersion { .. }
                    | DndScheduleStoreError::InvalidRule { .. }
                    | DndScheduleStoreError::InvalidRuleSet { .. }
            ));
        }
    }

    #[test]
    fn rejects_more_than_the_maximum_rule_count_before_parsing() {
        let storage = MemoryStore::default();
        let stored = StoredDndSchedule {
            version: STORED_FORMAT_VERSION,
            rules: vec!["22:00-23:00, mon, reject".into(); MAX_DND_SCHEDULES + 1],
        };
        storage.insert(
            DND_SCHEDULE_FAMILY,
            &key(&device()),
            &serde_json::to_string(&stored).unwrap(),
        );

        let error = DndScheduleStore::new(storage)
            .load_override(&device())
            .unwrap_err();
        assert!(matches!(error, DndScheduleStoreError::TooManyRules { .. }));
    }

    #[test]
    fn rejects_an_oversized_document_before_decoding_it() {
        let storage = MemoryStore::default();
        storage.insert(
            DND_SCHEDULE_FAMILY,
            &key(&device()),
            &"x".repeat(MAX_STORED_BYTES + 1),
        );

        let error = DndScheduleStore::new(storage)
            .load_override(&device())
            .unwrap_err();
        assert!(matches!(
            error,
            DndScheduleStoreError::DocumentTooLarge {
                bytes,
                maximum: MAX_STORED_BYTES,
                ..
            } if bytes == MAX_STORED_BYTES + 1
        ));
    }

    #[test]
    fn raw_snapshot_restores_present_and_absent_values_exactly() {
        let store = DndScheduleStore::new(MemoryStore::default());
        let absent = store.snapshot_raw(&device()).unwrap();
        assert!(absent.is_none());

        store
            .restore_raw(&device(), Some("legacy or corrupt bytes"))
            .unwrap();
        let present = store.snapshot_raw(&device()).unwrap();
        assert_eq!(present.as_deref(), Some("legacy or corrupt bytes"));

        store.put_override(&device(), &[]).unwrap();
        store.restore_raw(&device(), present.as_deref()).unwrap();
        assert_eq!(store.snapshot_raw(&device()).unwrap(), present);

        store.restore_raw(&device(), absent.as_deref()).unwrap();
        assert!(store.snapshot_raw(&device()).unwrap().is_none());
    }
}
