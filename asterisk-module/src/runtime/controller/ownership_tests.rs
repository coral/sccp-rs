use std::sync::{Arc, Barrier, mpsc};
use std::thread;
use std::time::Duration;

use super::*;
use crate::runtime::controller::tests::shared_inbound_controller;

struct RunningOwner {
    handle: ControllerHandle,
    worker: Option<thread::JoinHandle<()>>,
}

impl RunningOwner {
    fn new(controller: Controller) -> Self {
        let (handle, owner) = ControllerOwner::new(controller).unwrap();
        let worker = thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(owner.run());
        });
        Self {
            handle,
            worker: Some(worker),
        }
    }
}

impl Drop for RunningOwner {
    fn drop(&mut self) {
        self.handle.close();
        self.worker.take().unwrap().join().unwrap();
    }
}

#[tokio::test]
async fn call_retirement_prepares_during_stalled_native_work_but_cleanup_and_tone_wait() {
    use crate::runtime::call_queue::{CallEffect, CallExecutor, CallQueue, call_queue};
    use tokio::task::{AbortHandle, JoinSet};

    enum Command {
        Native {
            entered: tokio::sync::oneshot::Sender<()>,
            release: mpsc::Receiver<()>,
        },
        Hangup,
        Prepared(RemoteHangupToken),
    }
    struct Executor {
        controller: ControllerHandle,
        retired: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
        order: Arc<Mutex<Vec<&'static str>>>,
    }
    impl CallExecutor<PbxCallId, Command> for Executor {
        fn prepare_terminal(
            &self,
            key: PbxCallId,
            command: Command,
        ) -> impl std::future::Future<Output = Command> + Send + 'static {
            let controller = self.controller.clone();
            let retired = Arc::clone(&self.retired);
            async move {
                assert!(matches!(command, Command::Hangup));
                let (_, plan, _) = controller
                    .prepare_remote_hangup_async(
                        key,
                        Some(Tone::Zip),
                        Duration::from_secs(15),
                        Instant::now(),
                    )
                    .await
                    .unwrap();
                let token = plan.unwrap().pending.unwrap();
                retired.lock().unwrap().take().unwrap().send(()).unwrap();
                Command::Prepared(token)
            }
        }

        fn spawn(
            &self,
            effect: CallEffect<PbxCallId, Command>,
            workers: &mut JoinSet<()>,
        ) -> AbortHandle {
            let controller = self.controller.clone();
            let order = Arc::clone(&self.order);
            workers.spawn_blocking(move || match effect.command {
                Command::Native { entered, release } => {
                    entered.send(()).unwrap();
                    release.recv().unwrap();
                    // A late acknowledgement from the accepted native job
                    // cannot recreate the call removed by terminal preparation.
                    assert!(controller.pbx_answer(effect.key).unwrap().is_empty());
                    order.lock().unwrap().push("native completed");
                }
                Command::Prepared(token) => {
                    assert!(
                        controller
                            .activate_remote_hangup(token, Duration::from_secs(15), Instant::now())
                            .unwrap()
                    );
                    order.lock().unwrap().push("terminal cleanup");
                }
                Command::Hangup => panic!("unprepared terminal delivery"),
            })
        }
    }

    let mut controller = shared_inbound_controller();
    controller.phone_answer(CallId(2));
    let owner = RunningOwner::new(controller);
    let (handle, mailbox) = call_queue(2);
    let call = handle.admit(PbxCallId(8), Command::Hangup).unwrap();
    let (entered, entered_rx) = tokio::sync::oneshot::channel();
    let (release, released) = mpsc::channel();
    call.try_send(
        Command::Native {
            entered,
            release: released,
        },
        None,
    )
    .unwrap();
    let (retired, retired_rx) = tokio::sync::oneshot::channel();
    let order = Arc::new(Mutex::new(Vec::new()));
    let queue = CallQueue::new(
        mailbox,
        Executor {
            controller: owner.handle.clone(),
            retired: Arc::new(Mutex::new(Some(retired))),
            order: Arc::clone(&order),
        },
    );
    let task = tokio::spawn(queue.run());
    entered_rx.await.unwrap();
    call.retire(Command::Hangup);
    call.retire(Command::Hangup);
    tokio::time::timeout(Duration::from_secs(1), retired_rx)
        .await
        .unwrap()
        .unwrap();
    assert!(
        owner
            .handle
            .snapshot()
            .active_or_primary_call_by_pbx(PbxCallId(8))
            .is_none()
    );
    assert!(
        owner
            .handle
            .expire_remote_hangups_async(Instant::now() + Duration::from_secs(60))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(order.lock().unwrap().is_empty());
    assert_eq!(handle.snapshot().outstanding, 2);
    handle.close();
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        *order.lock().unwrap(),
        ["native completed", "terminal cleanup"]
    );
    assert_eq!(handle.snapshot().outstanding, 0);
    assert_eq!(
        owner
            .handle
            .expire_remote_hangups_async(Instant::now() + Duration::from_secs(60))
            .await
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn published_snapshots_are_coherent_and_stalled_native_work_does_not_hold_state() {
    let owner = RunningOwner::new(Controller::new(Duration::from_secs(1)));
    let device = DeviceId::new("SEP001122334455").unwrap();
    let original = owner.handle.snapshot();
    let (prepared, ready) = mpsc::sync_channel(1);
    let (release, native_wait) = mpsc::sync_channel(1);
    let worker = {
        let handle = owner.handle.clone();
        let device = device.clone();
        thread::spawn(move || {
            let next = DeviceFeatureState {
                privacy: true,
                ..Default::default()
            };
            assert!(handle.commit_device_features(&device, None, next).unwrap());
            prepared.send(()).unwrap();
            // An owned preparation result can be held by a stalled callback.
            let _ = native_wait.recv();
        })
    };
    ready.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(original.feature_state(&device).is_none());
    let prepared = owner.handle.snapshot();
    assert!(prepared.feature_state(&device).unwrap().privacy);
    let expected = prepared.feature_state(&device).cloned();
    assert!(
        owner
            .handle
            .commit_device_features(&device, expected, DeviceFeatureState::default())
            .unwrap()
    );
    assert!(
        !owner
            .handle
            .snapshot()
            .feature_state(&device)
            .unwrap()
            .privacy
    );
    assert!(prepared.feature_state(&device).unwrap().privacy);
    assert!(
        !owner
            .handle
            .commit_device_features(&device, None, DeviceFeatureState::default())
            .unwrap()
    );
    release.send(()).unwrap();
    worker.join().unwrap();
}

#[test]
fn cancelled_deferred_hangup_presentation_cannot_be_reactivated_by_late_cleanup() {
    let mut controller = shared_inbound_controller();
    controller.phone_answer(CallId(2));
    let owner = RunningOwner::new(controller);
    let (_, plan, _) = owner
        .handle
        .prepare_remote_hangup(
            PbxCallId(8),
            Some(Tone::Zip),
            Duration::from_secs(15),
            Instant::now(),
        )
        .unwrap();
    let token = plan.unwrap().pending.unwrap();
    assert!(
        owner
            .handle
            .complete_remote_hangup_token(token)
            .unwrap()
            .is_some()
    );
    assert!(
        !owner
            .handle
            .activate_remote_hangup(token, Duration::from_secs(15), Instant::now())
            .unwrap()
    );
    assert!(
        owner
            .handle
            .prepare_remote_hangup(
                PbxCallId(8),
                Some(Tone::Zip),
                Duration::from_secs(15),
                Instant::now()
            )
            .unwrap()
            .1
            .is_none()
    );
}

#[test]
fn concurrent_shared_line_answer_and_hangup_leave_no_live_call_or_late_metadata() {
    for _ in 0..16 {
        let owner = RunningOwner::new(shared_inbound_controller());
        assert!(owner.handle.set_audio_packet_ms(PbxCallId(8), 20).unwrap());
        assert!(
            owner
                .handle
                .set_assigned_channel_id(PbxCallId(8), Some("fixture-call".into()))
                .unwrap()
        );
        let gate = Arc::new(Barrier::new(2));
        let answer = {
            let gate = Arc::clone(&gate);
            let handle = owner.handle.clone();
            thread::spawn(move || {
                gate.wait();
                handle.phone_answer(CallId(2))
            })
        };
        let hangup = {
            let handle = owner.handle.clone();
            thread::spawn(move || {
                gate.wait();
                handle.pbx_hangup_with_effects(PbxCallId(8)).unwrap()
            })
        };
        let _ = answer.join().unwrap();
        assert!(hangup.join().unwrap().is_some());
        let snapshot = owner.handle.snapshot();
        assert!(snapshot.pbx_call(PbxCallId(8)).is_none());
        assert!(snapshot.call_runtime_record(PbxCallId(8)).is_none());
        assert!(!owner.handle.set_audio_packet_ms(PbxCallId(8), 40).unwrap());
        assert!(
            owner
                .handle
                .snapshot()
                .call_runtime_record(PbxCallId(8))
                .is_none()
        );
    }
}

#[test]
fn controller_admission_failure_is_reported_without_applying_a_transition() {
    let (handle, owner) = ControllerOwner::new(Controller::new(Duration::from_secs(1))).unwrap();
    handle.close();
    let result = handle.set_audio_packet_ms(PbxCallId(8), 20);
    assert_eq!(
        result,
        Err(ControllerRequestError::Admission(AdmissionError::Closed))
    );
    assert_eq!(handle.diagnostics().outstanding, 0);
    drop(owner);
}

#[test]
fn reserved_hangup_runs_with_both_ordinary_and_lifetime_capacity_exhausted() {
    let (handle, owner) = ControllerOwner::with_capacity(shared_inbound_controller(), 4).unwrap();
    assert_eq!(owner.completion_sender.snapshot().outstanding, 4);
    let mut queued = Vec::new();
    for packet_ms in [10, 20, 30, 40] {
        let (reply, result) = mpsc::sync_channel(1);
        handle
            .commands
            .try_send(
                Box::new(ControllerCommand::SetAudioPacketMs {
                    pbx_id: PbxCallId(8),
                    packet_ms,
                    reply: ControllerReply::Sync(reply),
                }),
                None,
            )
            .unwrap();
        queued.push(result);
    }
    assert_eq!(
        handle.set_audio_packet_ms(PbxCallId(8), 50),
        Err(ControllerRequestError::Admission(AdmissionError::Full))
    );
    let (reply, hung_up) = mpsc::sync_channel(1);
    handle
        .submit(ControllerCommand::PbxHangupWithEffects {
            pbx_id: PbxCallId(8),
            reply: ControllerReply::Sync(reply),
        })
        .unwrap();
    let worker = thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(owner.run());
    });
    assert!(
        hung_up
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap()
            .is_some()
    );
    for result in queued {
        assert!(
            !result
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap()
        );
    }
    handle.close();
    worker.join().unwrap();
    assert_eq!(handle.diagnostics().outstanding, 0);
    assert!(handle.snapshot().pbx_call(PbxCallId(8)).is_none());
}

