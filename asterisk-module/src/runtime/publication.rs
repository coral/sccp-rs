//! Ordered AMI publication with one sequence owner and tracked native effects.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use tokio::task::{AbortHandle, JoinSet};

use super::backend::ManagementEvent;
use super::mailbox::{AdmissionError, QueueSnapshot, RUNTIME_MAILBOX_CAPACITY};
use super::owner::{
    EffectExecutor, OperationId, RequestError, RuntimeHandle, RuntimeOwner, RuntimeState, Step,
};
use crate::ami::events::{
    AmiEventError, EVENT_LIMITS, EventSequence, ManagementEventBackend, build_manager_event,
};
use crate::ami::manager::ManagerEvent;

pub(crate) struct PublicationHandle {
    runtime: RuntimeHandle<ManagementEvent, Result<(), AmiEventError>>,
}

impl PublicationHandle {
    /// Success means admission. Sequence allocation and completed native delivery
    /// happen on the publication owner; failures are reported there.
    pub fn enqueue(&self, event: &ManagementEvent) -> Result<(), AmiEventError> {
        // Preserve fail-closed validation at the existing synchronous boundary.
        build_manager_event(event, 1)?;
        self.runtime
            .try_request(event.clone(), None)
            .map(|_| ())
            .map_err(|error| match error {
                AdmissionError::Closed => AmiEventError::Closed,
                AdmissionError::Full | AdmissionError::Expired => AmiEventError::Unavailable,
            })
    }

    pub fn close(&self) {
        self.runtime.close();
    }

    pub async fn publish_confirmed(&self, event: ManagementEvent) -> Result<(), AmiEventError> {
        self.runtime
            .request(event, None)
            .await
            .map_err(|error| match error {
                RequestError::Admission(AdmissionError::Closed) => AmiEventError::Closed,
                _ => AmiEventError::Unavailable,
            })?
    }

    pub fn snapshot(&self) -> QueueSnapshot {
        self.runtime.snapshot()
    }
}

pub(crate) enum PublicationEffect {
    Publish(ManagerEvent),
    ReportFailure(AmiEventError),
}

#[derive(Default)]
pub(crate) struct PublicationState {
    sequence: EventSequence,
}

impl RuntimeState for PublicationState {
    type Command = ManagementEvent;
    type Reply = Result<(), AmiEventError>;
    type Resource = ();
    type Effect = PublicationEffect;
    type Completion = Result<(), AmiEventError>;

    fn resources(&self, _: &ManagementEvent) -> Vec<()> {
        vec![()]
    }

    fn prepare(
        &mut self,
        _: OperationId,
        event: ManagementEvent,
    ) -> Step<Self::Reply, Self::Effect> {
        match self.sequence.prepare(&event) {
            Ok((_, event)) => Step::Effect(PublicationEffect::Publish(event)),
            Err(error) => Step::Effect(PublicationEffect::ReportFailure(error)),
        }
    }

    fn complete(
        &mut self,
        _: OperationId,
        result: Self::Completion,
    ) -> Step<Self::Reply, Self::Effect> {
        Step::Finished(result)
    }

    fn failed(&mut self, _: OperationId) -> Step<Self::Reply, Self::Effect> {
        // Publication is not replayed after an uncertain native failure: replay
        // could deliver the same event twice. The sequence remains consumed.
        Step::Finished(Err(AmiEventError::Unavailable))
    }
}

pub(crate) struct PublicationExecutor<B> {
    backend: Arc<B>,
    report: fn(&AmiEventError),
}

