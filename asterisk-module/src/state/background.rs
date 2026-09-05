//! Stores explicit per-device overrides; absence delegates to current configuration.

use sccp_protocol::{DeviceId, PhoneBackgroundHttpUrl};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::{BackgroundThumbnailSource, DeviceBackground, DeviceBackgroundError};
use crate::state::persistence::{PersistenceError, PersistentStore};

const BACKGROUND_FAMILY: &str = "SCCP";

const STORED_FORMAT_VERSION: u8 = 1;
const MAX_STORED_BYTES: usize = 2 * 1024;

pub struct BackgroundStore<S> {
    storage: S,
}

impl<S> BackgroundStore<S> {
    pub const fn new(storage: S) -> Self {
        Self { storage }
    }
}

impl<S: PersistentStore> BackgroundStore<S> {
    pub fn load_override(
        &self,
        device: &DeviceId,
    ) -> Result<Option<DeviceBackground>, BackgroundStoreError> {
        let Some(raw) = self.storage.get(BACKGROUND_FAMILY, &key(device))? else {
            return Ok(None);
        };
        if raw.len() > MAX_STORED_BYTES {
            return Err(BackgroundStoreError::DocumentTooLarge {
                device: device.clone(),
                bytes: raw.len(),
                maximum: MAX_STORED_BYTES,
            });
        }
        let version: StoredVersion =
            serde_json::from_str(&raw).map_err(|source| BackgroundStoreError::InvalidDocument {
                device: device.clone(),
                source,
            })?;
        if version.version != STORED_FORMAT_VERSION {
            return Err(BackgroundStoreError::UnsupportedVersion {
                device: device.clone(),
                version: version.version,
            });
        }
        let stored: StoredBackground =
            serde_json::from_str(&raw).map_err(|source| BackgroundStoreError::InvalidDocument {
                device: device.clone(),
                source,
            })?;
        DeviceBackground::new(stored.image_url, stored.thumbnail_url)
            .map(Some)
            .map_err(|source| BackgroundStoreError::InvalidBackground {
                device: device.clone(),
                source,
            })
    }

    pub fn put_override(
        &self,
        device: &DeviceId,
        background: &DeviceBackground,
    ) -> Result<(), BackgroundStoreError> {
        let thumbnail_url = match background.thumbnail_source() {
            BackgroundThumbnailSource::Explicit => Some(background.thumbnail_url().clone()),
            BackgroundThumbnailSource::Derived => None,
            BackgroundThumbnailSource::Dynamic => {
                return Err(BackgroundStoreError::DynamicOverride {
                    device: device.clone(),
                });
            }
        };
        let stored = StoredBackground {
            version: STORED_FORMAT_VERSION,
            image_url: background.image_url().clone(),
            thumbnail_url,
        };
        let raw =
            serde_json::to_string(&stored).map_err(|source| BackgroundStoreError::Encode {
                device: device.clone(),
                source,
            })?;
        if raw.len() > MAX_STORED_BYTES {
            return Err(BackgroundStoreError::EncodedDocumentTooLarge {
                device: device.clone(),
                bytes: raw.len(),
                maximum: MAX_STORED_BYTES,
            });
        }
        self.storage.put(BACKGROUND_FAMILY, &key(device), &raw)?;
        Ok(())
    }

