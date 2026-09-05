//! Policy-free connected-party and call-redirection metadata updates.

use std::ffi::c_void;
use std::marker::PhantomData;
use std::ptr::NonNull;

use thiserror::Error;

use sccp_protocol::{CallDirection, CallInfo};

const CALL_INFO_NAME_MAX_BYTES: usize = 39;
const CALL_INFO_NUMBER_MAX_BYTES: usize = 23;

/// A borrowed native channel that remains valid for the lifetime of this value.
#[derive(Debug)]
pub struct AsteriskChannel<'a> {
    raw: NonNull<c_void>,
    _borrow: PhantomData<&'a c_void>,
}

impl<'a> AsteriskChannel<'a> {
    /// Wraps a borrowed native channel pointer.
    ///
    /// # Safety
    ///
    /// `raw` must point to a live `ast_channel` and the caller must keep a
    /// reference to that channel for all of `'a`. The channel must also be safe
    /// to access from the calling thread under Asterisk's channel rules.
    pub unsafe fn from_raw(raw: *mut c_void) -> Result<Self, PartyUpdateError> {
        let raw = NonNull::new(raw).ok_or(PartyUpdateError::NullChannel)?;
        Ok(Self {
            raw,
            _borrow: PhantomData,
        })
    }

    pub fn as_raw(&self) -> *mut c_void {
        self.raw.as_ptr()
    }
}

/// Q.931 party-name character-set metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NameCharset(i32);

impl NameCharset {
    pub const UNKNOWN: Self = Self(0);
    pub const ISO_8859_1: Self = Self(1);
    pub const ISO_8859_2: Self = Self(3);
    pub const ISO_8859_3: Self = Self(4);
    pub const ISO_8859_4: Self = Self(5);
    pub const ISO_8859_5: Self = Self(6);
    pub const ISO_8859_7: Self = Self(7);
    pub const BMP_STRING: Self = Self(8);
    pub const UTF_8: Self = Self(9);

    pub const fn from_raw(value: i32) -> Self {
        Self(value)
    }

    pub const fn raw(self) -> i32 {
        self.0
    }
}

impl Default for NameCharset {
    fn default() -> Self {
        Self::UTF_8
    }
}

/// Q.931 presentation and screening metadata.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Presentation(i32);

impl Presentation {
    pub const ALLOWED_NOT_SCREENED: Self = Self(0x00);
    pub const ALLOWED_PASSED_SCREEN: Self = Self(0x01);
    pub const ALLOWED_FAILED_SCREEN: Self = Self(0x02);
    pub const ALLOWED_NETWORK_NUMBER: Self = Self(0x03);
    pub const RESTRICTED_NOT_SCREENED: Self = Self(0x20);
    pub const RESTRICTED_PASSED_SCREEN: Self = Self(0x21);
    pub const RESTRICTED_FAILED_SCREEN: Self = Self(0x22);
    pub const RESTRICTED_NETWORK_NUMBER: Self = Self(0x23);
    pub const NUMBER_UNAVAILABLE: Self = Self(0x43);

    pub const fn from_raw(value: i32) -> Self {
        Self(value)
    }

    pub const fn raw(self) -> i32 {
        self.0
    }

    /// Whether the party component may be presented to the handset.
    pub const fn is_allowed(self) -> bool {
        self.0 & 0x60 == 0
    }
}

/// Q.931 type-of-number and numbering-plan metadata.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NumberPlan(i32);

impl NumberPlan {
    pub const UNKNOWN: Self = Self(0);

    pub const fn from_raw(value: i32) -> Self {
        Self(value)
    }

    pub const fn raw(self) -> i32 {
        self.0
    }
}

/// Identifies why connected-party information changed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ConnectedLineSource(i32);

impl ConnectedLineSource {
    pub const UNKNOWN: Self = Self(0);
    pub const ANSWER: Self = Self(1);
    pub const DIVERSION: Self = Self(2);
    pub const TRANSFER: Self = Self(3);
    pub const TRANSFER_ALERTING: Self = Self(4);

    pub const fn from_raw(value: i32) -> Self {
        Self(value)
    }