impl<B: ManagementEventBackend> EffectExecutor<PublicationEffect, Result<(), AmiEventError>>
    for PublicationExecutor<B>
{
    fn spawn(
        &self,
        effect: PublicationEffect,
        workers: &mut JoinSet<Result<(), AmiEventError>>,
    ) -> AbortHandle {
        let backend = Arc::clone(&self.backend);
        let report = self.report;
        workers.spawn_blocking(move || {
            let result = match effect {
                PublicationEffect::Publish(event) => {
                    catch_unwind(AssertUnwindSafe(|| backend.publish(&event, EVENT_LIMITS)))
                        .map_err(|_| AmiEventError::Unavailable)
                        .and_then(|result| result.map_err(Into::into))
                }
                PublicationEffect::ReportFailure(error) => Err(error),
            };
            if let Err(error) = &result {
                report(error);
            }
            result
        })
    }
}

pub(crate) fn publication_runtime<B: ManagementEventBackend>(
    backend: B,
    report: fn(&AmiEventError),
) -> (
    PublicationHandle,
    RuntimeOwner<PublicationState, PublicationExecutor<B>>,
) {
    let (runtime, owner) = RuntimeOwner::new(
        PublicationState::default(),
        PublicationExecutor {
            backend: Arc::new(backend),
            report,
        },
        RUNTIME_MAILBOX_CAPACITY,
    );
    (PublicationHandle { runtime }, owner)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use sccp_protocol::{CallId, CallState, DeviceId};

    use super::*;
    use crate::ami::events::call_event;
    use crate::ami::manager::{ManagerError, ManagerLimits};

    struct Backend(Arc<Mutex<Vec<ManagerEvent>>>);
    impl ManagementEventBackend for Backend {
        fn publish(&self, event: &ManagerEvent, _: ManagerLimits) -> Result<(), ManagerError> {
            self.0.lock().unwrap().push(event.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn close_drains_publication_in_sequence_order() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let (handle, owner) = publication_runtime(Backend(Arc::clone(&events)), |_| {});
        let device = DeviceId::new("SEP000000000001").unwrap();
        for call in 1..=100 {
            handle
                .enqueue(&call_event(
                    &device,
                    CallId(call),
                    CallState::Connected,
                    false,
                ))
                .unwrap();
        }
        let task = tokio::spawn(owner.run());
        handle.close();
        task.await.unwrap();
        assert_eq!(handle.snapshot().outstanding, 0);
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 100);
        for (index, event) in events.iter().enumerate() {
            let field = event
                .fields()
                .iter()
                .find(|field| field.name() == "Sequence")
                .unwrap();
            assert_eq!(field.public_value(), Some((index + 1).to_string().as_str()));
        }
        assert!(matches!(
            handle.enqueue(&call_event(
                &device,
                CallId(101),
                CallState::Connected,
                false
            )),
            Err(AmiEventError::Closed)
        ));
    }

    #[tokio::test]
    async fn confirmed_publication_reports_native_failure_and_consumes_its_sequence() {
        struct FailingBackend(Arc<Mutex<Vec<ManagerEvent>>>);
        impl ManagementEventBackend for FailingBackend {
            fn publish(&self, event: &ManagerEvent, _: ManagerLimits) -> Result<(), ManagerError> {
                let mut events = self.0.lock().unwrap();
                events.push(event.clone());
                if events.len() == 1 {
                    Err(ManagerError::PublishFailed)
                } else {
                    Ok(())
                }
            }
        }
        let events = Arc::new(Mutex::new(Vec::new()));
        let (handle, owner) = publication_runtime(FailingBackend(Arc::clone(&events)), |_| {});
        let task = tokio::spawn(owner.run());
        let device = DeviceId::new("SEP000000000001").unwrap();
        let event = call_event(&device, CallId(1), CallState::Connected, false);
        assert!(matches!(
            handle.publish_confirmed(event.clone()).await,
            Err(AmiEventError::Manager(ManagerError::PublishFailed))
        ));
        handle.publish_confirmed(event).await.unwrap();
        handle.close();
        task.await.unwrap();
        let events = events.lock().unwrap();
        let sequences = events
            .iter()
            .map(|event| {
                event
                    .fields()
                    .iter()
                    .find(|field| field.name() == "Sequence")
                    .unwrap()
                    .public_value()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(sequences, ["1", "2"]);
    }
}
