//! Safe typed channel metadata reads, writes, and inherited-variable copying.
//!
//! This domain module owns validation and the backend-neutral metadata
//! contract. Conversion to Asterisk strings, party structs, locks, allocation,
//! and rollback lives exclusively in the native adapter.

use thiserror::Error;

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
use crate::call::metadata::CallMetadata;
use crate::call::metadata::MetadataError;
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
use crate::pbx::party::AsteriskChannel;

#[derive(Debug, Error)]
pub enum ChannelMetadataError {
    #[error("channel metadata is invalid: {0}")]
    InvalidMetadata(#[from] MetadataError),
    #[error("native {operation} failed")]
    NativeFailure { operation: &'static str },
    #[error("native {field} text is not valid bounded UTF-8")]
    InvalidNativeText { field: &'static str },
    #[error("native channel metadata is unavailable in development builds")]
    Unavailable,
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) trait ChannelMetadataBackend {
    fn snapshot(&self, channel: &AsteriskChannel<'_>)
    -> Result<CallMetadata, ChannelMetadataError>;

    fn apply(
        &self,
        channel: &AsteriskChannel<'_>,
        metadata: &CallMetadata,
    ) -> Result<(), ChannelMetadataError>;

    fn inherit(
        &self,
        parent: &AsteriskChannel<'_>,
        child: &AsteriskChannel<'_>,
    ) -> Result<(), ChannelMetadataError>;
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) fn dispatch_snapshot(
    backend: &impl ChannelMetadataBackend,
    channel: &AsteriskChannel<'_>,
) -> Result<CallMetadata, ChannelMetadataError> {
    backend.snapshot(channel)
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) fn dispatch_apply(
    backend: &impl ChannelMetadataBackend,
    channel: &AsteriskChannel<'_>,
    metadata: &CallMetadata,
) -> Result<(), ChannelMetadataError> {
    metadata.validate()?;
    backend.apply(channel, metadata)
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) fn dispatch_inherit(
    backend: &impl ChannelMetadataBackend,
    parent: &AsteriskChannel<'_>,
    child: &AsteriskChannel<'_>,
) -> Result<(), ChannelMetadataError> {
    backend.inherit(parent, child)
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) fn validate_native_channel_metadata(
    metadata: &CallMetadata,
) -> Result<(), ChannelMetadataError> {
    metadata.validate().map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use crate::call::metadata::ChannelVariable;

    use super::*;

    struct FakeBackend {
        calls: RefCell<Vec<&'static str>>,
        variables: RefCell<Vec<(String, String)>>,
        snapshot: RefCell<Option<CallMetadata>>,
        fail_apply: Cell<bool>,
    }

    impl ChannelMetadataBackend for FakeBackend {
        fn snapshot(
            &self,
            _channel: &AsteriskChannel<'_>,
        ) -> Result<CallMetadata, ChannelMetadataError> {
            self.calls.borrow_mut().push("snapshot");
            Ok(self.snapshot.borrow().clone().unwrap_or_default())
        }

        fn apply(
            &self,
            _channel: &AsteriskChannel<'_>,
            metadata: &CallMetadata,
        ) -> Result<(), ChannelMetadataError> {
            self.calls.borrow_mut().push("apply");
            self.variables.borrow_mut().extend(
                metadata
                    .variables
                    .iter()
                    .map(|variable| (variable.name().to_owned(), variable.value().to_owned())),
            );
            if self.fail_apply.get() {
                Err(ChannelMetadataError::NativeFailure {
                    operation: "channel metadata apply",
                })
            } else {
                Ok(())
            }
        }

        fn inherit(
            &self,
            _parent: &AsteriskChannel<'_>,
            _child: &AsteriskChannel<'_>,
        ) -> Result<(), ChannelMetadataError> {
            self.calls.borrow_mut().push("inherit");
            Ok(())
        }
    }

    fn borrowed_test_channel<'a>(storage: &'a mut u8) -> AsteriskChannel<'a> {
        unsafe { AsteriskChannel::from_raw(std::ptr::from_mut(storage).cast()).unwrap() }
    }

    #[test]
    fn validated_owned_metadata_reaches_typed_backend() {
        let backend = FakeBackend {
            calls: RefCell::new(Vec::new()),
            variables: RefCell::new(Vec::new()),
            snapshot: RefCell::new(None),
            fail_apply: Cell::new(false),
        };
        let mut storage = 0;
        let channel = borrowed_test_channel(&mut storage);
        let metadata = CallMetadata {
            dnid: Some(String::new()),
            language: Some("sv".into()),
            variables: vec![ChannelVariable::new("__TRACE_ID", "alpha").unwrap()],
            ..CallMetadata::default()
        };

        dispatch_apply(&backend, &channel, &metadata).unwrap();
        assert_eq!(&*backend.calls.borrow(), &["apply"]);
        assert_eq!(
            &*backend.variables.borrow(),
            &[("__TRACE_ID".into(), "alpha".into())]
        );
    }

    #[test]
    fn invalid_metadata_never_reaches_backend_and_failure_is_typed() {
        let backend = FakeBackend {
            calls: RefCell::new(Vec::new()),
            variables: RefCell::new(Vec::new()),
            snapshot: RefCell::new(None),
            fail_apply: Cell::new(true),
        };
        let mut storage = 0;
        let channel = borrowed_test_channel(&mut storage);
        let invalid = CallMetadata {
            variables: vec![
                ChannelVariable::new("DUP", "one").unwrap(),
                ChannelVariable::new("DUP", "two").unwrap(),
            ],
            ..CallMetadata::default()
        };
        assert!(matches!(
            dispatch_apply(&backend, &channel, &invalid),
            Err(ChannelMetadataError::InvalidMetadata(
                MetadataError::DuplicateVariable
            ))
        ));
        assert!(backend.calls.borrow().is_empty());
        assert!(matches!(
            validate_native_channel_metadata(&invalid),
            Err(ChannelMetadataError::InvalidMetadata(
                MetadataError::DuplicateVariable
            ))
        ));

        assert!(matches!(
            dispatch_apply(&backend, &channel, &CallMetadata::default()),
            Err(ChannelMetadataError::NativeFailure {
                operation: "channel metadata apply"
            })
        ));
    }

    #[test]
    fn snapshot_and_inheritance_keep_distinct_channel_borrows() {
        let expected = CallMetadata {
            language: Some("en".into()),
            ..CallMetadata::default()
        };
        let backend = FakeBackend {
            calls: RefCell::new(Vec::new()),
            variables: RefCell::new(Vec::new()),
            snapshot: RefCell::new(Some(expected.clone())),
            fail_apply: Cell::new(false),
        };
        let (mut parent_storage, mut child_storage) = (0, 0);
        let parent = borrowed_test_channel(&mut parent_storage);
        let child = borrowed_test_channel(&mut child_storage);

        assert_eq!(dispatch_snapshot(&backend, &parent).unwrap(), expected);
        dispatch_inherit(&backend, &parent, &child).unwrap();
        assert_eq!(&*backend.calls.borrow(), &["snapshot", "inherit"]);
    }
}