#[test]
fn missing_lifetime_capacity_rejects_preparation_before_creating_a_call() {
    let (handle, owner) =
        ControllerOwner::with_capacity(Controller::new(Duration::from_secs(1)), 1).unwrap();
    let worker = thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(owner.run());
    });
    let result = handle.prepare_phone_call(
        CallId(42),
        crate::runtime::controller::tests::binding_for("SEP001122334455", 1),
        Codec::Pcmu,
        Instant::now(),
    );
    assert!(matches!(
        result,
        Err(ControllerRequestError::Admission(AdmissionError::Full))
    ));
    assert!(handle.snapshot().call(CallId(42)).is_none());
    handle.close();
    worker.join().unwrap();
}

#[test]
fn retired_route_returns_accepted_cleanup_to_the_reserved_configuration_lane() {
    let owner = RunningOwner::new(shared_inbound_controller());
    let lane = owner
        .handle
        .routing
        .read()
        .unwrap()
        .get(&CompletionKey::Call(PbxCallId(8)))
        .unwrap()
        .clone();
    lane.retire();
    assert!(
        owner
            .handle
            .pbx_hangup_with_effects(PbxCallId(8))
            .unwrap()
            .is_some()
    );
    assert!(owner.handle.snapshot().pbx_call(PbxCallId(8)).is_none());
}

