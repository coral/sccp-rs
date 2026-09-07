use std::collections::HashSet;
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicU64, Ordering};

use super::backend::PbxCallId;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AnnouncementFailureStage {
    Retarget,
    Tone,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AnnouncementStartFailure {
    pub stage: AnnouncementFailureStage,
    pub call_id: PbxCallId,
    pub compensation_failures: Vec<PbxCallId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AnnouncementGeneration(NonZeroU64);

pub(crate) const MAX_RESTORE_ATTEMPTS: u8 = 3;

pub(crate) const fn restore_attempts_exhausted(attempts: u8) -> bool {
    attempts >= MAX_RESTORE_ATTEMPTS
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct ReplacementAnchorPlan {
    pub retain_previous: HashSet<PbxCallId>,
    pub acquire: Vec<PbxCallId>,
}

pub(crate) fn replacement_anchor_plan(
    participants: &[PbxCallId],
    previous: &[PbxCallId],
    restore_failures: &HashSet<PbxCallId>,
) -> ReplacementAnchorPlan {
    let participant_set = participants.iter().copied().collect::<HashSet<_>>();
    let previous_set = previous.iter().copied().collect::<HashSet<_>>();
    let retain_previous = previous_set
        .intersection(&participant_set)
        .copied()
        .chain(previous_set.intersection(restore_failures).copied())
        .collect::<HashSet<_>>();
    let mut seen = HashSet::new();
    let acquire = participants
        .iter()
        .copied()
        .filter(|call_id| !previous_set.contains(call_id) && seen.insert(*call_id))
        .collect();
    ReplacementAnchorPlan {
        retain_previous,
        acquire,
    }
}

pub(crate) trait AnnouncementAdapter<C> {
    fn retarget_to_anchor(&mut self, call: &C) -> bool;
    fn retarget_to_direct(&mut self, call: &C) -> bool;
    fn start_tone(&mut self, call_id: PbxCallId) -> bool;
    fn stop_tone(&mut self, call_id: PbxCallId);
}

pub(crate) fn start_announcement<C: AnnouncementCall>(
    adapter: &mut impl AnnouncementAdapter<C>,
    inherited_announcement_calls: &[C],
    to_retarget: &[C],
    tone_calls: &[PbxCallId],
) -> Result<(), AnnouncementStartFailure> {
    let mut retargeted = Vec::with_capacity(to_retarget.len());
    for call in to_retarget {
        if !adapter.retarget_to_anchor(call) {
            let compensation_failures = inherited_announcement_calls
                .iter()
                .chain(retargeted)
                .filter_map(|call| (!adapter.retarget_to_direct(call)).then_some(call.call_id()))
                .collect();
            return Err(AnnouncementStartFailure {
                stage: AnnouncementFailureStage::Retarget,
                call_id: call.call_id(),
                compensation_failures,
            });
        }
        retargeted.push(call);
    }

    let mut started = Vec::with_capacity(tone_calls.len());
    for call_id in tone_calls {
        if !adapter.start_tone(*call_id) {
            for started_call in started {
                adapter.stop_tone(started_call);
            }
            let compensation_failures = inherited_announcement_calls
                .iter()
                .chain(to_retarget)
                .filter_map(|call| (!adapter.retarget_to_direct(call)).then_some(call.call_id()))
                .collect();
            return Err(AnnouncementStartFailure {
                stage: AnnouncementFailureStage::Tone,
                call_id: *call_id,
                compensation_failures,
            });
        }
        started.push(*call_id);
    }
    Ok(())
}

pub(crate) fn allocate_generation(next: &AtomicU64) -> Option<AnnouncementGeneration> {
    next.try_update(Ordering::Relaxed, Ordering::Relaxed, |generation| {
        NonZeroU64::new(generation)?;
        generation.checked_add(1)
    })
    .ok()
    .and_then(NonZeroU64::new)
    .map(AnnouncementGeneration)
}

pub(crate) trait AnnouncementCall {
    fn call_id(&self) -> PbxCallId;
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct Call(PbxCallId);

    impl AnnouncementCall for Call {
        fn call_id(&self) -> PbxCallId {
            self.0
        }
    }

    #[derive(Default)]
    struct FakeAdapter {
        fail_retarget: Option<PbxCallId>,
        fail_tone: Option<PbxCallId>,
        fail_restore: Option<PbxCallId>,
        anchored: HashSet<PbxCallId>,
        tones: HashSet<PbxCallId>,
        events: Vec<(&'static str, PbxCallId)>,
    }

    impl AnnouncementAdapter<Call> for FakeAdapter {
        fn retarget_to_anchor(&mut self, call: &Call) -> bool {
            self.events.push(("anchor", call.0));
            if self.fail_retarget == Some(call.0) {
                return false;
            }
            self.anchored.insert(call.0);
            true
        }

        fn retarget_to_direct(&mut self, call: &Call) -> bool {
            self.events.push(("direct", call.0));
            if self.fail_restore == Some(call.0) {
                return false;
            }
            self.anchored.remove(&call.0);
            true
        }

        fn start_tone(&mut self, call_id: PbxCallId) -> bool {
            self.events.push(("tone", call_id));
            if self.fail_tone == Some(call_id) {
                return false;
            }
            self.tones.insert(call_id);
            true
        }

        fn stop_tone(&mut self, call_id: PbxCallId) {
            self.events.push(("stop", call_id));
            self.tones.remove(&call_id);
        }
    }

    #[test]
    fn later_retarget_failure_compensates_every_completed_call() {
        let mut adapter = FakeAdapter {
            fail_retarget: Some(PbxCallId(3)),
            anchored: HashSet::from([PbxCallId(1)]),
            ..FakeAdapter::default()
        };
        let result = start_announcement(
            &mut adapter,
            &[Call(PbxCallId(1))],
            &[Call(PbxCallId(2)), Call(PbxCallId(3))],
            &[PbxCallId(1), PbxCallId(2), PbxCallId(3)],
        );

        assert_eq!(
            result,
            Err(AnnouncementStartFailure {
                stage: AnnouncementFailureStage::Retarget,
                call_id: PbxCallId(3),
                compensation_failures: Vec::new(),
            })
        );
        assert!(adapter.anchored.is_empty());
        assert!(adapter.tones.is_empty());
        assert_eq!(
            adapter.events,
            [
                ("anchor", PbxCallId(2)),
                ("anchor", PbxCallId(3)),
                ("direct", PbxCallId(1)),
                ("direct", PbxCallId(2)),
            ]
        );
    }

    #[test]
    fn tone_failure_stops_started_tones_and_restores_every_direct_call() {
        let mut adapter = FakeAdapter {
            fail_tone: Some(PbxCallId(3)),
            anchored: HashSet::from([PbxCallId(1)]),
            ..FakeAdapter::default()
        };
        let result = start_announcement(
            &mut adapter,
            &[Call(PbxCallId(1))],
            &[Call(PbxCallId(2)), Call(PbxCallId(3))],
            &[PbxCallId(1), PbxCallId(2), PbxCallId(3)],
        );

        assert_eq!(
            result,
            Err(AnnouncementStartFailure {
                stage: AnnouncementFailureStage::Tone,
                call_id: PbxCallId(3),
                compensation_failures: Vec::new(),
            })
        );
        assert!(adapter.anchored.is_empty());
        assert!(adapter.tones.is_empty());
        assert_eq!(
            adapter.events,
            [
                ("anchor", PbxCallId(2)),
                ("anchor", PbxCallId(3)),
                ("tone", PbxCallId(1)),
                ("tone", PbxCallId(2)),
                ("tone", PbxCallId(3)),
                ("stop", PbxCallId(1)),
                ("stop", PbxCallId(2)),
                ("direct", PbxCallId(1)),
                ("direct", PbxCallId(2)),
                ("direct", PbxCallId(3)),
            ]
        );
    }

    #[test]
    fn generation_exhaustion_does_not_wrap_or_mutate_the_counter() {
        let next = AtomicU64::new(u64::MAX);
        assert_eq!(allocate_generation(&next), None);
        assert_eq!(next.load(Ordering::Relaxed), u64::MAX);
        let zero = AtomicU64::new(0);
        assert_eq!(allocate_generation(&zero), None);
        assert_eq!(zero.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn restore_retry_limit_has_one_terminal_boundary() {
        assert!(!restore_attempts_exhausted(MAX_RESTORE_ATTEMPTS - 1));
        assert!(restore_attempts_exhausted(MAX_RESTORE_ATTEMPTS));
        assert!(restore_attempts_exhausted(u8::MAX));
    }

    #[test]
    fn compensation_failures_remain_explicit_and_inherited_calls_do_not_bounce_on_success() {
        let mut failure = FakeAdapter {
            fail_tone: Some(PbxCallId(2)),
            fail_restore: Some(PbxCallId(1)),
            anchored: HashSet::from([PbxCallId(1)]),
            ..FakeAdapter::default()
        };
        assert_eq!(
            start_announcement(
                &mut failure,
                &[Call(PbxCallId(1))],
                &[],
                &[PbxCallId(1), PbxCallId(2)],
            ),
            Err(AnnouncementStartFailure {
                stage: AnnouncementFailureStage::Tone,
                call_id: PbxCallId(2),
                compensation_failures: vec![PbxCallId(1)],
            })
        );
        assert!(failure.anchored.contains(&PbxCallId(1)));

        let mut replacement = FakeAdapter {
            anchored: HashSet::from([PbxCallId(1)]),
            ..FakeAdapter::default()
        };
        assert_eq!(
            start_announcement(
                &mut replacement,
                &[Call(PbxCallId(1))],
                &[],
                &[PbxCallId(1)],
            ),
            Ok(())
        );
        assert_eq!(replacement.events, [("tone", PbxCallId(1))]);
    }

    #[test]
    fn replacement_transfers_retained_ownership_and_acquires_only_missing_calls() {
        let plan = replacement_anchor_plan(
            &[PbxCallId(1), PbxCallId(2), PbxCallId(2)],
            &[PbxCallId(1), PbxCallId(3), PbxCallId(4)],
            &HashSet::from([PbxCallId(3)]),
        );

        assert_eq!(
            plan.retain_previous,
            HashSet::from([PbxCallId(1), PbxCallId(3)])
        );
        assert_eq!(plan.acquire, [PbxCallId(2)]);
        assert!(!plan.acquire.contains(&PbxCallId(1)));
        assert!(!plan.retain_previous.contains(&PbxCallId(4)));
    }
}