    pub const fn raw(self) -> i32 {
        self.0
    }
}

/// A redirect reason code, including codes added by future backends.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RedirectReasonCode(i32);

impl RedirectReasonCode {
    pub const UNKNOWN: Self = Self(0);
    pub const USER_BUSY: Self = Self(1);
    pub const NO_ANSWER: Self = Self(2);
    pub const UNAVAILABLE: Self = Self(3);
    pub const UNCONDITIONAL: Self = Self(4);
    pub const TIME_OF_DAY: Self = Self(5);
    pub const DO_NOT_DISTURB: Self = Self(6);
    pub const DEFLECTION: Self = Self(7);
    pub const FOLLOW_ME: Self = Self(8);
    pub const OUT_OF_ORDER: Self = Self(9);
    pub const AWAY: Self = Self(10);
    pub const CALL_FORWARD_DTE: Self = Self(11);
    pub const SEND_TO_VOICEMAIL: Self = Self(12);

    pub const fn from_raw(value: i32) -> Self {
        Self(value)
    }

    pub const fn raw(self) -> i32 {
        self.0
    }
}

/// One public or private party identity.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PartyIdentity {
    /// `None` explicitly marks the name as invalid/absent; an empty string is
    /// preserved as a valid but empty name.
    pub name: Option<String>,
    /// `None` explicitly marks the number as invalid/absent.
    pub number: Option<String>,
    pub name_charset: NameCharset,
    pub name_presentation: Presentation,
    pub number_plan: NumberPlan,
    pub number_presentation: Presentation,
}

impl PartyIdentity {
    fn has_identity(&self) -> bool {
        self.name.is_some() || self.number.is_some()
    }

    pub fn visible_name(&self) -> Option<&str> {
        self.name_presentation
            .is_allowed()
            .then_some(self.name.as_deref())
            .flatten()
    }

    pub fn visible_number(&self) -> Option<&str> {
        self.number_presentation
            .is_allowed()
            .then_some(self.number.as_deref())
            .flatten()
    }

    pub const fn is_restricted(&self) -> bool {
        !self.name_presentation.is_allowed() || !self.number_presentation.is_allowed()
    }
}

/// Owned public party metadata copied atomically from one native channel.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PartySnapshot {
    pub caller: PartyIdentity,
    pub connected: PartyIdentity,
    pub redirecting_original: PartyIdentity,
    pub redirecting_from: PartyIdentity,
    pub redirecting_to: PartyIdentity,
    pub private_redirecting_original: PartyIdentity,
    pub private_redirecting_from: PartyIdentity,
    pub private_redirecting_to: PartyIdentity,
    pub connected_source: ConnectedLineSource,
    pub redirect_reason: RedirectReasonCode,
    pub original_redirect_reason: RedirectReasonCode,
    pub redirect_count: u32,
}

impl PartySnapshot {
    /// Seeds an inbound appearance from the requesting channel's caller.
    pub fn apply_initial_inbound_to_call_info(&self, current: &CallInfo) -> CallInfo {
        self.apply_with_primary(current, &self.caller)
    }

    /// Applies public native metadata without replacing appearance-local
    /// labels, and suppresses party components whose presentation is denied.
    pub fn apply_to_call_info(&self, current: &CallInfo) -> CallInfo {
        let primary = match current.direction {
            CallDirection::Inbound if self.connected.has_identity() => &self.connected,
            CallDirection::Inbound => &self.caller,
            CallDirection::Outbound => &self.connected,
        };
        self.apply_with_primary(current, primary)
    }