#[test]
fn replaced_session_rejects_old_disconnect_and_delayed_feature_restore() {
    let owner = RunningOwner::new(shared_inbound_controller());
    let device = DeviceId::new("SEP001122334455").unwrap();
    let old = owner
        .handle
        .snapshot()
        .registered_device(&device)
        .unwrap()
        .clone();
    let replacement = SessionGeneration::new(u64::from(old.session_generation) + 1).unwrap();
    assert!(
        owner
            .handle
            .prepare_register_session(replacement, old.registration)
            .unwrap()
            .is_some()
    );
    assert!(
        owner
            .handle
            .prepare_disconnect(device.clone(), old.session_generation)
            .unwrap()
            .is_none()
    );
    assert!(
        owner
            .handle
            .snapshot()
            .session_is_current(&device, replacement)
    );
    assert!(
        owner
            .handle
            .prepare_disconnect(device.clone(), replacement)
            .unwrap()
            .is_some()
    );
    assert!(
        !owner
            .handle
            .commit_registered_features(&device, replacement, DeviceFeatureState::default())
            .unwrap()
    );
}

#[test]
fn native_codec_preparation_is_private_and_late_commit_cannot_restore_a_retired_call() {
    let mut controller = shared_inbound_controller();
    controller.pbx_hangup_with_effects(PbxCallId(8));
    controller.begin_phone_call(
        CallId(42),
        crate::runtime::controller::tests::binding_for("SEP001122334455", 1),
        Codec::Pcmu,
        Instant::now(),
    );
    let pbx_id = controller.call(CallId(42)).unwrap().pbx_id;
    let owner = RunningOwner::new(controller);
    let mutation = owner
        .handle
        .prepare_pre_dial_codec(pbx_id, Codec::Pcma, Some(vec![PbxAudioFormat::G711Alaw]))
        .unwrap()
        .unwrap();
    assert_eq!(
        owner.handle.snapshot().call(CallId(42)).unwrap().codec,
        Codec::Pcmu
    );
    assert!(
        owner
            .handle
            .prepare_pre_dial_codec(pbx_id, Codec::G72264k, None)
            .unwrap()
            .is_err()
    );
    assert!(owner.handle.commit_codec_mutation(mutation).unwrap());
    assert_eq!(
        owner.handle.snapshot().call(CallId(42)).unwrap().codec,
        Codec::Pcma
    );
    assert_eq!(
        owner
            .handle
            .snapshot()
            .call_runtime_record(pbx_id)
            .unwrap()
            .audio_preferences,
        Some(vec![PbxAudioFormat::G711Alaw])
    );
    let late = owner
        .handle
        .prepare_pre_dial_codec(pbx_id, Codec::Pcmu, None)
        .unwrap()
        .unwrap();
    owner.handle.pbx_hangup_with_effects(pbx_id).unwrap();
    assert!(!owner.handle.commit_codec_mutation(late).unwrap());
    owner.handle.abort_codec_mutation(late).unwrap();
    assert!(
        owner
            .handle
            .snapshot()
            .call_runtime_record(pbx_id)
            .is_none()
    );
}

