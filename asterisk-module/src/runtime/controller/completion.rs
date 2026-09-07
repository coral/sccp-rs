//! Reusable completion delivery reserved before call or station admission.
//!
//! Each live resource retains one physical mailbox slot and its admission
//! budget. Sending temporarily lends it to the owner; consumption restores the
//! same budget and physical slot before another operation can use it. These
//! locks protect delivery only, never controller state. Waiters still own their
//! upstream call/operation permits, and the dedicated controller thread never
//! waits for them to perform native work.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};

use super::ownership::ControllerCommand;
use super::{Controller, DeviceId, PbxCallId};
use crate::runtime::mailbox::{AdmissionError, MailboxReservation, MailboxSender, WorkPermit};

#[derive(Clone, Eq, Hash, PartialEq)]
pub(super) enum CompletionKey {
    Call(PbxCallId),
    Device(DeviceId),
    Configuration,
}

pub(super) type CompletionRoutes = HashMap<CompletionKey, Arc<CompletionLane>>;

pub(super) struct CompletionRequest {
    pub command: Box<ControllerCommand>,
    pub lane: Arc<CompletionLane>,
}

struct Delivery {
    slot: Option<MailboxReservation<CompletionRequest>>,
    retired: bool,
}

pub(super) struct CompletionLane {
    delivery: Mutex<Delivery>,
    ready: Condvar,
    async_ready: tokio::sync::Notify,
}

impl CompletionLane {
    pub fn new(slot: MailboxReservation<CompletionRequest>) -> Arc<Self> {
        Arc::new(Self {
            delivery: Mutex::new(Delivery {
                slot: Some(slot),
                retired: false,
            }),
            ready: Condvar::new(),
            async_ready: tokio::sync::Notify::new(),
        })
    }

    pub fn send(
        self: &Arc<Self>,
        command: Box<ControllerCommand>,
    ) -> Result<(), Box<ControllerCommand>> {
        let mut delivery = self
            .delivery
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if delivery.retired {
                return Err(command);
            }
            if let Some(slot) = delivery.slot.take() {
                drop(delivery);
                let _ = slot.send(
                    CompletionRequest {
                        command,
                        lane: Arc::clone(self),
                    },
                    None,
                );
                return Ok(());
            }
            delivery = self
                .ready
                .wait(delivery)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    pub async fn send_async(
        self: &Arc<Self>,
        command: Box<ControllerCommand>,
    ) -> Result<(), Box<ControllerCommand>> {
        loop {
            let ready = self.async_ready.notified();
            tokio::pin!(ready);
            ready.as_mut().enable();
            let slot = {
                let mut delivery = self
                    .delivery
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if delivery.retired {
                    return Err(command);
                }
                delivery.slot.take()
            };
            if let Some(slot) = slot {
                let _ = slot.send(
                    CompletionRequest {
                        command,
                        lane: Arc::clone(self),
                    },
                    None,
                );
                return Ok(());
            }
            ready.await;
        }
    }

    pub fn recycle(&self, sender: &MailboxSender<CompletionRequest>, permit: WorkPermit) {
        let mut delivery = self
            .delivery
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !delivery.retired {
            // This resource's consumed message freed one physical slot; every
            // other resource retains at most its own single slot.
            delivery.slot = sender.recycle(permit).ok();
            if delivery.slot.is_none() {
                delivery.retired = true;
            }
        }
        self.ready.notify_all();
        self.async_ready.notify_waiters();
    }

    pub fn retire(&self) {
        let mut delivery = self
            .delivery
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        delivery.retired = true;
        delivery.slot.take();
        self.ready.notify_all();
        self.async_ready.notify_waiters();
    }
}

pub(super) fn current_resources(controller: &Controller) -> Vec<CompletionKey> {
    let mut resources = controller
        .call_registry
        .pbx
        .keys()
        .copied()
        .map(CompletionKey::Call)
        .collect::<Vec<_>>();
    resources.extend(
        controller
            .call_runtime
            .keys()
            .copied()
            .map(CompletionKey::Call),
    );
    resources.extend(
        controller
            .devices
            .keys()
            .cloned()
            .map(CompletionKey::Device),
    );
    resources.push(CompletionKey::Configuration);
    resources
}

pub(super) fn reserve_lane(
    sender: &MailboxSender<CompletionRequest>,
) -> Result<Arc<CompletionLane>, AdmissionError> {
    sender.try_reserve().map(CompletionLane::new)
}