    pub fn reset(&self, device: &DeviceId) -> Result<(), BackgroundStoreError> {
        self.storage.delete(BACKGROUND_FAMILY, &key(device))?;
        Ok(())
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredBackground {
    version: u8,
    image_url: PhoneBackgroundHttpUrl,
    thumbnail_url: Option<PhoneBackgroundHttpUrl>,
}

#[derive(Deserialize)]
struct StoredVersion {
    version: u8,
}

#[derive(Debug, Error)]
pub enum BackgroundStoreError {
    #[error(transparent)]
    Storage(#[from] PersistenceError),
    #[error("persisted background for device {device} is {bytes} bytes; maximum is {maximum}")]
    DocumentTooLarge {
        device: DeviceId,
        bytes: usize,
        maximum: usize,
    },
    #[error("persisted background for device {device} is not a valid document")]
    InvalidDocument {
        device: DeviceId,
        #[source]
        source: serde_json::Error,
    },
    #[error("persisted background for device {device} has unsupported version {version}")]
    UnsupportedVersion { device: DeviceId, version: u8 },
    #[error("persisted background for device {device} is invalid")]
    InvalidBackground {
        device: DeviceId,
        #[source]
        source: DeviceBackgroundError,
    },
    #[error("a dynamic background cannot be stored as an explicit override for device {device}")]
    DynamicOverride { device: DeviceId },
    #[error("unable to encode the background override for device {device}")]
    Encode {
        device: DeviceId,
        #[source]
        source: serde_json::Error,
    },
    #[error("encoded background for device {device} is {bytes} bytes; maximum is {maximum}")]
    EncodedDocumentTooLarge {
        device: DeviceId,
        bytes: usize,
        maximum: usize,
    },
}

fn key(device: &DeviceId) -> String {
    format!("device/{}/background", device.as_str())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;
    use crate::config::ResolvedDeviceBackground;

    #[derive(Default)]
    struct MemoryStore {
        entries: Mutex<HashMap<(String, String), String>>,
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
            self.entries
                .lock()
                .unwrap()
                .insert((family.to_owned(), key.to_owned()), value.to_owned());
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

    impl MemoryStore {
        fn insert(&self, device: &DeviceId, value: impl Into<String>) {
            self.entries
                .lock()
                .unwrap()
                .insert((BACKGROUND_FAMILY.to_owned(), key(device)), value.into());
        }
    }

    fn device() -> DeviceId {
        DeviceId::new("SEP001122334455").unwrap()
    }

    #[test]
    fn absent_override_delegates_to_configuration() {
        let store = BackgroundStore::new(MemoryStore::default());
        assert!(store.load_override(&device()).unwrap().is_none());
    }

    #[test]
    fn derived_thumbnail_round_trips_as_derived_state() {
        let store = BackgroundStore::new(MemoryStore::default());
        let background = DeviceBackground::new(
            PhoneBackgroundHttpUrl::new("http://assets.example.test/topkek.jpg").unwrap(),
            None,
        )
        .unwrap();
        store.put_override(&device(), &background).unwrap();

        let loaded = store.load_override(&device()).unwrap().unwrap();
        assert_eq!(loaded, background);
        assert_eq!(
            loaded.thumbnail_source(),
            BackgroundThumbnailSource::Derived
        );
    }

    #[test]
    fn explicit_thumbnail_round_trips_and_reset_removes_it() {
        let store = BackgroundStore::new(MemoryStore::default());
        let background = DeviceBackground::new(
            PhoneBackgroundHttpUrl::new("http://assets.example.test/image").unwrap(),
            Some(PhoneBackgroundHttpUrl::new("http://assets.example.test/icon").unwrap()),
        )
        .unwrap();
        store.put_override(&device(), &background).unwrap();
        assert_eq!(store.load_override(&device()).unwrap(), Some(background));

        store.reset(&device()).unwrap();
        assert!(store.load_override(&device()).unwrap().is_none());
    }

    #[test]
    fn dynamic_backgrounds_cannot_enter_the_override_store() {
        let pattern = crate::config::DynamicBackgroundPattern::new(
            "https://images.example.test/render.{FORMAT}?w={W}&h={H}&bitdepth={B}",
        )
        .unwrap();
        let Some(ResolvedDeviceBackground::Set(background)) = pattern
            .resolve(sccp_protocol::DeviceType::Cisco7965)
            .unwrap()
        else {
            panic!("7965 dynamic background must use set-background");
        };
        let store = BackgroundStore::new(MemoryStore::default());

        assert!(matches!(
            store.put_override(&device(), &background),
            Err(BackgroundStoreError::DynamicOverride { .. })
        ));
    }

    #[test]
    fn future_versions_are_reported_before_version_specific_fields_are_decoded() {
        let storage = MemoryStore::default();
        storage.insert(&device(), r#"{"version":2,"future":"value"}"#);
        let store = BackgroundStore::new(storage);

        assert!(matches!(
            store.load_override(&device()),
            Err(BackgroundStoreError::UnsupportedVersion { version: 2, .. })
        ));
    }

    #[test]
    fn malformed_documents_are_rejected_without_echoing_their_contents() {
        let storage = MemoryStore::default();
        storage.insert(&device(), r#"{"version":1,"token":"private""#);
        let store = BackgroundStore::new(storage);

        let error = store.load_override(&device()).unwrap_err();
        assert!(matches!(
            error,
            BackgroundStoreError::InvalidDocument { .. }
        ));
        assert!(!error.to_string().contains("private"));
    }

    #[test]
    fn oversized_documents_are_rejected_before_json_decoding() {
        let storage = MemoryStore::default();
        storage.insert(&device(), "x".repeat(MAX_STORED_BYTES + 1));
        let store = BackgroundStore::new(storage);

        assert!(matches!(
            store.load_override(&device()),
            Err(BackgroundStoreError::DocumentTooLarge {
                bytes,
                maximum,
                ..
            }) if bytes == MAX_STORED_BYTES + 1 && maximum == MAX_STORED_BYTES
        ));
    }

    #[test]
    fn current_documents_reject_unknown_fields_and_invalid_urls() {
        for document in [
            r#"{"version":1,"image_url":"https://example.test/image.png","thumbnail_url":null,"unknown":true}"#,
            r#"{"version":1,"image_url":"ftp://private.example/image.png","thumbnail_url":null}"#,
        ] {
            let storage = MemoryStore::default();
            storage.insert(&device(), document);
            let store = BackgroundStore::new(storage);

            let error = store.load_override(&device()).unwrap_err();
            assert!(matches!(
                error,
                BackgroundStoreError::InvalidDocument { .. }
            ));
            assert!(!error.to_string().contains("private.example"));
        }
    }
}
