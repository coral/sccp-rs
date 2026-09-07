//! Typed claims for redirecting one existing call to configured voicemail.

use std::collections::HashMap;
use std::fmt;

use sccp_protocol::{CallId, DeviceId};
use thiserror::Error;

use crate::ami::controls::MAX_DIAL_DESTINATION_BYTES;
use crate::runtime::backend::PbxCallId;

pub const MAX_VOICEMAIL_CONTEXT_BYTES: usize = 79;

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct VoicemailContext(String);

impl VoicemailContext {
    pub fn new(value: impl AsRef<str>) -> Result<Self, VoicemailRejection> {
        let value = value.as_ref().trim();
        if value.is_empty()
            || value.len() > MAX_VOICEMAIL_CONTEXT_BYTES
            || value.chars().any(char::is_control)
        {
            return Err(VoicemailRejection::InvalidTarget);
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for VoicemailContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VoicemailContext")
            .field("bytes", &self.0.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct VoicemailDestination(String);

impl VoicemailDestination {
    pub fn new(value: impl AsRef<str>) -> Result<Self, VoicemailRejection> {
        let value = value.as_ref().trim();
        if value.is_empty()
            || value.len() > MAX_DIAL_DESTINATION_BYTES
            || value.chars().any(char::is_control)
        {
            return Err(VoicemailRejection::InvalidTarget);
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for VoicemailDestination {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VoicemailDestination")
            .field("bytes", &self.0.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct VoicemailTransactionId(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VoicemailAction {
    ImmediateDivert,
    TransferSelected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VoicemailPhase {
    Claimed,
    Executing,
}

#[derive(Clone, Eq, PartialEq)]
pub struct VoicemailTarget {
    context: VoicemailContext,
    destination: VoicemailDestination,
}

impl VoicemailTarget {
    pub fn new(
        context: impl AsRef<str>,
        destination: impl AsRef<str>,
    ) -> Result<Self, VoicemailRejection> {
        let context = VoicemailContext::new(context)?;
        let destination = VoicemailDestination::new(destination)?;
        Ok(Self {
            context,
            destination,
        })
    }

    pub fn context(&self) -> &str {
        self.context.as_str()
    }

    pub fn destination(&self) -> &str {
        self.destination.as_str()
    }
}

impl fmt::Debug for VoicemailTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VoicemailTarget")
            .field("context", &self.context)
            .field("destination", &self.destination)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VoicemailTransaction {
    pub id: VoicemailTransactionId,
    pub device_id: DeviceId,
    pub handset_call_id: CallId,
    pub pbx_call_id: PbxCallId,
    pub action: VoicemailAction,
    pub phase: VoicemailPhase,
    pub target: VoicemailTarget,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VoicemailOperation {
    pub transaction_id: VoicemailTransactionId,
    pub device_id: DeviceId,
    pub handset_call_id: CallId,
    pub pbx_call_id: PbxCallId,
    pub action: VoicemailAction,
    pub target: VoicemailTarget,
}

#[derive(Clone, Debug, Default)]
pub struct VoicemailRegistry {
    next_id: u64,
    by_device: HashMap<DeviceId, VoicemailTransaction>,
}

impl VoicemailRegistry {
    pub fn claim(
        &mut self,
        device_id: DeviceId,
        handset_call_id: CallId,
        pbx_call_id: PbxCallId,
        action: VoicemailAction,
        target: VoicemailTarget,
    ) -> Result<VoicemailTransaction, VoicemailRejection> {
        if self.by_device.contains_key(&device_id)
            || self.by_device.values().any(|transaction| {
                transaction.handset_call_id == handset_call_id
                    || transaction.pbx_call_id == pbx_call_id
            })
        {
            return Err(VoicemailRejection::Conflict);
        }
        let id = VoicemailTransactionId(
            self.next_id
                .checked_add(1)
                .ok_or(VoicemailRejection::IdentifierExhausted)?,
        );
        self.next_id = id.0;
        let transaction = VoicemailTransaction {
            id,
            device_id: device_id.clone(),
            handset_call_id,
            pbx_call_id,
            action,
            phase: VoicemailPhase::Claimed,
            target,
        };
        self.by_device.insert(device_id, transaction.clone());
        Ok(transaction)
    }

    pub fn get(&self, device_id: &DeviceId) -> Option<&VoicemailTransaction> {
        self.by_device.get(device_id)
    }

    pub fn for_pbx(&self, pbx_call_id: PbxCallId) -> Option<&VoicemailTransaction> {
        self.by_device
            .values()
            .find(|transaction| transaction.pbx_call_id == pbx_call_id)
    }

    pub fn transactions(&self) -> impl Iterator<Item = &VoicemailTransaction> {
        self.by_device.values()
    }

    pub fn begin_execution(
        &mut self,
        device_id: &DeviceId,
        transaction_id: VoicemailTransactionId,
    ) -> Result<VoicemailOperation, VoicemailRejection> {
        let transaction = self.exact_mut(device_id, transaction_id)?;
        if transaction.phase != VoicemailPhase::Claimed {
            return Err(VoicemailRejection::InvalidPhase);
        }
        transaction.phase = VoicemailPhase::Executing;
        Ok(VoicemailOperation {
            transaction_id: transaction.id,
            device_id: transaction.device_id.clone(),
            handset_call_id: transaction.handset_call_id,
            pbx_call_id: transaction.pbx_call_id,
            action: transaction.action,
            target: transaction.target.clone(),
        })
    }

    pub fn commit(
        &mut self,
        device_id: &DeviceId,
        transaction_id: VoicemailTransactionId,
    ) -> Result<VoicemailTransaction, VoicemailRejection> {
        if self.by_device.get(device_id).is_none_or(|transaction| {
            transaction.id != transaction_id || transaction.phase != VoicemailPhase::Executing
        }) {
            return Err(VoicemailRejection::Conflict);
        }
        Ok(self
            .by_device
            .remove(device_id)
            .expect("exact voicemail transaction was checked"))
    }

    pub fn cancel(
        &mut self,
        device_id: &DeviceId,
        transaction_id: VoicemailTransactionId,
    ) -> Result<VoicemailTransaction, VoicemailRejection> {
        if self
            .by_device
            .get(device_id)
            .is_none_or(|transaction| transaction.id != transaction_id)
        {
            return Err(VoicemailRejection::Conflict);
        }
        Ok(self
            .by_device
            .remove(device_id)
            .expect("exact voicemail transaction was checked"))
    }

    pub fn cancel_for_pbx(
        &mut self,
        pbx_call_id: PbxCallId,
    ) -> Result<VoicemailTransaction, VoicemailRejection> {
        let (device_id, transaction_id) = self
            .by_device
            .values()
            .find(|transaction| transaction.pbx_call_id == pbx_call_id)
            .map(|transaction| (transaction.device_id.clone(), transaction.id))
            .ok_or(VoicemailRejection::Conflict)?;
        self.cancel(&device_id, transaction_id)
    }

    fn exact_mut(
        &mut self,
        device_id: &DeviceId,
        transaction_id: VoicemailTransactionId,
    ) -> Result<&mut VoicemailTransaction, VoicemailRejection> {
        self.by_device
            .get_mut(device_id)
            .filter(|transaction| transaction.id == transaction_id)
            .ok_or(VoicemailRejection::Conflict)
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum VoicemailRejection {
    #[error("voicemail redirect target is missing or invalid")]
    InvalidTarget,
    #[error("voicemail redirect conflicts with current state")]
    Conflict,
    #[error("voicemail redirect is not valid in the current phase")]
    InvalidPhase,
    #[error("voicemail redirect identifier space is exhausted")]
    IdentifierExhausted,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(number: u8) -> DeviceId {
        DeviceId::new(format!("SEP0011223344{number:02}")).unwrap()
    }

    fn target(number: &str) -> VoicemailTarget {
        VoicemailTarget::new("from-sccp", number).unwrap()
    }

    #[test]
    fn targets_validate_without_disclosing_destinations() {
        let target = target("61001");
        assert_eq!(target.context(), "from-sccp");
        assert_eq!(target.destination(), "61001");
        assert!(!format!("{target:?}").contains("61001"));
        assert!(!format!("{target:?}").contains("from-sccp"));
        assert_eq!(
            VoicemailTarget::new("", "61001"),
            Err(VoicemailRejection::InvalidTarget)
        );
        assert_eq!(
            VoicemailTarget::new("from-sccp", "61\n001"),
            Err(VoicemailRejection::InvalidTarget)
        );
        assert_eq!(
            VoicemailTarget::new("c".repeat(MAX_VOICEMAIL_CONTEXT_BYTES + 1), "61001"),
            Err(VoicemailRejection::InvalidTarget)
        );
        assert_eq!(
            VoicemailTarget::new("from-sccp", "6".repeat(MAX_DIAL_DESTINATION_BYTES + 1)),
            Err(VoicemailRejection::InvalidTarget)
        );
    }

    #[test]
    fn immediate_and_selected_redirects_are_exactly_serialized() {
        let mut registry = VoicemailRegistry::default();
        let first = registry
            .claim(
                device(1),
                CallId(10),
                PbxCallId(20),
                VoicemailAction::ImmediateDivert,
                target("600"),
            )
            .unwrap();
        assert_eq!(registry.for_pbx(PbxCallId(20)), Some(&first));
        assert_eq!(
            registry.claim(
                device(2),
                CallId(11),
                PbxCallId(20),
                VoicemailAction::TransferSelected,
                target("61001"),
            ),
            Err(VoicemailRejection::Conflict)
        );
        let operation = registry.begin_execution(&device(1), first.id).unwrap();
        assert_eq!(operation.pbx_call_id, PbxCallId(20));
        assert_eq!(operation.action, VoicemailAction::ImmediateDivert);
        assert_eq!(
            registry.begin_execution(&device(1), first.id),
            Err(VoicemailRejection::InvalidPhase)
        );
        assert_eq!(registry.commit(&device(1), first.id).unwrap().id, first.id);
        assert!(registry.get(&device(1)).is_none());
    }

    #[test]
    fn stale_completion_cannot_remove_a_retry() {
        let mut registry = VoicemailRegistry::default();
        let first = registry
            .claim(
                device(1),
                CallId(10),
                PbxCallId(20),
                VoicemailAction::ImmediateDivert,
                target("600"),
            )
            .unwrap();
        registry.cancel(&device(1), first.id).unwrap();
        let retry = registry
            .claim(
                device(1),
                CallId(10),
                PbxCallId(20),
                VoicemailAction::ImmediateDivert,
                target("600"),
            )
            .unwrap();
        assert!(retry.id > first.id);
        assert_eq!(
            registry.commit(&device(1), first.id),
            Err(VoicemailRejection::Conflict)
        );
        assert_eq!(registry.get(&device(1)).unwrap().id, retry.id);
        assert_eq!(registry.cancel_for_pbx(PbxCallId(20)).unwrap().id, retry.id);
    }

    #[test]
    fn claim_fails_closed_when_identifier_space_is_exhausted() {
        let mut registry = VoicemailRegistry {
            next_id: u64::MAX,
            ..Default::default()
        };
        assert_eq!(
            registry.claim(
                device(1),
                CallId(10),
                PbxCallId(20),
                VoicemailAction::ImmediateDivert,
                target("600"),
            ),
            Err(VoicemailRejection::IdentifierExhausted)
        );
        assert!(registry.get(&device(1)).is_none());
    }
}
