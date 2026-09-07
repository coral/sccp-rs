//! Typed forwarding destinations, handset entry, and no-answer timer claims.

use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, Instant};

use sccp_protocol::{CallId, DeviceId, Digit};
use thiserror::Error;

use crate::runtime::backend::PbxCallId;

/// Legacy ForwardStatus messages reserve 24 bytes including the terminator.
pub const MAX_FORWARD_DESTINATION_BYTES: usize = 23;
pub const MAX_FORWARD_CONTEXT_BYTES: usize = 79;

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ForwardingDestination(String);

impl ForwardingDestination {
    pub fn new(value: impl AsRef<str>) -> Result<Self, ForwardingRejection> {
        let value = value.as_ref().trim();
        if value.is_empty() {
            return Err(ForwardingRejection::InvalidDestination);
        }
        if value.len() > MAX_FORWARD_DESTINATION_BYTES {
            return Err(ForwardingRejection::DestinationTooLong);
        }
        if value.chars().any(char::is_control) {
            return Err(ForwardingRejection::InvalidDestination);
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Debug for ForwardingDestination {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ForwardingDestination")
            .field("bytes", &self.0.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ForwardingContext(String);

impl ForwardingContext {
    pub fn new(value: impl AsRef<str>) -> Result<Self, ForwardingRejection> {
        let value = value.as_ref().trim();
        if value.is_empty()
            || value.len() > MAX_FORWARD_CONTEXT_BYTES
            || value.chars().any(char::is_control)
        {
            return Err(ForwardingRejection::InvalidContext);
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ForwardingContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ForwardingContext")
            .field("bytes", &self.0.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ForwardingKind {
    All,
    Busy,
    NoAnswer,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ForwardingRouteReason {
    Unconditional,
    Busy,
    NoAnswer,
}

#[derive(Clone, Eq, PartialEq)]
pub struct ForwardingOperation {
    pub call_id: PbxCallId,
    pub context: ForwardingContext,
    pub destination: ForwardingDestination,
    pub reason: ForwardingRouteReason,
}

impl fmt::Debug for ForwardingOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ForwardingOperation")
            .field("call_id", &self.call_id)
            .field("context", &self.context)
            .field("destination", &self.destination)
            .field("reason", &self.reason)
            .finish()
    }
}

impl ForwardingKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Busy => "busy",
            Self::NoAnswer => "noanswer",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        if value.eq_ignore_ascii_case("all") {
            Some(Self::All)
        } else if value.eq_ignore_ascii_case("busy") {
            Some(Self::Busy)
        } else if value.eq_ignore_ascii_case("noanswer") {
            Some(Self::NoAnswer)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ForwardingEntryId(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ForwardingEntryPhase {
    Collecting,
    Committing,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ForwardingWriteOutcome {
    Written,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ForwardingDigitOutcome {
    Collected,
    Commit(ForwardingCommit),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ForwardingExpiryOutcome {
    Cancel(ForwardingEntry),
    Commit(ForwardingCommit),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ForwardingEntryTiming {
    pub now: Instant,
    pub first_digit_timeout: Duration,
    pub interdigit_timeout: Duration,
}

#[derive(Clone, Eq, PartialEq)]
pub struct ForwardingEntry {
    pub id: ForwardingEntryId,
    pub device_id: DeviceId,
    pub line_instance: u32,
    pub call_id: CallId,
    pub kind: ForwardingKind,
    pub phase: ForwardingEntryPhase,
    pub deadline: Instant,
    dial_terminator: Digit,
    first_digit_timeout: Duration,
    interdigit_timeout: Duration,
    digits: String,
}

impl fmt::Debug for ForwardingEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ForwardingEntry")
            .field("id", &self.id)
            .field("device_id", &self.device_id)
            .field("line_instance", &self.line_instance)
            .field("call_id", &self.call_id)
            .field("kind", &self.kind)
            .field("phase", &self.phase)
            .field("deadline", &self.deadline)
            .field("dial_terminator", &self.dial_terminator)
            .field("first_digit_timeout", &self.first_digit_timeout)
            .field("interdigit_timeout", &self.interdigit_timeout)
            .field(
                "digits",
                &format_args!("<redacted:{} bytes>", self.digits.len()),
            )
            .finish_non_exhaustive()
    }
}

impl ForwardingEntry {
    pub fn digits(&self) -> &str {
        &self.digits
    }

    fn push_digit(&mut self, digit: Digit, now: Instant) -> Result<(), ForwardingRejection> {
        if self.phase != ForwardingEntryPhase::Collecting {
            return Err(ForwardingRejection::InvalidPhase);
        }
        if self.digits.len() >= MAX_FORWARD_DESTINATION_BYTES {
            return Err(ForwardingRejection::DestinationTooLong);
        }
        self.digits.push(match digit {
            Digit::Number(number) if number <= 9 => char::from(b'0' + number),
            Digit::Number(_) => return Err(ForwardingRejection::InvalidDestination),
            Digit::Star => '*',
            Digit::Pound => '#',
            Digit::A => 'A',
            Digit::B => 'B',
            Digit::C => 'C',
            Digit::D => 'D',
            Digit::Unknown(_) => return Err(ForwardingRejection::InvalidDestination),
        });
        self.deadline = now + self.interdigit_timeout;
        Ok(())
    }

    fn input_digit(
        &mut self,
        digit: Digit,
        now: Instant,
    ) -> Result<ForwardingDigitOutcome, ForwardingRejection> {
        if digit == self.dial_terminator {
            return self.begin_commit().map(ForwardingDigitOutcome::Commit);
        }
        self.push_digit(digit, now)?;
        Ok(ForwardingDigitOutcome::Collected)
    }

    fn backspace(&mut self, now: Instant) -> Result<(), ForwardingRejection> {
        if self.phase != ForwardingEntryPhase::Collecting {
            return Err(ForwardingRejection::InvalidPhase);
        }
        if self.digits.is_empty() {
            self.deadline = now + self.first_digit_timeout;
        } else {
            self.digits.pop();
            self.deadline = now + self.interdigit_timeout;
        }
        Ok(())
    }

    fn begin_commit(&mut self) -> Result<ForwardingCommit, ForwardingRejection> {
        if self.phase != ForwardingEntryPhase::Collecting {
            return Err(ForwardingRejection::InvalidPhase);
        }
        let destination = ForwardingDestination::new(&self.digits)?;
        self.phase = ForwardingEntryPhase::Committing;
        Ok(ForwardingCommit {
            entry_id: self.id,
            device_id: self.device_id.clone(),
            line_instance: self.line_instance,
            call_id: self.call_id,
            kind: self.kind,
            destination,
        })
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct ForwardingCommit {
    pub entry_id: ForwardingEntryId,
    pub device_id: DeviceId,
    pub line_instance: u32,
    pub call_id: CallId,
    pub kind: ForwardingKind,
    pub destination: ForwardingDestination,
}

impl fmt::Debug for ForwardingCommit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ForwardingCommit")
            .field("entry_id", &self.entry_id)
            .field("device_id", &self.device_id)
            .field("line_instance", &self.line_instance)
            .field("call_id", &self.call_id)
            .field("kind", &self.kind)
            .field("destination", &self.destination)
            .finish()
    }
}

#[derive(Clone, Debug, Default)]
pub struct ForwardingEntryRegistry {
    next_id: u64,
    by_device: HashMap<DeviceId, ForwardingEntry>,
}

impl ForwardingEntryRegistry {
    pub fn begin(
        &mut self,
        device_id: DeviceId,
        line_instance: u32,
        call_id: CallId,
        kind: ForwardingKind,
        dial_terminator: Digit,
        timing: ForwardingEntryTiming,
    ) -> Result<ForwardingEntry, ForwardingRejection> {
        if line_instance == 0
            || timing.first_digit_timeout.is_zero()
            || timing.interdigit_timeout.is_zero()
            || self.by_device.contains_key(&device_id)
        {
            return Err(ForwardingRejection::Conflict);
        }
        if self
            .by_device
            .values()
            .any(|entry| entry.call_id == call_id)
        {
            return Err(ForwardingRejection::Conflict);
        }
        let id = ForwardingEntryId(
            self.next_id
                .checked_add(1)
                .ok_or(ForwardingRejection::IdentifierExhausted)?,
        );
        self.next_id = id.0;
        let entry = ForwardingEntry {
            id,
            device_id: device_id.clone(),
            line_instance,
            call_id,
            kind,
            phase: ForwardingEntryPhase::Collecting,
            deadline: timing.now + timing.first_digit_timeout,
            dial_terminator,
            first_digit_timeout: timing.first_digit_timeout,
            interdigit_timeout: timing.interdigit_timeout,
            digits: String::new(),
        };
        self.by_device.insert(device_id, entry.clone());
        Ok(entry)
    }

    pub fn get(&self, device_id: &DeviceId) -> Option<&ForwardingEntry> {
        self.by_device.get(device_id)
    }

    pub fn for_call(&self, call_id: CallId) -> Option<&ForwardingEntry> {
        self.by_device
            .values()
            .find(|entry| entry.call_id == call_id)
    }

    pub fn input_digit(
        &mut self,
        device_id: &DeviceId,
        entry_id: ForwardingEntryId,
        digit: Digit,
        now: Instant,
    ) -> Result<ForwardingDigitOutcome, ForwardingRejection> {
        self.exact_mut(device_id, entry_id)?.input_digit(digit, now)
    }

    pub fn backspace(
        &mut self,
        device_id: &DeviceId,
        entry_id: ForwardingEntryId,
        now: Instant,
    ) -> Result<(), ForwardingRejection> {
        self.exact_mut(device_id, entry_id)?.backspace(now)
    }

    pub fn replace_digits(
        &mut self,
        device_id: &DeviceId,
        entry_id: ForwardingEntryId,
        digits: &str,
        now: Instant,
    ) -> Result<(), ForwardingRejection> {
        let entry = self.exact_mut(device_id, entry_id)?;
        let digits = digits
            .strip_suffix(entry.dial_terminator.as_char())
            .unwrap_or(digits);
        if digits.len() > MAX_FORWARD_DESTINATION_BYTES {
            return Err(ForwardingRejection::DestinationTooLong);
        }
        if digits
            .chars()
            .any(|digit| !matches!(digit, '0'..='9' | '*' | '#' | 'A' | 'B' | 'C' | 'D'))
        {
            return Err(ForwardingRejection::InvalidDestination);
        }
        if entry.phase != ForwardingEntryPhase::Collecting {
            return Err(ForwardingRejection::InvalidPhase);
        }
        entry.digits.clear();
        entry.digits.push_str(digits);
        entry.deadline = now
            + if digits.is_empty() {
                entry.first_digit_timeout
            } else {
                entry.interdigit_timeout
            };
        Ok(())
    }

    pub fn claim_expired(&mut self, now: Instant) -> Vec<ForwardingExpiryOutcome> {
        let mut expired = self
            .by_device
            .values()
            .filter(|entry| {
                entry.phase == ForwardingEntryPhase::Collecting && entry.deadline <= now
            })
            .map(|entry| (entry.deadline, entry.id, entry.device_id.clone()))
            .collect::<Vec<_>>();
        expired.sort_unstable_by_key(|(deadline, id, _)| (*deadline, *id));
        expired
            .into_iter()
            .filter_map(|(_, id, device_id)| {
                if self.by_device.get(&device_id).is_none_or(|entry| {
                    entry.id != id
                        || entry.phase != ForwardingEntryPhase::Collecting
                        || entry.deadline > now
                }) {
                    return None;
                }
                if self
                    .by_device
                    .get(&device_id)
                    .is_some_and(|entry| entry.digits.is_empty())
                {
                    return self
                        .by_device
                        .remove(&device_id)
                        .map(ForwardingExpiryOutcome::Cancel);
                }
                match self
                    .by_device
                    .get_mut(&device_id)
                    .expect("exact expired forwarding entry was checked")
                    .begin_commit()
                {
                    Ok(commit) => Some(ForwardingExpiryOutcome::Commit(commit)),
                    Err(_) => self
                        .by_device
                        .remove(&device_id)
                        .map(ForwardingExpiryOutcome::Cancel),
                }
            })
            .collect()
    }

    pub fn begin_commit(
        &mut self,
        device_id: &DeviceId,
        entry_id: ForwardingEntryId,
    ) -> Result<ForwardingCommit, ForwardingRejection> {
        self.exact_mut(device_id, entry_id)?.begin_commit()
    }

    pub fn settle_collection_write(
        &mut self,
        device_id: &DeviceId,
        entry_id: ForwardingEntryId,
        outcome: ForwardingWriteOutcome,
    ) -> Result<ForwardingWriteOutcome, ForwardingRejection> {
        if self.by_device.get(device_id).is_none_or(|entry| {
            entry.id != entry_id || entry.phase != ForwardingEntryPhase::Collecting
        }) {
            return Err(ForwardingRejection::Conflict);
        }
        if outcome == ForwardingWriteOutcome::Failed {
            self.by_device.remove(device_id);
        }
        Ok(outcome)
    }

    pub fn settle_terminal_write(
        &mut self,
        device_id: &DeviceId,
        entry_id: ForwardingEntryId,
        outcome: ForwardingWriteOutcome,
    ) -> Result<ForwardingWriteOutcome, ForwardingRejection> {
        if self.by_device.get(device_id).is_none_or(|entry| {
            entry.id != entry_id || entry.phase != ForwardingEntryPhase::Committing
        }) {
            return Err(ForwardingRejection::Conflict);
        }
        if outcome == ForwardingWriteOutcome::Failed {
            self.by_device.remove(device_id);
        }
        Ok(outcome)
    }

    pub fn commit(
        &mut self,
        device_id: &DeviceId,
        entry_id: ForwardingEntryId,
    ) -> Result<ForwardingEntry, ForwardingRejection> {
        if self.by_device.get(device_id).is_none_or(|entry| {
            entry.id != entry_id || entry.phase != ForwardingEntryPhase::Committing
        }) {
            return Err(ForwardingRejection::Conflict);
        }
        Ok(self
            .by_device
            .remove(device_id)
            .expect("exact forwarding entry was checked"))
    }

    pub fn cancel(
        &mut self,
        device_id: &DeviceId,
        entry_id: ForwardingEntryId,
    ) -> Result<ForwardingEntry, ForwardingRejection> {
        if self
            .by_device
            .get(device_id)
            .is_none_or(|entry| entry.id != entry_id)
        {
            return Err(ForwardingRejection::Conflict);
        }
        Ok(self
            .by_device
            .remove(device_id)
            .expect("exact forwarding entry was checked"))
    }

    pub fn cancel_collection(
        &mut self,
        device_id: &DeviceId,
        entry_id: ForwardingEntryId,
    ) -> Result<ForwardingEntry, ForwardingRejection> {
        if self.by_device.get(device_id).is_none_or(|entry| {
            entry.id != entry_id || entry.phase != ForwardingEntryPhase::Collecting
        }) {
            return Err(ForwardingRejection::Conflict);
        }
        Ok(self
            .by_device
            .remove(device_id)
            .expect("exact forwarding collection was checked"))
    }

    fn exact_mut(
        &mut self,
        device_id: &DeviceId,
        entry_id: ForwardingEntryId,
    ) -> Result<&mut ForwardingEntry, ForwardingRejection> {
        self.by_device
            .get_mut(device_id)
            .filter(|entry| entry.id == entry_id)
            .ok_or(ForwardingRejection::Conflict)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NoAnswerTimerId(pub u64);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NoAnswerTimer {
    pub id: NoAnswerTimerId,
    pub call_id: PbxCallId,
    pub deadline: Instant,
    pub phase: NoAnswerTimerPhase,
    pub context: ForwardingContext,
    pub destination: ForwardingDestination,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NoAnswerTimerPhase {
    Pending,
    Claimed,
}

#[derive(Debug, Default)]
pub struct NoAnswerTimerRegistry {
    next_id: u64,
    by_call: HashMap<PbxCallId, NoAnswerTimer>,
}

impl NoAnswerTimerRegistry {
    pub fn schedule(
        &mut self,
        call_id: PbxCallId,
        deadline: Instant,
        context: ForwardingContext,
        destination: ForwardingDestination,
    ) -> Result<NoAnswerTimer, ForwardingRejection> {
        if self.by_call.contains_key(&call_id) {
            return Err(ForwardingRejection::Conflict);
        }
        let id = NoAnswerTimerId(
            self.next_id
                .checked_add(1)
                .ok_or(ForwardingRejection::IdentifierExhausted)?,
        );
        self.next_id = id.0;
        let timer = NoAnswerTimer {
            id,
            call_id,
            deadline,
            phase: NoAnswerTimerPhase::Pending,
            context,
            destination,
        };
        self.by_call.insert(call_id, timer.clone());
        Ok(timer)
    }

    pub fn cancel(
        &mut self,
        call_id: PbxCallId,
        timer_id: NoAnswerTimerId,
    ) -> Result<NoAnswerTimer, ForwardingRejection> {
        if self
            .by_call
            .get(&call_id)
            .is_none_or(|timer| timer.id != timer_id)
        {
            return Err(ForwardingRejection::Conflict);
        }
        Ok(self
            .by_call
            .remove(&call_id)
            .expect("exact no-answer timer was checked"))
    }

    pub fn cancel_pending(
        &mut self,
        call_id: PbxCallId,
        timer_id: NoAnswerTimerId,
    ) -> Result<NoAnswerTimer, ForwardingRejection> {
        if self
            .by_call
            .get(&call_id)
            .is_none_or(|timer| timer.id != timer_id || timer.phase != NoAnswerTimerPhase::Pending)
        {
            return Err(ForwardingRejection::Conflict);
        }
        Ok(self
            .by_call
            .remove(&call_id)
            .expect("exact pending no-answer timer was checked"))
    }

    pub fn claim_expired(&mut self, now: Instant) -> Vec<NoAnswerTimer> {
        let mut expired = self
            .by_call
            .values()
            .filter(|timer| timer.phase == NoAnswerTimerPhase::Pending && timer.deadline <= now)
            .map(|timer| (timer.deadline, timer.id, timer.call_id))
            .collect::<Vec<_>>();
        expired.sort_unstable_by_key(|(deadline, id, call_id)| (*deadline, *id, call_id.0));
        expired
            .into_iter()
            .filter_map(|(_, id, call_id)| {
                let timer = self.by_call.get_mut(&call_id)?;
                if timer.id != id || timer.phase != NoAnswerTimerPhase::Pending {
                    return None;
                }
                timer.phase = NoAnswerTimerPhase::Claimed;
                Some(timer.clone())
            })
            .collect()
    }

    pub fn commit(
        &mut self,
        call_id: PbxCallId,
        timer_id: NoAnswerTimerId,
    ) -> Result<NoAnswerTimer, ForwardingRejection> {
        if self
            .by_call
            .get(&call_id)
            .is_none_or(|timer| timer.id != timer_id || timer.phase != NoAnswerTimerPhase::Claimed)
        {
            return Err(ForwardingRejection::Conflict);
        }
        Ok(self
            .by_call
            .remove(&call_id)
            .expect("exact claimed no-answer timer was checked"))
    }

    pub fn rollback_claim(
        &mut self,
        call_id: PbxCallId,
        timer_id: NoAnswerTimerId,
    ) -> Result<(), ForwardingRejection> {
        let timer = self
            .by_call
            .get_mut(&call_id)
            .filter(|timer| timer.id == timer_id && timer.phase == NoAnswerTimerPhase::Claimed)
            .ok_or(ForwardingRejection::Conflict)?;
        timer.phase = NoAnswerTimerPhase::Pending;
        Ok(())
    }

    pub fn get(&self, call_id: PbxCallId) -> Option<&NoAnswerTimer> {
        self.by_call.get(&call_id)
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ForwardingRejection {
    #[error("forwarding operation conflicts with current state")]
    Conflict,
    #[error("forwarding operation is not valid in the current phase")]
    InvalidPhase,
    #[error("forwarding destination is missing or invalid")]
    InvalidDestination,
    #[error("forwarding destination exceeds the protocol bound")]
    DestinationTooLong,
    #[error("forwarding dialplan context is missing or invalid")]
    InvalidContext,
    #[error("forwarding operation identifier space is exhausted")]
    IdentifierExhausted,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn device(number: u8) -> DeviceId {
        DeviceId::new(format!("SEP0011223344{number:02}")).unwrap()
    }

    fn entry_timing(now: Instant) -> ForwardingEntryTiming {
        ForwardingEntryTiming {
            now,
            first_digit_timeout: Duration::from_secs(16),
            interdigit_timeout: Duration::from_secs(8),
        }
    }

    #[test]
    fn destination_enforces_legacy_wire_bound_without_debug_disclosure() {
        let destination = ForwardingDestination::new(" 123*45# ").unwrap();
        assert_eq!(destination.as_str(), "123*45#");
        assert!(!format!("{destination:?}").contains("123"));
        assert_eq!(
            ForwardingDestination::new(""),
            Err(ForwardingRejection::InvalidDestination)
        );
        assert_eq!(
            ForwardingDestination::new("1".repeat(MAX_FORWARD_DESTINATION_BYTES + 1)),
            Err(ForwardingRejection::DestinationTooLong)
        );
        assert_eq!(
            ForwardingDestination::new("12\n34"),
            Err(ForwardingRejection::InvalidDestination)
        );
    }

    #[test]
    fn forwarding_entry_debug_exposes_safe_timing_but_not_digits() {
        let now = Instant::now();
        let device_id = device(1);
        let mut entries = ForwardingEntryRegistry::default();
        let entry = entries
            .begin(
                device_id.clone(),
                1,
                CallId(9),
                ForwardingKind::All,
                Digit::Pound,
                entry_timing(now),
            )
            .unwrap();
        entries
            .replace_digits(&device_id, entry.id, "5551212", now)
            .unwrap();
        let debug = format!("{:?}", entries.get(&device_id).unwrap());
        assert!(debug.contains("dial_terminator"));
        assert!(debug.contains("first_digit_timeout"));
        assert!(debug.contains("interdigit_timeout"));
        assert!(debug.contains("<redacted:7 bytes>"));
        assert!(debug.ends_with(".. }"));
        assert!(!debug.contains("5551212"));
    }

    #[test]
    fn context_is_bounded_and_debug_redacted() {
        let context = ForwardingContext::new(" from-sccp ").unwrap();
        assert_eq!(context.as_str(), "from-sccp");
        assert!(!format!("{context:?}").contains("from-sccp"));
        assert_eq!(
            ForwardingContext::new(""),
            Err(ForwardingRejection::InvalidContext)
        );
        assert_eq!(
            ForwardingContext::new("x".repeat(MAX_FORWARD_CONTEXT_BYTES + 1)),
            Err(ForwardingRejection::InvalidContext)
        );
        assert_eq!(
            ForwardingContext::new("from\nsccp"),
            Err(ForwardingRejection::InvalidContext)
        );
    }

    #[test]
    fn handset_entry_is_device_scoped_bounded_and_generation_checked() {
        let mut entries = ForwardingEntryRegistry::default();
        let now = Instant::now();
        let first = entries
            .begin(
                device(1),
                1,
                CallId(10),
                ForwardingKind::All,
                Digit::D,
                entry_timing(now),
            )
            .unwrap();
        assert_eq!(
            entries.begin(
                device(1),
                2,
                CallId(11),
                ForwardingKind::Busy,
                Digit::D,
                entry_timing(now),
            ),
            Err(ForwardingRejection::Conflict)
        );
        for digit in [
            Digit::Number(1),
            Digit::Number(2),
            Digit::Star,
            Digit::Pound,
        ] {
            entries
                .input_digit(&device(1), first.id, digit, now)
                .unwrap();
        }
        entries.backspace(&device(1), first.id, now).unwrap();
        let commit = entries.begin_commit(&device(1), first.id).unwrap();
        assert_eq!(commit.destination.as_str(), "12*");
        assert_eq!(
            entries.input_digit(&device(1), first.id, Digit::Number(3), now),
            Err(ForwardingRejection::InvalidPhase)
        );
        entries.commit(&device(1), first.id).unwrap();

        let retry = entries
            .begin(
                device(1),
                1,
                CallId(11),
                ForwardingKind::Busy,
                Digit::Pound,
                entry_timing(now),
            )
            .unwrap();
        assert!(retry.id > first.id);
        assert_eq!(
            entries.cancel(&device(1), first.id),
            Err(ForwardingRejection::Conflict)
        );
        assert_eq!(entries.get(&device(1)).unwrap().id, retry.id);
    }

    #[test]
    fn empty_entry_and_exact_overflow_fail_without_losing_collection() {
        let mut entries = ForwardingEntryRegistry::default();
        let now = Instant::now();
        let entry = entries
            .begin(
                device(1),
                1,
                CallId(10),
                ForwardingKind::NoAnswer,
                Digit::Pound,
                entry_timing(now),
            )
            .unwrap();
        assert_eq!(
            entries.begin_commit(&device(1), entry.id),
            Err(ForwardingRejection::InvalidDestination)
        );
        for _ in 0..MAX_FORWARD_DESTINATION_BYTES {
            entries
                .input_digit(&device(1), entry.id, Digit::Number(9), now)
                .unwrap();
        }
        assert_eq!(
            entries.input_digit(&device(1), entry.id, Digit::Number(9), now),
            Err(ForwardingRejection::DestinationTooLong)
        );
        assert_eq!(
            entries.for_call(CallId(10)).unwrap().digits().len(),
            MAX_FORWARD_DESTINATION_BYTES
        );
    }

    #[test]
    fn configured_terminator_commits_without_becoming_part_of_the_destination() {
        let mut entries = ForwardingEntryRegistry::default();
        let now = Instant::now();
        let entry = entries
            .begin(
                device(1),
                1,
                CallId(10),
                ForwardingKind::All,
                Digit::Pound,
                entry_timing(now),
            )
            .unwrap();
        assert_eq!(
            entries.input_digit(&device(1), entry.id, Digit::Number(2), now),
            Ok(ForwardingDigitOutcome::Collected)
        );
        let ForwardingDigitOutcome::Commit(commit) = entries
            .input_digit(&device(1), entry.id, Digit::Pound, now)
            .unwrap()
        else {
            panic!("dial terminator did not commit forwarding")
        };
        assert_eq!(commit.destination.as_str(), "2");
        assert_eq!(
            entries.get(&device(1)).unwrap().phase,
            ForwardingEntryPhase::Committing
        );
        assert_eq!(
            entries.cancel_collection(&device(1), entry.id),
            Err(ForwardingRejection::Conflict),
            "handset teardown must not cancel an in-flight commit"
        );

        entries.cancel(&device(1), entry.id).unwrap();
        let enbloc = entries
            .begin(
                device(1),
                1,
                CallId(11),
                ForwardingKind::All,
                Digit::Pound,
                entry_timing(now),
            )
            .unwrap();
        entries
            .replace_digits(&device(1), enbloc.id, "2001#", now)
            .unwrap();
        assert_eq!(
            entries
                .begin_commit(&device(1), enbloc.id)
                .unwrap()
                .destination
                .as_str(),
            "2001"
        );
    }

    #[test]
    fn populated_interdigit_timeout_claims_one_commit_while_empty_timeout_cancels() {
        let mut entries = ForwardingEntryRegistry::default();
        let now = Instant::now();
        let populated = entries
            .begin(
                device(1),
                1,
                CallId(10),
                ForwardingKind::All,
                Digit::Pound,
                entry_timing(now),
            )
            .unwrap();
        entries
            .input_digit(
                &device(1),
                populated.id,
                Digit::Number(2),
                now + Duration::from_secs(1),
            )
            .unwrap();
        assert!(
            entries
                .claim_expired(now + Duration::from_secs(8))
                .is_empty()
        );
        let expired = entries.claim_expired(now + Duration::from_secs(9));
        let [ForwardingExpiryOutcome::Commit(commit)] = expired.as_slice() else {
            panic!("populated forwarding entry did not become a commit")
        };
        assert_eq!(commit.entry_id, populated.id);
        assert_eq!(commit.destination.as_str(), "2");
        assert!(
            entries
                .claim_expired(now + Duration::from_secs(30))
                .is_empty(),
            "a committing generation must not be claimed twice"
        );

        entries.cancel(&device(1), populated.id).unwrap();
        let empty = entries
            .begin(
                device(1),
                1,
                CallId(11),
                ForwardingKind::Busy,
                Digit::Pound,
                entry_timing(now),
            )
            .unwrap();
        assert!(matches!(
            entries
                .claim_expired(now + Duration::from_secs(16))
                .as_slice(),
            [ForwardingExpiryOutcome::Cancel(entry)] if entry.id == empty.id
        ));
        assert!(entries.get(&device(1)).is_none());
    }

    #[test]
    fn entry_deadlines_snapshot_first_and_interdigit_policy_and_expire_exactly() {
        let now = Instant::now();
        let mut entries = ForwardingEntryRegistry::default();
        let entry = entries
            .begin(
                device(1),
                1,
                CallId(10),
                ForwardingKind::All,
                Digit::Pound,
                entry_timing(now),
            )
            .unwrap();
        assert_eq!(entry.deadline, now + Duration::from_secs(16));
        assert!(
            entries
                .claim_expired(now + Duration::from_secs(15))
                .is_empty()
        );
        entries
            .input_digit(
                &device(1),
                entry.id,
                Digit::Number(2),
                now + Duration::from_secs(10),
            )
            .unwrap();
        assert_eq!(
            entries.get(&device(1)).unwrap().deadline,
            now + Duration::from_secs(18)
        );
        entries
            .backspace(&device(1), entry.id, now + Duration::from_secs(11))
            .unwrap();
        assert_eq!(
            entries.get(&device(1)).unwrap().deadline,
            now + Duration::from_secs(19)
        );
        entries
            .backspace(&device(1), entry.id, now + Duration::from_secs(12))
            .unwrap();
        assert_eq!(
            entries.get(&device(1)).unwrap().deadline,
            now + Duration::from_secs(28)
        );
        assert!(
            entries
                .claim_expired(now + Duration::from_secs(27))
                .is_empty()
        );
        assert_eq!(
            entries.claim_expired(now + Duration::from_secs(28)),
            [ForwardingExpiryOutcome::Cancel(ForwardingEntry {
                deadline: now + Duration::from_secs(28),
                ..entry
            })]
        );

        let committing = entries
            .begin(
                device(1),
                1,
                CallId(11),
                ForwardingKind::Busy,
                Digit::Pound,
                entry_timing(now),
            )
            .unwrap();
        entries
            .replace_digits(
                &device(1),
                committing.id,
                "1234",
                now + Duration::from_secs(1),
            )
            .unwrap();
        entries.begin_commit(&device(1), committing.id).unwrap();
        assert!(
            entries
                .claim_expired(now + Duration::from_secs(30))
                .is_empty()
        );
        assert_eq!(entries.get(&device(1)).unwrap().id, committing.id);
    }

    #[test]
    fn confirmed_writer_failures_cancel_only_the_exact_collection_generation() {
        let now = Instant::now();
        let mut entries = ForwardingEntryRegistry::default();
        let failed_begin = entries
            .begin(
                device(1),
                1,
                CallId(10),
                ForwardingKind::All,
                Digit::Pound,
                entry_timing(now),
            )
            .unwrap();
        assert_eq!(
            entries.settle_collection_write(
                &device(1),
                failed_begin.id,
                ForwardingWriteOutcome::Failed,
            ),
            Ok(ForwardingWriteOutcome::Failed)
        );
        assert!(entries.get(&device(1)).is_none());

        let failed_prompt = entries
            .begin(
                device(2),
                1,
                CallId(12),
                ForwardingKind::Busy,
                Digit::Pound,
                entry_timing(now),
            )
            .unwrap();
        assert_eq!(
            entries.settle_collection_write(
                &device(2),
                failed_prompt.id,
                ForwardingWriteOutcome::Written,
            ),
            Ok(ForwardingWriteOutcome::Written)
        );
        assert_eq!(
            entries.settle_collection_write(
                &device(2),
                failed_prompt.id,
                ForwardingWriteOutcome::Failed,
            ),
            Ok(ForwardingWriteOutcome::Failed)
        );
        assert!(entries.get(&device(2)).is_none());

        let retry = entries
            .begin(
                device(1),
                1,
                CallId(11),
                ForwardingKind::All,
                Digit::Pound,
                entry_timing(now),
            )
            .unwrap();
        assert_eq!(
            entries.settle_collection_write(
                &device(1),
                failed_begin.id,
                ForwardingWriteOutcome::Failed,
            ),
            Err(ForwardingRejection::Conflict)
        );
        assert_eq!(
            entries.settle_collection_write(
                &device(1),
                failed_begin.id,
                ForwardingWriteOutcome::Written,
            ),
            Err(ForwardingRejection::Conflict)
        );
        assert_eq!(entries.get(&device(1)).unwrap().id, retry.id);

        entries
            .replace_digits(&device(1), retry.id, "2000", now)
            .unwrap();
        entries.begin_commit(&device(1), retry.id).unwrap();
        assert_eq!(
            entries.settle_terminal_write(&device(1), retry.id, ForwardingWriteOutcome::Failed,),
            Ok(ForwardingWriteOutcome::Failed)
        );
        assert!(entries.get(&device(1)).is_none());
    }

    #[test]
    fn no_answer_timers_are_claimed_ordered_and_generation_safe() {
        let now = Instant::now();
        let mut timers = NoAnswerTimerRegistry::default();
        let later = timers
            .schedule(
                PbxCallId(2),
                now + Duration::from_secs(2),
                ForwardingContext::new("from-sccp").unwrap(),
                ForwardingDestination::new("2002").unwrap(),
            )
            .unwrap();
        let earlier = timers
            .schedule(
                PbxCallId(1),
                now + Duration::from_secs(1),
                ForwardingContext::new("from-sccp").unwrap(),
                ForwardingDestination::new("2001").unwrap(),
            )
            .unwrap();
        let earlier_id = earlier.id;
        let claimed_earlier = NoAnswerTimer {
            phase: NoAnswerTimerPhase::Claimed,
            ..earlier
        };
        assert_eq!(
            timers
                .claim_expired(now + Duration::from_secs(1))
                .as_slice(),
            std::slice::from_ref(&claimed_earlier)
        );
        assert!(
            timers
                .claim_expired(now + Duration::from_secs(1))
                .is_empty()
        );
        assert_eq!(
            timers.cancel_pending(PbxCallId(1), claimed_earlier.id),
            Err(ForwardingRejection::Conflict)
        );
        timers
            .rollback_claim(PbxCallId(1), claimed_earlier.id)
            .unwrap();
        assert_eq!(
            timers
                .claim_expired(now + Duration::from_secs(1))
                .as_slice(),
            std::slice::from_ref(&claimed_earlier)
        );
        assert_eq!(
            timers.cancel(PbxCallId(2), earlier_id),
            Err(ForwardingRejection::Conflict)
        );
        assert_eq!(timers.get(PbxCallId(2)).unwrap(), &later);
        assert_eq!(
            timers.commit(PbxCallId(2), later.id),
            Err(ForwardingRejection::Conflict)
        );
        assert_eq!(
            timers.commit(PbxCallId(1), claimed_earlier.id).unwrap(),
            claimed_earlier
        );
        let claimed_later = NoAnswerTimer {
            phase: NoAnswerTimerPhase::Claimed,
            ..later
        };
        assert_eq!(
            timers
                .claim_expired(now + Duration::from_secs(3))
                .as_slice(),
            std::slice::from_ref(&claimed_later)
        );
        assert_eq!(
            timers.commit(PbxCallId(1), earlier_id),
            Err(ForwardingRejection::Conflict)
        );
        assert_eq!(timers.get(PbxCallId(2)).unwrap(), &claimed_later);
        timers.commit(PbxCallId(2), claimed_later.id).unwrap();
        assert!(
            timers
                .claim_expired(now + Duration::from_secs(4))
                .is_empty()
        );
    }

    #[test]
    fn stale_cross_call_and_failed_answer_preserve_pending_timer_until_commit() {
        let now = Instant::now();
        let mut timers = NoAnswerTimerRegistry::default();
        let answered = timers
            .schedule(
                PbxCallId(1),
                now + Duration::from_secs(10),
                ForwardingContext::new("from-sccp").unwrap(),
                ForwardingDestination::new("private-2001").unwrap(),
            )
            .unwrap();
        let other = timers
            .schedule(
                PbxCallId(2),
                now + Duration::from_secs(10),
                ForwardingContext::new("from-sccp").unwrap(),
                ForwardingDestination::new("private-2002").unwrap(),
            )
            .unwrap();

        // Stale ownership and an answer which fails before commit do not call
        // `cancel_pending`; the valid timer remains claimable. Only the exact
        // generation of a successfully committed answer is retired.
        assert_eq!(timers.get(PbxCallId(1)), Some(&answered));
        assert_eq!(timers.get(PbxCallId(2)), Some(&other));
        assert_eq!(
            timers.cancel_pending(PbxCallId(2), answered.id),
            Err(ForwardingRejection::Conflict)
        );
        assert_eq!(timers.get(PbxCallId(1)), Some(&answered));
        assert_eq!(
            timers.cancel_pending(PbxCallId(1), answered.id).unwrap(),
            answered
        );
        assert_eq!(timers.get(PbxCallId(2)), Some(&other));
        assert!(
            timers
                .claim_expired(now + Duration::from_secs(9))
                .is_empty()
        );
        assert_eq!(
            timers
                .claim_expired(now + Duration::from_secs(10))
                .as_slice(),
            &[NoAnswerTimer {
                phase: NoAnswerTimerPhase::Claimed,
                ..other
            }]
        );
    }

    #[test]
    fn no_answer_timer_snapshots_route_and_deadline_across_policy_reload() {
        let now = Instant::now();
        let mut timers = NoAnswerTimerRegistry::default();
        let scheduled = timers
            .schedule(
                PbxCallId(1),
                now + Duration::from_secs(12),
                ForwardingContext::new("original-context").unwrap(),
                ForwardingDestination::new("private-original").unwrap(),
            )
            .unwrap();

        let _reloaded_context = ForwardingContext::new("reloaded-context").unwrap();
        let _reloaded_destination = ForwardingDestination::new("private-reloaded").unwrap();
        assert_eq!(timers.get(PbxCallId(1)), Some(&scheduled));
        assert!(
            timers
                .claim_expired(now + Duration::from_secs(11))
                .is_empty()
        );
        assert_eq!(
            timers
                .claim_expired(now + Duration::from_secs(12))
                .first()
                .unwrap(),
            &NoAnswerTimer {
                phase: NoAnswerTimerPhase::Claimed,
                ..scheduled
            }
        );
    }

    #[test]
    fn registries_fail_closed_when_identifier_space_is_exhausted() {
        let mut entries = ForwardingEntryRegistry {
            next_id: u64::MAX,
            ..Default::default()
        };
        assert_eq!(
            entries.begin(
                device(1),
                1,
                CallId(10),
                ForwardingKind::All,
                Digit::Pound,
                entry_timing(Instant::now()),
            ),
            Err(ForwardingRejection::IdentifierExhausted)
        );
        assert!(entries.get(&device(1)).is_none());

        let mut timers = NoAnswerTimerRegistry {
            next_id: u64::MAX,
            ..Default::default()
        };
        assert_eq!(
            timers.schedule(
                PbxCallId(1),
                Instant::now(),
                ForwardingContext::new("from-sccp").unwrap(),
                ForwardingDestination::new("2001").unwrap(),
            ),
            Err(ForwardingRejection::IdentifierExhausted)
        );
        assert!(timers.get(PbxCallId(1)).is_none());
    }
}