#[tokio::test]
async fn async_completion_waiters_yield_while_the_reserved_lane_is_busy() {
    let (handle, owner) = ControllerOwner::with_capacity(shared_inbound_controller(), 4).unwrap();
    let first = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.expire_call_deadlines_async(Instant::now()).await })
    };
    tokio::task::yield_now().await;
    let second = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.expire_remote_hangups_async(Instant::now()).await })
    };
    tokio::task::yield_now().await;
    assert!(!first.is_finished());
    assert!(!second.is_finished());
    // Both replies and the contended delivery reservation remain asynchronous
    // even when the owner shares this single Tokio executor thread.
    let worker = tokio::spawn(owner.run());
    tokio::time::timeout(Duration::from_secs(1), async {
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();
    })
    .await
    .unwrap();
    handle.close();
    worker.await.unwrap();
}

#[test]
fn admitted_mobility_response_finishes_with_ordinary_and_lifetime_capacity_full() {
    let mut controller = shared_inbound_controller();
    let device = DeviceId::new("SEP001122334455").unwrap();
    let slot = crate::call::mobility::MobilitySlot::new(device.clone(), 1).unwrap();
    let prompt = controller.reserve_mobility_prompt(slot.clone()).unwrap();
    let (handle, owner) = ControllerOwner::with_capacity(controller, 4).unwrap();
    for packet_ms in [10, 20, 30, 40] {
        let (reply, _result) = mpsc::sync_channel(1);
        handle
            .commands
            .try_send(
                Box::new(ControllerCommand::SetAudioPacketMs {
                    pbx_id: PbxCallId(8),
                    packet_ms,
                    reply: ControllerReply::Sync(reply),
                }),
                None,
            )
            .unwrap();
    }
    assert_eq!(handle.diagnostics().outstanding, 4);
    assert_eq!(handle.completion_diagnostics().outstanding, 4);
    let (reply, response) = mpsc::sync_channel(1);
    handle
        .submit(ControllerCommand::TakeMobilityPrompt {
            device: device.clone(),
            id: prompt.transaction_id,
            reply: ControllerReply::Sync(reply),
        })
        .unwrap();
    let worker = thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(owner.run());
    });
    assert_eq!(
        response
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap(),
        Some(slot)
    );
    assert!(
        handle
            .take_mobility_prompt(&device, prompt.transaction_id)
            .unwrap()
            .is_none()
    );
    handle.close();
    worker.join().unwrap();
}
