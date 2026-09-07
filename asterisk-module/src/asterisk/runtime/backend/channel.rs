//! channel backend-effect translation.

use super::{
    AsteriskBackend, AsteriskBackendError, AsteriskChannel, AsteriskChannelMetadata, CString,
    CallId, ChannelAllocationOwner, ChannelAllocationRequest, ChannelBackend, Codec, LineBinding,
    NORMAL_CLEARING, NonNull, PbxCallId, PbxVideoFormat, allocate_channel, c_string,
    native_channel, prepare_channel_allocation_text, ptr, with_channel,
};

struct RoutingText {
    context: CString,
    destination: CString,
}

fn prepare_routing_text(
    call_id: PbxCallId,
    context: &str,
    destination: &str,
) -> Result<RoutingText, AsteriskBackendError> {
    let context = c_string(context).map_err(|source| AsteriskBackendError::NativeText {
        operation: "routing context",
        call_id,
        source,
    })?;
    let destination = c_string(destination).map_err(|source| AsteriskBackendError::NativeText {
        operation: "routing destination",
        call_id,
        source,
    })?;
    Ok(RoutingText {
        context,
        destination,
    })
}

impl ChannelBackend for AsteriskBackend<'_> {
    fn create_channel(
        &self,
        handset_call_id: CallId,
        call_id: PbxCallId,
        binding: &LineBinding,
        codec: Codec,
    ) -> Result<(), Self::Error> {
        let text = prepare_channel_allocation_text(self.access, binding, call_id)
            .map_err(|source| AsteriskBackendError::ChannelAllocation { call_id, source })?;
        allocate_channel(
            self.access,
            ChannelAllocationRequest {
                sccp_id: handset_call_id,
                pbx_id: call_id,
                binding,
                codec,
                pbx_video_formats: &PbxVideoFormat::ALL,
                assigned_ids: ptr::null(),
                requestor: ptr::null(),
                metadata: None,
                text,
                owner: ChannelAllocationOwner::Module,
            },
        )
        .map_err(|source| AsteriskBackendError::ChannelAllocation { call_id, source })
    }

    fn create_consultation_channel(
        &self,
        source_call_id: PbxCallId,
        handset_call_id: CallId,
        call_id: PbxCallId,
        binding: &LineBinding,
        codec: Codec,
    ) -> Result<(), Self::Error> {
        let text = prepare_channel_allocation_text(self.access, binding, call_id)
            .map_err(|source| AsteriskBackendError::ChannelAllocation { call_id, source })?;
        let created = with_channel(self.access, source_call_id, |source| {
            allocate_channel(
                self.access,
                ChannelAllocationRequest {
                    sccp_id: handset_call_id,
                    pbx_id: call_id,
                    binding,
                    codec,
                    pbx_video_formats: &PbxVideoFormat::ALL,
                    assigned_ids: ptr::null(),
                    requestor: source.cast_const(),
                    metadata: None,
                    text,
                    owner: ChannelAllocationOwner::Module,
                },
            )
        })
        .ok_or(AsteriskBackendError::CallUnavailable {
            operation: "create consultation channel",
            call_id: source_call_id,
        })?;
        created.map_err(|source| AsteriskBackendError::ChannelAllocation { call_id, source })
    }

    fn start_routing(
        &self,
        call_id: PbxCallId,
        context: &str,
        destination: &str,
    ) -> Result<(), Self::Error> {
        // Stage every fallible boundary conversion before reading or mutating
        // controller/native channel state. Invalid text must be a pure reject.
        let routing = prepare_routing_text(call_id, context, destination)?;
        let mut metadata = self
            .access
            .shared
            .controller
            .snapshot()
            .call_metadata(call_id)
            .cloned()
            .ok_or(AsteriskBackendError::CallUnavailable {
                operation: "set dialed number",
                call_id,
            })?;
        metadata.dnid = Some(destination.to_owned());
        metadata
            .validate()
            .map_err(|source| AsteriskBackendError::ChannelMetadata {
                call_id,
                source: source.into(),
            })?;
        with_channel(self.access, call_id, |channel| {
            let channel = unsafe { AsteriskChannel::from_raw(channel.cast()) }.map_err(|_| {
                AsteriskBackendError::CallUnavailable {
                    operation: "set dialed number",
                    call_id,
                }
            })?;
            AsteriskChannelMetadata::new()
                .apply(&channel, &metadata)
                .map_err(|source| AsteriskBackendError::ChannelMetadata { call_id, source })
        })
        .unwrap_or(Err(AsteriskBackendError::CallUnavailable {
            operation: "set dialed number",
            call_id,
        }))?;
        if !matches!(
            self.access
                .shared
                .controller
                .set_call_metadata(call_id, metadata)
                .unwrap_or_else(|_| Ok(false)),
            Ok(true)
        ) {
            return Err(AsteriskBackendError::CallUnavailable {
                operation: "commit dialed number",
                call_id,
            });
        }
        let result = with_channel(self.access, call_id, |channel| {
            NonNull::new(channel).ok_or(()).and_then(|channel| unsafe {
                native_channel::start_dialplan(channel, &routing.context, &routing.destination)
                    .map_err(|_| ())
            })
        });
        Self::typed_operation_result("start routing", call_id, result)
    }

    fn answer(&self, call_id: PbxCallId) -> Result<(), Self::Error> {
        let result = with_channel(self.access, call_id, |channel| {
            NonNull::new(channel).ok_or(()).and_then(|channel| unsafe {
                native_channel::queue_control(channel, native_channel::ChannelControl::Answer)
                    .map_err(|_| ())
            })
        });
        Self::typed_operation_result("answer", call_id, result)
    }

    fn hangup(&self, call_id: PbxCallId) -> Result<(), Self::Error> {
        let result = with_channel(self.access, call_id, |channel| {
            NonNull::new(channel).ok_or(()).and_then(|channel| unsafe {
                native_channel::hangup(channel, NORMAL_CLEARING).map_err(|_| ())
            })
        });
        Self::typed_operation_result("hangup", call_id, result)
    }

    fn send_digit(&self, call_id: PbxCallId, digit: char) -> Result<(), Self::Error> {
        let result = with_channel(self.access, call_id, |channel| {
            u8::try_from(digit).map_err(|_| ()).and_then(|digit| {
                NonNull::new(channel).ok_or(()).and_then(|channel| unsafe {
                    native_channel::queue_digit(channel, digit, 100).map_err(|_| ())
                })
            })
        });
        Self::typed_operation_result("send digit", call_id, result)
    }

    fn hold(&self, call_id: PbxCallId) -> Result<(), Self::Error> {
        let result = with_channel(self.access, call_id, |channel| {
            NonNull::new(channel).ok_or(()).and_then(|channel| unsafe {
                native_channel::queue_control(channel, native_channel::ChannelControl::Hold)
                    .map_err(|_| ())
            })
        });
        Self::typed_operation_result("hold", call_id, result)
    }

    fn resume(&self, call_id: PbxCallId) -> Result<(), Self::Error> {
        let result = with_channel(self.access, call_id, |channel| {
            NonNull::new(channel).ok_or(()).and_then(|channel| unsafe {
                native_channel::queue_control(channel, native_channel::ChannelControl::Unhold)
                    .map_err(|_| ())
            })
        });
        Self::typed_operation_result("resume", call_id, result)
    }
}

#[cfg(test)]
mod routing_text_tests {
    use super::*;

    #[test]
    fn rejects_context_before_routing_can_mutate_state() {
        let error = prepare_routing_text(PbxCallId(7), "from\0internal", "4000").unwrap_err();
        assert!(matches!(
            error,
            AsteriskBackendError::NativeText {
                operation: "routing context",
                call_id: PbxCallId(7),
                ..
            }
        ));
    }

    #[test]
    fn rejects_destination_before_routing_can_mutate_state() {
        let error = prepare_routing_text(PbxCallId(8), "from-internal", "40\0 00").unwrap_err();
        assert!(matches!(
            error,
            AsteriskBackendError::NativeText {
                operation: "routing destination",
                call_id: PbxCallId(8),
                ..
            }
        ));
    }
}
