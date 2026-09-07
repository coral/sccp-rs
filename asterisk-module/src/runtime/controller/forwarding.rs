//! Forwarding-entry ownership and generation-checked handset completions.

use super::*;
use crate::call::forwarding::{
    ForwardingCommit, ForwardingDigitOutcome, ForwardingEntry, ForwardingEntryId,
    ForwardingEntryTiming, ForwardingExpiryOutcome, ForwardingKind, ForwardingRejection,
    ForwardingWriteOutcome,
};

impl Controller {
    pub(crate) fn begin_forwarding_entry(
        &mut self,
        device_id: DeviceId,
        line_instance: u32,
        call_id: CallId,
        kind: ForwardingKind,
        dial_terminator: Digit,
        timing: ForwardingEntryTiming,
    ) -> Result<ForwardingEntry, ForwardingRejection> {
        if !self.is_registered(&device_id) {
            return Err(ForwardingRejection::Conflict);
        }
        self.forwarding_entries.begin(
            device_id,
            line_instance,
            call_id,
            kind,
            dial_terminator,
            timing,
        )
    }
    pub(crate) fn expire_forwarding_entries(
        &mut self,
        now: Instant,
    ) -> Vec<ForwardingExpiryOutcome> {
        self.forwarding_entries.claim_expired(now)
    }
    pub(crate) fn settle_forwarding_collection(
        &mut self,
        device_id: &DeviceId,
        entry_id: ForwardingEntryId,
        outcome: ForwardingWriteOutcome,
    ) -> Result<ForwardingWriteOutcome, ForwardingRejection> {
        self.forwarding_entries
            .settle_collection_write(device_id, entry_id, outcome)
    }
    pub(crate) fn settle_forwarding_terminal(
        &mut self,
        device_id: &DeviceId,
        entry_id: ForwardingEntryId,
        outcome: ForwardingWriteOutcome,
    ) -> Result<ForwardingWriteOutcome, ForwardingRejection> {
        self.forwarding_entries
            .settle_terminal_write(device_id, entry_id, outcome)
    }
    pub(crate) fn cancel_forwarding_for_call(
        &mut self,
        device_id: &DeviceId,
        call_id: CallId,
    ) -> bool {
        let Some(entry) = self
            .forwarding_entries
            .for_call(call_id)
            .filter(|entry| &entry.device_id == device_id)
        else {
            return false;
        };
        self.forwarding_entries
            .cancel_collection(device_id, entry.id)
            .is_ok()
    }
    pub(crate) fn cancel_forwarding_for_device(&mut self, device_id: &DeviceId) -> bool {
        let Some(entry) = self.forwarding_entries.get(device_id) else {
            return false;
        };
        self.forwarding_entries.cancel(device_id, entry.id).is_ok()
    }
    pub(crate) fn input_forwarding_digit(
        &mut self,
        device_id: &DeviceId,
        call_id: CallId,
        digit: Digit,
        now: Instant,
    ) -> Option<Result<ForwardingDigitOutcome, ForwardingRejection>> {
        let entry = self.forwarding_entries.for_call(call_id)?;
        if &entry.device_id != device_id {
            return Some(Ok(ForwardingDigitOutcome::Collected));
        }
        Some(
            self.forwarding_entries
                .input_digit(device_id, entry.id, digit, now),
        )
    }
    pub(crate) fn backspace_forwarding(
        &mut self,
        device_id: &DeviceId,
        call_id: CallId,
        now: Instant,
    ) -> bool {
        let Some(entry) = self
            .forwarding_entries
            .for_call(call_id)
            .filter(|entry| &entry.device_id == device_id)
        else {
            return false;
        };
        self.forwarding_entries
            .backspace(device_id, entry.id, now)
            .is_ok()
    }
    pub(crate) fn replace_forwarding_digits(
        &mut self,
        device_id: &DeviceId,
        call_id: CallId,
        digits: &str,
        now: Instant,
    ) -> Option<Result<(), ForwardingRejection>> {
        let entry = self.forwarding_entries.for_call(call_id)?;
        if &entry.device_id != device_id {
            return Some(Ok(()));
        }
        Some(
            self.forwarding_entries
                .replace_digits(device_id, entry.id, digits, now),
        )
    }
    pub(crate) fn begin_forwarding_commit(
        &mut self,
        device_id: &DeviceId,
        call_id: CallId,
    ) -> Option<Result<ForwardingCommit, ForwardingRejection>> {
        let entry = self
            .forwarding_entries
            .for_call(call_id)
            .filter(|entry| &entry.device_id == device_id)?;
        Some(self.forwarding_entries.begin_commit(device_id, entry.id))
    }
    pub(crate) fn finish_forwarding_commit(
        &mut self,
        device_id: &DeviceId,
        entry_id: ForwardingEntryId,
        succeeded: bool,
    ) -> bool {
        if succeeded {
            self.forwarding_entries.commit(device_id, entry_id).is_ok()
        } else {
            self.forwarding_entries.cancel(device_id, entry_id).is_ok()
        }
    }
}

impl ControllerSnapshot {
    pub fn forwarding_entry_for_call(&self, call_id: CallId) -> Option<&ForwardingEntry> {
        self.forwarding_entries.for_call(call_id)
    }
}