    fn apply_with_primary(&self, current: &CallInfo, primary: &PartyIdentity) -> CallInfo {
        let mut updated = current.clone();
        let restricted = primary.is_restricted()
            || self.redirecting_original.is_restricted()
            || self.redirecting_from.is_restricted();

        match current.direction {
            CallDirection::Inbound => {
                assign_visible(
                    &mut updated.calling_name,
                    primary.visible_name(),
                    CALL_INFO_NAME_MAX_BYTES,
                );
                assign_visible(
                    &mut updated.calling_number,
                    primary.visible_number(),
                    CALL_INFO_NUMBER_MAX_BYTES,
                );
            }
            CallDirection::Outbound => {
                assign_visible(
                    &mut updated.called_name,
                    primary.visible_name(),
                    CALL_INFO_NAME_MAX_BYTES,
                );
                assign_visible(
                    &mut updated.called_number,
                    primary.visible_number(),
                    CALL_INFO_NUMBER_MAX_BYTES,
                );
            }
        }
        assign_visible(
            &mut updated.original_called_name,
            self.redirecting_original.visible_name(),
            CALL_INFO_NAME_MAX_BYTES,
        );
        assign_visible(
            &mut updated.original_called_number,
            self.redirecting_original.visible_number(),
            CALL_INFO_NUMBER_MAX_BYTES,
        );
        assign_visible(
            &mut updated.last_redirecting_name,
            self.redirecting_from.visible_name(),
            CALL_INFO_NAME_MAX_BYTES,
        );
        assign_visible(
            &mut updated.last_redirecting_number,
            self.redirecting_from.visible_number(),
            CALL_INFO_NUMBER_MAX_BYTES,
        );
        updated.original_redirect_reason = self.original_redirect_reason.raw() as u32;
        updated.last_redirect_reason = self.redirect_reason.raw() as u32;
        updated.party_restrictions = if restricted { 0xf } else { 0 };
        clear_restricted(
            &mut updated,
            primary,
            &self.redirecting_original,
            &self.redirecting_from,
        );
        updated
    }
}

fn assign_visible(destination: &mut String, value: Option<&str>, maximum_bytes: usize) {
    if let Some(value) = value {
        destination.clear();
        let mut end = value.len().min(maximum_bytes);
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        destination.push_str(&value[..end]);
    }
}

fn clear_restricted(
    info: &mut CallInfo,
    primary: &PartyIdentity,
    original: &PartyIdentity,
    redirecting: &PartyIdentity,
) {
    if !primary.name_presentation.is_allowed() {
        match info.direction {
            CallDirection::Inbound => info.calling_name.clear(),
            CallDirection::Outbound => info.called_name.clear(),
        }
    }
    if !primary.number_presentation.is_allowed() {
        match info.direction {
            CallDirection::Inbound => info.calling_number.clear(),
            CallDirection::Outbound => info.called_number.clear(),
        }
    }
    if !original.name_presentation.is_allowed() {
        info.original_called_name.clear();
    }
    if !original.number_presentation.is_allowed() {
        info.original_called_number.clear();
    }
    if !redirecting.name_presentation.is_allowed() {
        info.last_redirecting_name.clear();
    }
    if !redirecting.number_presentation.is_allowed() {
        info.last_redirecting_number.clear();
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ConnectedLineUpdate {
    pub party: PartyIdentity,
    pub private_party: PartyIdentity,
    pub source: ConnectedLineSource,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RedirectReason {
    pub code: RedirectReasonCode,
    /// Optional protocol-specific reason text for codes without a standard
    /// mapping. It is retained even when the numeric code is known.
    pub text: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RedirectingUpdate {
    pub original: PartyIdentity,
    pub from: PartyIdentity,
    pub to: PartyIdentity,
    pub private_original: PartyIdentity,
    pub private_from: PartyIdentity,
    pub private_to: PartyIdentity,
    pub reason: RedirectReason,
    pub original_reason: RedirectReason,
    pub count: u32,
}

/// Builds one presentation-preserving redirect update from a native snapshot.
/// The caller must validate and bound `destination` before this boundary.
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) fn redirected_call_update(
    snapshot: &PartySnapshot,
    destination: &str,
    reason: RedirectReasonCode,
) -> Result<RedirectingUpdate, PartyUpdateError> {
    let count = snapshot
        .redirect_count
        .checked_add(1)
        .filter(|count| i32::try_from(*count).is_ok())
        .ok_or(PartyUpdateError::CountOutOfRange {
            count: snapshot.redirect_count,
        })?;
    let from = if identity_is_present(&snapshot.redirecting_to) {
        snapshot.redirecting_to.clone()
    } else if identity_is_present(&snapshot.connected) {
        snapshot.connected.clone()
    } else {
        snapshot.caller.clone()
    };
    let original = if identity_is_present(&snapshot.redirecting_original) {
        snapshot.redirecting_original.clone()
    } else {
        from.clone()
    };
    let original_reason = if snapshot.redirect_count == 0 {
        reason
    } else {
        snapshot.original_redirect_reason
    };
    Ok(RedirectingUpdate {
        original,
        from,
        to: PartyIdentity {
            number: Some(destination.to_owned()),
            number_presentation: Presentation::ALLOWED_NETWORK_NUMBER,
            ..PartyIdentity::default()
        },
        reason: RedirectReason {
            code: reason,
            text: None,
        },
        original_reason: RedirectReason {
            code: original_reason,
            text: None,
        },
        count,
        ..RedirectingUpdate::default()
    })
}

/// Reconstructs the complete redirecting pre-image for compensating a failed
/// route after the channel metadata was already applied.
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) fn restore_redirecting_update(snapshot: &PartySnapshot) -> RedirectingUpdate {
    RedirectingUpdate {
        original: snapshot.redirecting_original.clone(),
        from: snapshot.redirecting_from.clone(),
        to: snapshot.redirecting_to.clone(),
        private_original: snapshot.private_redirecting_original.clone(),
        private_from: snapshot.private_redirecting_from.clone(),
        private_to: snapshot.private_redirecting_to.clone(),
        reason: RedirectReason {
            code: snapshot.redirect_reason,
            text: None,
        },
        original_reason: RedirectReason {
            code: snapshot.original_redirect_reason,
            text: None,
        },
        count: snapshot.redirect_count,
    }
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) fn validate_redirecting_update(
    update: &RedirectingUpdate,
) -> Result<(), PartyUpdateError> {
    validate_redirecting(update)
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
fn identity_is_present(identity: &PartyIdentity) -> bool {
    identity.name.is_some() || identity.number.is_some()
}

#[derive(Debug, Error)]
pub enum PartyUpdateError {
    #[error("native channel pointer is null")]
    NullChannel,

    #[error("{field} contains a NUL byte")]
    InvalidText { field: &'static str },

    #[error("redirect count {count} exceeds the native range")]
    CountOutOfRange { count: u32 },

    #[error("native {operation} update failed")]
    NativeFailure { operation: &'static str },

    #[error("native {field} text is not valid UTF-8")]
    InvalidNativeText { field: &'static str },

    #[error("native redirect count {count} is negative")]
    InvalidNativeRedirectCount { count: i32 },

    #[error("native party updates are unavailable in development builds")]
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
/// Whether a party update mutates the channel immediately or is queued for its
/// consumer. Native adapters must preserve this distinction.
pub enum Delivery {
    Set,
    Queue,
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
fn validate_identity(
    identity: &PartyIdentity,
    name_field: &'static str,
    number_field: &'static str,
) -> Result<(), PartyUpdateError> {
    validate_optional_text(name_field, identity.name.as_deref())?;
    validate_optional_text(number_field, identity.number.as_deref())
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
fn validate_connected_line(update: &ConnectedLineUpdate) -> Result<(), PartyUpdateError> {
    validate_identity(&update.party, "party name", "party number")?;
    validate_identity(
        &update.private_party,
        "private party name",
        "private party number",
    )
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
fn validate_redirecting(update: &RedirectingUpdate) -> Result<(), PartyUpdateError> {
    i32::try_from(update.count).map_err(|_| PartyUpdateError::CountOutOfRange {
        count: update.count,
    })?;
    for (identity, name_field, number_field) in [
        (
            &update.original,
            "original party name",
            "original party number",
        ),
        (
            &update.from,
            "redirecting-from name",
            "redirecting-from number",
        ),
        (&update.to, "redirecting-to name", "redirecting-to number"),
        (
            &update.private_original,
            "private original party name",
            "private original party number",
        ),
        (
            &update.private_from,
            "private redirecting-from name",
            "private redirecting-from number",
        ),
        (
            &update.private_to,
            "private redirecting-to name",
            "private redirecting-to number",
        ),
    ] {
        validate_identity(identity, name_field, number_field)?;
    }
    validate_optional_text("redirect reason", update.reason.text.as_deref())?;
    validate_optional_text(
        "original redirect reason",
        update.original_reason.text.as_deref(),
    )
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
fn validate_optional_text(
    field: &'static str,
    value: Option<&str>,
) -> Result<(), PartyUpdateError> {
    if value.is_some_and(|value| value.contains('\0')) {
        Err(PartyUpdateError::InvalidText { field })
    } else {
        Ok(())
    }
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) trait PartyUpdateBackend {
    fn snapshot(&self, channel: &AsteriskChannel<'_>) -> Result<PartySnapshot, PartyUpdateError>;

    fn connected_line(
        &self,
        channel: &AsteriskChannel<'_>,
        update: &ConnectedLineUpdate,
        delivery: Delivery,
    ) -> Result<(), PartyUpdateError>;

    fn redirecting(
        &self,
        channel: &AsteriskChannel<'_>,
        update: &RedirectingUpdate,
        delivery: Delivery,
    ) -> Result<(), PartyUpdateError>;
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) fn dispatch_snapshot(
    backend: &impl PartyUpdateBackend,
    channel: &AsteriskChannel<'_>,
) -> Result<PartySnapshot, PartyUpdateError> {
    backend.snapshot(channel)
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) fn dispatch_connected(
    backend: &impl PartyUpdateBackend,
    channel: &AsteriskChannel<'_>,
    update: &ConnectedLineUpdate,
    delivery: Delivery,
) -> Result<(), PartyUpdateError> {
    validate_connected_line(update)?;
    backend.connected_line(channel, update, delivery)
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) fn dispatch_redirecting(
    backend: &impl PartyUpdateBackend,
    channel: &AsteriskChannel<'_>,
    update: &RedirectingUpdate,
    delivery: Delivery,
) -> Result<(), PartyUpdateError> {
    validate_redirecting(update)?;
    backend.redirecting(channel, update, delivery)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    struct FakeNative {
        calls: Cell<usize>,
    }

    impl PartyUpdateBackend for FakeNative {
        fn snapshot(
            &self,
            _channel: &AsteriskChannel<'_>,
        ) -> Result<PartySnapshot, PartyUpdateError> {
            self.calls.set(self.calls.get() + 1);
            Ok(PartySnapshot {
                caller: PartyIdentity {
                    name: Some("Restricted caller".into()),
                    number: Some("1200".into()),
                    name_presentation: Presentation::RESTRICTED_NOT_SCREENED,
                    number_presentation: Presentation::ALLOWED_NETWORK_NUMBER,
                    ..PartyIdentity::default()
                },
                connected: numbered("2200"),
                redirecting_original: numbered("1000"),
                redirecting_from: numbered("1100"),
                redirecting_to: numbered("2200"),
                private_redirecting_original: numbered("private-1000"),
                private_redirecting_from: numbered("private-1100"),
                private_redirecting_to: numbered("private-2200"),
                connected_source: ConnectedLineSource::TRANSFER,
                redirect_reason: RedirectReasonCode::UNCONDITIONAL,
                original_redirect_reason: RedirectReasonCode::USER_BUSY,
                redirect_count: 2,
            })
        }

        fn connected_line(
            &self,
            _channel: &AsteriskChannel<'_>,
            update: &ConnectedLineUpdate,
            delivery: Delivery,
        ) -> Result<(), PartyUpdateError> {
            assert_eq!(delivery, Delivery::Queue);
            assert_eq!(update.party.name.as_deref(), Some("Public"));
            assert_eq!(update.party.number.as_deref(), Some("1200"));
            assert_eq!(update.party.name_charset.raw(), 9);
            assert_eq!(update.party.name_presentation.raw(), 0x01);
            assert_eq!(update.party.number_plan.raw(), 0x11);
            assert_eq!(update.party.number_presentation.raw(), 0x22);
            assert_eq!(update.private_party.name.as_deref(), Some("Private"));
            assert!(update.private_party.number.is_none());
            assert_eq!(update.source.raw(), 4);
            self.calls.set(self.calls.get() + 1);
            Ok(())
        }

        fn redirecting(
            &self,
            _channel: &AsteriskChannel<'_>,
            update: &RedirectingUpdate,
            delivery: Delivery,
        ) -> Result<(), PartyUpdateError> {
            assert_eq!(delivery, Delivery::Set);
            let numbers = [
                &update.original,
                &update.from,
                &update.to,
                &update.private_original,
                &update.private_from,
                &update.private_to,
            ]
            .map(|identity| identity.number.clone().unwrap());
            assert_eq!(
                numbers,
                [
                    "1".to_owned(),
                    "2".to_owned(),
                    "3".to_owned(),
                    "4".to_owned(),
                    "5".to_owned(),
                    "6".to_owned()
                ]
            );
            assert_eq!(update.reason.code.raw(), 4);
            assert_eq!(update.reason.text.as_deref(), Some("unconditional"));
            assert_eq!(update.original_reason.code.raw(), 44);
            assert_eq!(
                update.original_reason.text.as_deref(),
                Some("vendor-reason")
            );
            assert_eq!(update.count, 3);
            self.calls.set(self.calls.get() + 1);
            Ok(())
        }
    }

    fn borrowed_test_channel<'a>(storage: &'a mut u8) -> AsteriskChannel<'a> {
        unsafe { AsteriskChannel::from_raw(std::ptr::from_mut(storage).cast()).unwrap() }
    }

    fn numbered(number: &str) -> PartyIdentity {
        PartyIdentity {
            number: Some(number.to_owned()),
            ..PartyIdentity::default()
        }
    }

    #[test]
    fn fake_native_connected_contract_preserves_metadata_and_delivery() {
        let mut storage = 0;
        let channel = borrowed_test_channel(&mut storage);
        let native = FakeNative {
            calls: Cell::new(0),
        };
        let update = ConnectedLineUpdate {
            party: PartyIdentity {
                name: Some("Public".into()),
                number: Some("1200".into()),
                name_charset: NameCharset::UTF_8,
                name_presentation: Presentation::ALLOWED_PASSED_SCREEN,
                number_plan: NumberPlan::from_raw(0x11),
                number_presentation: Presentation::RESTRICTED_FAILED_SCREEN,
            },
            private_party: PartyIdentity {
                name: Some("Private".into()),
                ..PartyIdentity::default()
            },
            source: ConnectedLineSource::TRANSFER_ALERTING,
        };
        dispatch_connected(&native, &channel, &update, Delivery::Queue).unwrap();
        assert_eq!(native.calls.get(), 1);
    }

    #[test]
    fn fake_native_snapshot_preserves_metadata_and_filters_each_component() {
        let mut storage = 0;
        let channel = borrowed_test_channel(&mut storage);
        let native = FakeNative {
            calls: Cell::new(0),
        };

        let snapshot = dispatch_snapshot(&native, &channel).unwrap();
        assert_eq!(snapshot.caller.visible_name(), None);
        assert_eq!(snapshot.caller.visible_number(), Some("1200"));
        assert!(snapshot.caller.is_restricted());
        assert_eq!(snapshot.connected.visible_number(), Some("2200"));
        assert_eq!(snapshot.connected_source, ConnectedLineSource::TRANSFER);
        assert_eq!(snapshot.redirect_reason, RedirectReasonCode::UNCONDITIONAL);
        assert_eq!(
            snapshot.original_redirect_reason,
            RedirectReasonCode::USER_BUSY
        );
        assert_eq!(snapshot.redirect_count, 2);
        assert_eq!(native.calls.get(), 1);
    }

    #[test]
    fn snapshot_updates_directional_parties_and_never_exposes_restricted_text() {
        let inbound = CallInfo {
            direction: CallDirection::Inbound,
            called_name: "Local appearance".into(),
            called_number: "2000".into(),
            ..CallInfo::default()
        };
        let snapshot = PartySnapshot {
            caller: PartyIdentity {
                name: Some("Caller".into()),
                number: Some("1000".into()),
                ..PartyIdentity::default()
            },
            redirecting_original: PartyIdentity {
                name: Some("Private destination".into()),
                number: Some("1900".into()),
                name_presentation: Presentation::RESTRICTED_NOT_SCREENED,
                number_presentation: Presentation::RESTRICTED_NOT_SCREENED,
                ..PartyIdentity::default()
            },
            redirecting_from: PartyIdentity {
                name: Some("Reception".into()),
                number: Some("1950".into()),
                ..PartyIdentity::default()
            },
            redirect_reason: RedirectReasonCode::UNCONDITIONAL,
            original_redirect_reason: RedirectReasonCode::NO_ANSWER,
            ..PartySnapshot::default()
        };

        let updated = snapshot.apply_to_call_info(&inbound);
        assert_eq!(updated.calling_name, "Caller");
        assert_eq!(updated.calling_number, "1000");
        assert_eq!(updated.called_name, "Local appearance");
        assert_eq!(updated.called_number, "2000");
        assert!(updated.original_called_name.is_empty());
        assert!(updated.original_called_number.is_empty());
        assert_eq!(updated.last_redirecting_name, "Reception");
        assert_eq!(updated.last_redirecting_number, "1950");
        assert_eq!(updated.original_redirect_reason, 2);
        assert_eq!(updated.last_redirect_reason, 4);
        assert_eq!(updated.party_restrictions, 0xf);
    }

    #[test]
    fn snapshot_fits_utf8_party_text_to_every_supported_handset_layout() {
        let snapshot = PartySnapshot {
            caller: PartyIdentity {
                name: Some("é".repeat(30)),
                number: Some("123456789012345678901234567890".into()),
                ..PartyIdentity::default()
            },
            ..PartySnapshot::default()
        };
        let current = CallInfo {
            direction: CallDirection::Inbound,
            ..CallInfo::default()
        };

        let updated = snapshot.apply_initial_inbound_to_call_info(&current);
        assert!(updated.calling_name.len() <= CALL_INFO_NAME_MAX_BYTES);
        assert!(
            updated
                .calling_name
                .is_char_boundary(updated.calling_name.len())
        );
        assert_eq!(updated.calling_number.len(), CALL_INFO_NUMBER_MAX_BYTES);
    }

    #[test]
    fn fake_native_redirecting_contract_preserves_every_party_and_reason() {
        let mut storage = 0;
        let channel = borrowed_test_channel(&mut storage);
        let native = FakeNative {
            calls: Cell::new(0),
        };
        let update = RedirectingUpdate {
            original: numbered("1"),
            from: numbered("2"),
            to: numbered("3"),
            private_original: numbered("4"),
            private_from: numbered("5"),
            private_to: numbered("6"),
            reason: RedirectReason {
                code: RedirectReasonCode::UNCONDITIONAL,
                text: Some("unconditional".into()),
            },
            original_reason: RedirectReason {
                code: RedirectReasonCode::from_raw(44),
                text: Some("vendor-reason".into()),
            },
            count: 3,
        };
        dispatch_redirecting(&native, &channel, &update, Delivery::Set).unwrap();
        assert_eq!(native.calls.get(), 1);
    }

    #[test]
    fn routed_call_preserves_original_and_restricted_last_redirecting_party() {
        let snapshot = PartySnapshot {
            caller: numbered("1000"),
            connected: PartyIdentity {
                name: Some("Private desk".into()),
                number: Some("2000".into()),
                name_presentation: Presentation::RESTRICTED_NOT_SCREENED,
                number_presentation: Presentation::RESTRICTED_NOT_SCREENED,
                ..PartyIdentity::default()
            },
            redirecting_original: numbered("1500"),
            redirecting_to: numbered("2000"),
            original_redirect_reason: RedirectReasonCode::USER_BUSY,
            redirect_count: 2,
            ..PartySnapshot::default()
        };

        let update = redirected_call_update(
            &snapshot,
            "private-voicemail",
            RedirectReasonCode::NO_ANSWER,
        )
        .unwrap();

        assert_eq!(update.original.number.as_deref(), Some("1500"));
        assert_eq!(update.from.number.as_deref(), Some("2000"));
        assert_eq!(
            update.from.number_presentation,
            Presentation::ALLOWED_NOT_SCREENED
        );
        assert_eq!(update.to.number.as_deref(), Some("private-voicemail"));
        assert_eq!(update.reason.code, RedirectReasonCode::NO_ANSWER);
        assert_eq!(update.original_reason.code, RedirectReasonCode::USER_BUSY);
        assert_eq!(update.count, 3);

        let first = redirected_call_update(
            &PartySnapshot {
                connected: snapshot.connected,
                ..PartySnapshot::default()
            },
            "3000",
            RedirectReasonCode::UNCONDITIONAL,
        )
        .unwrap();
        assert_eq!(
            first.from.number_presentation,
            Presentation::RESTRICTED_NOT_SCREENED
        );
        assert_eq!(
            first.original.number_presentation,
            Presentation::RESTRICTED_NOT_SCREENED
        );
        assert_eq!(
            first.original_reason.code,
            RedirectReasonCode::UNCONDITIONAL
        );
    }

    #[test]
    fn routed_call_rejects_redirect_count_overflow() {
        assert!(matches!(
            redirected_call_update(
                &PartySnapshot {
                    redirect_count: i32::MAX as u32,
                    ..PartySnapshot::default()
                },
                "3000",
                RedirectReasonCode::UNCONDITIONAL,
            ),
            Err(PartyUpdateError::CountOutOfRange { .. })
        ));
    }

    #[test]
    fn redirect_rollback_reconstructs_the_complete_public_and_private_preimage() {
        let snapshot = PartySnapshot {
            redirecting_original: numbered("1000"),
            redirecting_from: numbered("1100"),
            redirecting_to: numbered("1200"),
            private_redirecting_original: numbered("private-1000"),
            private_redirecting_from: numbered("private-1100"),
            private_redirecting_to: numbered("private-1200"),
            redirect_reason: RedirectReasonCode::NO_ANSWER,
            original_redirect_reason: RedirectReasonCode::USER_BUSY,
            redirect_count: 3,
            ..PartySnapshot::default()
        };

        let update = restore_redirecting_update(&snapshot);
        validate_redirecting_update(&update).unwrap();
        assert_eq!(update.original, snapshot.redirecting_original);
        assert_eq!(update.from, snapshot.redirecting_from);
        assert_eq!(update.to, snapshot.redirecting_to);
        assert_eq!(
            update.private_original,
            snapshot.private_redirecting_original
        );
        assert_eq!(update.private_from, snapshot.private_redirecting_from);
        assert_eq!(update.private_to, snapshot.private_redirecting_to);
        assert_eq!(update.reason.code, RedirectReasonCode::NO_ANSWER);
        assert_eq!(update.original_reason.code, RedirectReasonCode::USER_BUSY);
        assert_eq!(update.count, 3);
    }

    #[test]
    fn rejects_invalid_text_and_out_of_range_counts() {
        let connected = ConnectedLineUpdate {
            party: PartyIdentity {
                name: Some("bad\0name".into()),
                ..PartyIdentity::default()
            },
            ..ConnectedLineUpdate::default()
        };
        assert!(matches!(
            validate_connected_line(&connected),
            Err(PartyUpdateError::InvalidText {
                field: "party name"
            })
        ));

        let redirecting = RedirectingUpdate {
            count: i32::MAX as u32 + 1,
            ..RedirectingUpdate::default()
        };
        assert!(matches!(
            validate_redirecting(&redirecting),
            Err(PartyUpdateError::CountOutOfRange { .. })
        ));
    }

    #[test]
    fn rejects_null_channel_handles() {
        let error = unsafe { AsteriskChannel::from_raw(std::ptr::null_mut()) }.unwrap_err();
        assert!(matches!(error, PartyUpdateError::NullChannel));
    }
}
