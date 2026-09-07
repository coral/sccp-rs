use super::{
    Access, Arc, AsteriskCallCompletion, AsteriskCallFeatures, AsteriskChannel,
    AsteriskChannelMetadata, CString, CallFeatureError, CallId, CallMetadata,
    ChannelAllocationRequest, ChannelBinding, ChannelOperationPermit, ChannelState, Codec,
    ConferenceTaskCancellation, ConfiguredChannelMetadata, DeviceId, HandsetEffect, LineBinding,
    LogLevel, MediaEndpointAddress, ModuleConfig, MutexExt as _, NatMode, NonNull, PbxAudioFormat,
    PbxCallId, PbxVideoFormat, REQUESTED_CHANNEL_UNAVAILABLE, ReceiveTransmit,
    StationMediaCapabilities, StationTransport, VideoMode, ast_log, c_string,
    compose_channel_metadata, format_for, local_video_endpoint, native_audio_format,
    native_channel, negotiate_audio, pbx_audio_format, pbx_audio_format_from_native, raw,
    station_nat_active, sys,
};
use crate::asterisk::raw::handles::ChannelRef;
use crate::media::formats::{OwnedNegotiatedVideo, negotiate_video_owned};
use crate::pbx::operations::CallFeatureProvider as _;
use crate::runtime::controller::{VideoFallbackReason, VideoPlan, VideoPlanReadiness};
use sccp_protocol::message::MediaCapability;
use sccp_protocol::{
    DEFAULT_AUDIO_PACKET_MS, IpAddressType, MultimediaPayload, ProtocolVersion, RtpPayloadNumber,
    SessionGeneration,
};

pub fn handset_effect_call_id(effect: &HandsetEffect) -> Option<CallId> {
    Some(effect.subject_call_id())
}

pub fn preferred_codec(
    access: &Access,
    device: &DeviceId,
    line_instance: u32,
    pbx_formats: &[PbxAudioFormat],
) -> Option<Codec> {
    let (codecs, capabilities) = codec_policy(access, device, line_instance)?;
    preferred_codec_from_policy(
        &codecs,
        capabilities
            .as_ref()
            .map(StationMediaCapabilities::audio)
            .filter(|capabilities| !capabilities.is_empty()),
        pbx_formats,
    )
}

pub fn preferred_codec_upgrade(
    access: &Access,
    device: &DeviceId,
    line_instance: u32,
    current: Codec,
    pbx_formats: &[PbxAudioFormat],
) -> Option<Codec> {
    let (codecs, capabilities) = codec_policy(access, device, line_instance)?;
    let candidate = preferred_codec_from_policy(
        &codecs,
        capabilities
            .as_ref()
            .map(StationMediaCapabilities::audio)
            .filter(|capabilities| !capabilities.is_empty()),
        pbx_formats,
    )?;
    codec_upgrade(&codecs, current, candidate)
}

fn codec_upgrade(configured: &[Codec], current: Codec, candidate: Codec) -> Option<Codec> {
    let current = configured.iter().position(|codec| *codec == current)?;
    let candidate_rank = configured.iter().position(|codec| *codec == candidate)?;
    (candidate_rank < current).then_some(candidate)
}

fn codec_policy(
    access: &Access,
    device: &DeviceId,
    line_instance: u32,
) -> Option<(Vec<Codec>, Option<StationMediaCapabilities>)> {
    let config = access.config();
    let capabilities = access
        .shared
        .controller
        .snapshot()
        .registered_device(device)
        .map(|state| state.capabilities.clone())
        .flatten();
    let mut codecs = config
        .media_for_binding(&access.line_binding(device, line_instance)?)
        .map(|media| media.codecs)
        .or_else(|| {
            config
                .guest_hotline_binding(device, line_instance)
                .map(|_| config.general.codecs.clone())
        })?;
    constrain_unreported_audio_codecs(&mut codecs, capabilities.as_ref());
    Some((codecs, capabilities))
}

fn constrain_unreported_audio_codecs(
    codecs: &mut Vec<Codec>,
    capabilities: Option<&StationMediaCapabilities>,
) {
    if capabilities.is_none_or(|capabilities| capabilities.audio().is_empty()) {
        codecs.retain(|codec| matches!(codec, Codec::Pcma | Codec::Pcmu));
    }
}

struct SelectedVideo {
    session_generation: SessionGeneration,
    protocol: ProtocolVersion,
    mode: VideoMode,
    negotiated: OwnedNegotiatedVideo,
    payload_type: RtpPayloadNumber,
    payload: MultimediaPayload,
}

enum VideoSelection {
    Disabled,
    AudioOnly {
        session_generation: SessionGeneration,
        reason: VideoFallbackReason,
    },
    Ready(Box<SelectedVideo>),
}

impl VideoSelection {
    fn ready(&self) -> Option<&SelectedVideo> {
        match self {
            Self::Ready(selected) => Some(selected),
            Self::Disabled | Self::AudioOnly { .. } => None,
        }
    }
}

fn preferred_video(
    access: &Access,
    device: &DeviceId,
    line_instance: u32,
    pbx_formats: &[PbxVideoFormat],
) -> VideoSelection {
    let config = access.config();
    let Some(binding) = access.line_binding(device, line_instance) else {
        return VideoSelection::Disabled;
    };
    let Some(media) = config.media_for_binding(&binding) else {
        return VideoSelection::Disabled;
    };
    if media.video_mode == VideoMode::Off {
        return VideoSelection::Disabled;
    }
    let Some((session_generation, protocol, capabilities)) = access
        .shared
        .controller
        .snapshot()
        .registered_device(device)
        .map(|state| {
            (
                state.session_generation,
                state.registration.protocol,
                state.capabilities.clone(),
            )
        })
    else {
        return VideoSelection::Disabled;
    };
    let Some(capabilities) = capabilities else {
        return VideoSelection::AudioOnly {
            session_generation,
            reason: VideoFallbackReason::NotNegotiated,
        };
    };
    let representable = pbx_formats
        .iter()
        .copied()
        .filter(|format| format.payload_type().is_some())
        .collect::<Vec<_>>();
    let Ok(negotiated) = negotiate_video_owned(
        &media.codecs,
        capabilities,
        &representable,
        ReceiveTransmit::RECEIVE | ReceiveTransmit::TRANSMIT,
    ) else {
        return VideoSelection::AudioOnly {
            session_generation,
            reason: VideoFallbackReason::NotNegotiated,
        };
    };
    let Some(payload_type) = negotiated.pbx_format.payload_type() else {
        return VideoSelection::AudioOnly {
            session_generation,
            reason: VideoFallbackReason::DescriptorUnavailable,
        };
    };
    let Ok(payload) = negotiated.multimedia_payload(payload_type) else {
        return VideoSelection::AudioOnly {
            session_generation,
            reason: VideoFallbackReason::DescriptorUnavailable,
        };
    };
    VideoSelection::Ready(Box::new(SelectedVideo {
        session_generation,
        protocol,
        mode: media.video_mode,
        negotiated,
        payload_type,
        payload,
    }))
}

fn video_endpoint_is_supported(selected: &SelectedVideo, endpoint: MediaEndpointAddress) -> bool {
    if selected.protocol < ProtocolVersion::V17 && endpoint.address.is_ipv6() {
        return false;
    }
    match selected.negotiated.capability().address_type {
        None | Some(IpAddressType::Ipv4) => endpoint.address.is_ipv4(),
        Some(IpAddressType::Ipv6) => endpoint.address.is_ipv6(),
        Some(IpAddressType::Ipv4AndIpv6) => true,
        Some(IpAddressType::Invalid | IpAddressType::Unknown(_)) => false,
    }
}

fn preferred_codec_from_policy(
    configured: &[Codec],
    station: Option<&[MediaCapability]>,
    pbx_formats: &[PbxAudioFormat],
) -> Option<Codec> {
    negotiate_audio(configured, station, pbx_formats)
        .ok()
        .map(|negotiated| negotiated.codec)
}

/// Select the codec for an Asterisk-originated inbound channel request.
///
/// The original request capability set is compared with this exact station's
/// mapped capabilities by Asterisk's translator graph. This avoids proving a
/// path to the technology-wide union and then selecting an unreachable codec
/// for a particular phone.
pub unsafe fn preferred_inbound_codec(
    access: &Access,
    device: &DeviceId,
    line_instance: u32,
    requested: NonNull<sys::ast_format_cap>,
) -> Option<Codec> {
    let (codecs, capabilities) = codec_policy(access, device, line_instance)?;
    let station = capabilities
        .as_ref()
        .map(StationMediaCapabilities::audio)
        .filter(|capabilities| !capabilities.is_empty());
    let ordered = station
        .map(|capabilities| {
            capabilities
                .iter()
                .map(|capability| capability.codec)
                .filter(|codec| codecs.contains(codec))
                .fold(Vec::new(), |mut ordered, codec| {
                    if !ordered.contains(&codec) {
                        ordered.push(codec);
                    }
                    ordered
                })
        })
        .unwrap_or(codecs);
    let candidates = ordered
        .into_iter()
        .filter_map(|codec| {
            let format = pbx_audio_format(codec).ok()?;
            let station_supports = station.is_none_or(|capabilities| {
                capabilities.iter().any(|capability| {
                    capability.codec == codec && capability.max_packet_ms >= DEFAULT_AUDIO_PACKET_MS
                })
            });
            station_supports.then_some((codec, format))
        })
        .collect::<Vec<_>>();
    let destinations = candidates
        .iter()
        .map(|(_, format)| native_audio_format(*format))
        .fold(Vec::new(), |mut formats, format| {
            if !formats.contains(&format) {
                formats.push(format);
            }
            formats
        });
    let selected =
        unsafe { native_channel::best_translated_audio_format(requested, &destinations) }
            .map(pbx_audio_format_from_native)?;
    candidates
        .into_iter()
        .find_map(|(codec, format)| (format == selected).then_some(codec))
}

pub unsafe fn queue_unavailable(channel: *mut sys::ast_channel) {
    if let Some(channel) = NonNull::new(channel) {
        let _ = unsafe { native_channel::hangup(channel, REQUESTED_CHANNEL_UNAVAILABLE) };
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChannelAllocationOwner {
    Asterisk,
    Module,
}

pub struct ChannelAllocationText {
    line: CString,
    context: CString,
    caller_number: CString,
    caller_name: CString,
    media_bind_address: CString,
    assigned_uniqueid: Option<CString>,
}

#[derive(Debug, thiserror::Error)]
pub enum ChannelAllocationError {
    #[error("invalid native text in {field}: {source}")]
    NativeText {
        field: &'static str,
        #[source]
        source: crate::asterisk::boundary::NativeTextError,
    },
    #[error("native channel allocation failed")]
    Failed,
}

fn allocation_text(field: &'static str, value: &str) -> Result<CString, ChannelAllocationError> {
    c_string(value).map_err(|source| ChannelAllocationError::NativeText { field, source })
}

pub fn prepare_channel_allocation_text(
    access: &Access,
    binding: &LineBinding,
    pbx_id: PbxCallId,
) -> Result<ChannelAllocationText, ChannelAllocationError> {
    let config = access.config();
    let assigned_uniqueid = access
        .shared
        .controller
        .snapshot()
        .call_runtime_record(pbx_id)
        .and_then(|record| record.assigned_channel_id.as_ref())
        .map(|uniqueid| allocation_text("assigned unique ID", uniqueid))
        .transpose()?;
    Ok(ChannelAllocationText {
        line: allocation_text("line", &binding.line.number)?,
        context: allocation_text("context", &binding.line.context)?,
        caller_number: allocation_text("caller number", &binding.line.caller_number)?,
        caller_name: allocation_text("caller name", &binding.line.caller_name)?,
        media_bind_address: allocation_text(
            "media bind address",
            &config.general.bind.ip().to_string(),
        )?,
        assigned_uniqueid,
    })
}

pub fn allocate_channel(
    access: &Access,
    request: ChannelAllocationRequest<'_>,
) -> Result<(), ChannelAllocationError> {
    // Native allocation and publication in the binding map are one lifetime
    // operation. Holding this permit makes unload fail; after unload closes
    // admission, later queued effects fail without starting native I/O.
    let _allocation = access
        .shared
        .channel_allocations
        .try_enter()
        .ok_or(ChannelAllocationError::Failed)?;
    let ChannelAllocationRequest {
        sccp_id,
        pbx_id,
        binding,
        codec,
        pbx_video_formats,
        assigned_ids,
        requestor,
        metadata,
        text,
        owner,
    } = request;
    let signals = access
        .shared
        .call_signals
        .admit(
            pbx_id,
            super::RuntimeCallSignalKind::Hangup {
                handset_call_id: sccp_id,
            },
        )
        .map_err(|_| ChannelAllocationError::Failed)?;
    let Some(format) = format_for(codec) else {
        return Err(ChannelAllocationError::Failed);
    };
    let ChannelAllocationText {
        line,
        context,
        caller_number,
        caller_name,
        media_bind_address,
        assigned_uniqueid,
    } = text;
    let config = access.config();
    let metadata = match metadata {
        Some(metadata) => metadata,
        None => {
            let Some(metadata) = configured_channel_metadata(access, &config, binding, pbx_id)
            else {
                return Err(ChannelAllocationError::Failed);
            };
            metadata
        }
    };
    let network = config.network_for_device(&binding.device_id);
    let audio_qos = network
        .map(|network| network.qos.audio)
        .unwrap_or(config.general.qos.audio);
    let video_qos = network
        .map(|network| network.qos.video)
        .unwrap_or(config.general.qos.video);
    let selected_video = preferred_video(
        access,
        &binding.device_id,
        binding.appearance.instance,
        pbx_video_formats,
    );
    let symmetric_rtp =
        station_nat_active(access, &config, &binding.device_id).unwrap_or_else(|| {
            network.is_some_and(|network| matches!(network.nat, NatMode::On | NatMode::AutoOn))
        });
    let signaling_secure = access
        .shared
        .controller
        .snapshot()
        .registered_device(&binding.device_id)
        .is_some_and(|device| device.registration.transport == StationTransport::Secure);
    let allocation = unsafe {
        native_channel::allocate_channel(native_channel::ChannelAllocation {
            line: &line,
            context: &context,
            caller_number: &caller_number,
            caller_name: &caller_name,
            identity: ChannelState { pbx_id, sccp_id }.into(),
            format,
            media_bind_address: &media_bind_address,
            assigned_ids: assigned_ids.as_ref(),
            assigned_uniqueid: assigned_uniqueid.as_deref(),
            requestor,
            rtp_policy: native_channel::RtpPolicy {
                symmetric: symmetric_rtp,
                dscp: audio_qos.dscp.0,
                cos: audio_qos.cos.0,
            },
            video: selected_video.ready().map(|selected| {
                native_channel::video::VideoRtpConfiguration {
                    format: selected.negotiated.pbx_format,
                    payload_type: selected.payload_type,
                    media_bind_address: &media_bind_address,
                    policy: native_channel::RtpPolicy {
                        symmetric: symmetric_rtp,
                        dscp: video_qos.dscp.0,
                        cos: video_qos.cos.0,
                    },
                }
            }),
            security: native_channel::NativeChannelSecurity {
                signaling: signaling_secure,
                media: false,
            },
        })
    };
    let Ok(allocation) = allocation else {
        return Err(ChannelAllocationError::Failed);
    };
    for failure in allocation.qos.failures() {
        ast_log(
            LogLevel::Warning,
            &format!("unable to apply configured audio socket QoS: {failure}"),
        );
    }
    let (video_readiness, video_allocated) = match allocation.video {
        native_channel::VideoRtpAllocation::Active(report) => {
            for failure in report.failures() {
                ast_log(
                    LogLevel::Warning,
                    &format!("unable to apply configured video socket QoS: {failure}"),
                );
            }
            (Some(Ok(())), true)
        }
        native_channel::VideoRtpAllocation::Unavailable(error) => {
            ast_log(
                LogLevel::Warning,
                &format!("unable to allocate optional video RTP: {error:?}"),
            );
            (Some(Err(VideoFallbackReason::NativeRtpUnavailable)), false)
        }
        native_channel::VideoRtpAllocation::NotRequested => (None, false),
    };
    let channel = allocation.channel.as_ptr();
    let channel_metadata = AsteriskChannelMetadata::new();
    let Ok(channel_borrow) = (unsafe { AsteriskChannel::from_raw(channel.cast()) }) else {
        unsafe { queue_unavailable(channel) };
        return Err(ChannelAllocationError::Failed);
    };
    if let Err(error) = AsteriskCallCompletion::new().configure(&channel_borrow) {
        ast_log(
            LogLevel::Debug,
            &format!("SCCP call completion is not active for this channel: {error}"),
        );
    }
    if !requestor.is_null() {
        let Ok(requestor_borrow) =
            (unsafe { AsteriskChannel::from_raw((requestor as *mut sys::ast_channel).cast()) })
        else {
            unsafe { queue_unavailable(channel) };
            return Err(ChannelAllocationError::Failed);
        };
        if let Err(error) = channel_metadata.inherit_variables(&requestor_borrow, &channel_borrow) {
            ast_log(
                LogLevel::Warning,
                &format!("unable to inherit PBX channel variables: {error}"),
            );
            unsafe { queue_unavailable(channel) };
            return Err(ChannelAllocationError::Failed);
        }
    }
    if let Err(error) = channel_metadata.apply(&channel_borrow, &metadata) {
        ast_log(
            LogLevel::Warning,
            &format!("unable to apply PBX channel metadata: {error}"),
        );
        unsafe { queue_unavailable(channel) };
        return Err(ChannelAllocationError::Failed);
    }
    let media = config.media_for_binding(binding);
    let jitter = config.general.jitter_buffer;
    if jitter.should_configure_channel(media.is_some_and(|media| media.direct_media))
        && raw::bridge::configure_jitter_buffer(
            &channel_borrow,
            jitter.enabled,
            jitter.forced,
            jitter.log_frames,
            jitter.max_size_ms,
            jitter.resync_threshold_ms,
            jitter.implementation,
        )
        .is_err()
    {
        unsafe { queue_unavailable(channel) };
        return Err(ChannelAllocationError::Failed);
    }
    let private_call = access
        .shared
        .controller
        .snapshot()
        .call_privacy(sccp_id)
        .unwrap_or(false);
    if configure_pickup_policy(access, binding, channel, private_call).is_err() {
        unsafe { queue_unavailable(channel) };
        return Err(ChannelAllocationError::Failed);
    }
    let parking_lot = access
        .config()
        .parking_for_line(&binding.line.number)
        .and_then(|parking| parking.lot.clone());
    if let Some(parking_lot) = parking_lot
        && raw::bridge::set_channel_parking_lot(&channel_borrow, &parking_lot).is_err()
    {
        unsafe { queue_unavailable(channel) };
        return Err(ChannelAllocationError::Failed);
    }
    let Some(retained) = (unsafe { ChannelRef::acquire(channel) }) else {
        unsafe { queue_unavailable(channel) };
        return Err(ChannelAllocationError::Failed);
    };
    access
        .shared
        .channels
        .lock_unpoisoned()
        .insert(pbx_id, ChannelBinding::new(retained, signals));
    let (installed, keep_video) = match selected_video {
        VideoSelection::Disabled => (true, false),
        VideoSelection::AudioOnly {
            session_generation,
            reason,
        } => (
            access
                .shared
                .controller
                .set_video_audio_only_for_device(
                    &binding.device_id,
                    session_generation,
                    sccp_id,
                    reason,
                )
                .unwrap_or_else(|_| false),
            false,
        ),
        VideoSelection::Ready(selected) => {
            let state = video_readiness.unwrap_or(Err(VideoFallbackReason::NativeRtpUnavailable));
            match state {
                Ok(()) => {
                    let local_endpoint = local_video_endpoint(access, pbx_id, &binding.device_id)
                        .filter(|endpoint| video_endpoint_is_supported(&selected, *endpoint));
                    match local_endpoint {
                        Some(local_endpoint) => {
                            let SelectedVideo {
                                session_generation,
                                protocol,
                                mode,
                                negotiated,
                                payload,
                                ..
                            } = *selected;
                            let plan = VideoPlan {
                                session_generation,
                                protocol,
                                mode,
                                negotiated,
                                payload,
                                local_endpoint,
                            };
                            let installed = access
                                .shared
                                .controller
                                .install_video_plan_for_device(
                                    &binding.device_id,
                                    sccp_id,
                                    plan,
                                    VideoPlanReadiness::Ready,
                                )
                                .unwrap_or_else(|_| false);
                            (installed, installed)
                        }
                        None => (
                            access
                                .shared
                                .controller
                                .set_video_audio_only_for_device(
                                    &binding.device_id,
                                    selected.session_generation,
                                    sccp_id,
                                    VideoFallbackReason::LocalEndpointUnavailable,
                                )
                                .unwrap_or_else(|_| false),
                            false,
                        ),
                    }
                }
                Err(reason) => (
                    access
                        .shared
                        .controller
                        .set_video_audio_only_for_device(
                            &binding.device_id,
                            selected.session_generation,
                            sccp_id,
                            reason,
                        )
                        .unwrap_or_else(|_| false),
                    false,
                ),
            }
        }
    };
    if !installed {
        ast_log(
            LogLevel::Warning,
            &format!("unable to install video media state for call {pbx_id:?}"),
        );
    }
    if video_allocated
        && !keep_video
        && (unsafe { native_channel::video::disable_video(NonNull::new_unchecked(channel)) })
            .is_err()
    {
        let binding = access.shared.channels.lock_unpoisoned().remove(&pbx_id);
        if let Some(binding) = binding {
            drop(binding.close());
        }
        unsafe { queue_unavailable(channel) };
        return Err(ChannelAllocationError::Failed);
    }
    if owner == ChannelAllocationOwner::Asterisk
        && (unsafe { native_channel::handoff_channel_to_asterisk(NonNull::new_unchecked(channel)) })
            .is_err()
    {
        let binding = access.shared.channels.lock_unpoisoned().remove(&pbx_id);
        if let Some(binding) = binding {
            drop(binding.close());
        }
        unsafe { queue_unavailable(channel) };
        return Err(ChannelAllocationError::Failed);
    }
    Ok(())
}

pub fn configured_channel_metadata(
    access: &Access,
    config: &ModuleConfig,
    binding: &LineBinding,
    pbx_id: PbxCallId,
) -> Option<CallMetadata> {
    let (direction, digits, metadata) =
        (|controller: &crate::runtime::controller::ControllerSnapshot| {
            let call = controller.pbx_call(pbx_id)?;
            Some((call.direction, call.digits.clone(), call.metadata.clone()))
        })(access.shared.controller.snapshot().as_ref())?;
    let device_variables = config
        .devices
        .get(&binding.device_id)
        .map(|device| device.channel_variables.as_slice())
        .unwrap_or_default();
    let metadata = compose_channel_metadata(
        metadata,
        ConfiguredChannelMetadata {
            direction,
            caller_number: &binding.line.caller_number,
            dialed_number: (!digits.is_empty()).then_some(digits.as_str()),
            account_code: binding.line.account_code.as_deref(),
            language: &binding.line.language,
            device_variables,
            line_variables: &binding.line.channel_variables,
        },
    )
    .ok()?;
    if !matches!(
        access
            .shared
            .controller
            .set_call_metadata(pbx_id, metadata.clone())
            .unwrap_or_else(|_| Ok(false)),
        Ok(true)
    ) {
        return None;
    }
    Some(metadata)
}

pub fn configure_pickup_policy(
    access: &Access,
    binding: &LineBinding,
    channel: *mut sys::ast_channel,
    private_call: bool,
) -> Result<(), CallFeatureError> {
    let config = access.config();
    let pickup = config
        .features_for_line(&binding.line.number)
        .map(|features| &features.pickup);
    let group_mask = |groups: Option<&std::collections::BTreeSet<u8>>| {
        groups.into_iter().flatten().fold(0_u64, |mask, group| {
            mask | 1_u64.checked_shl(u32::from(*group)).unwrap_or(0)
        })
    };
    let call_groups = group_mask(pickup.map(|pickup| &pickup.call_groups));
    let pickup_groups = group_mask(pickup.map(|pickup| &pickup.pickup_groups));
    let named_call_groups = pickup
        .map(|pickup| {
            pickup
                .named_call_groups
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default();
    let named_pickup_groups = pickup
        .map(|pickup| {
            pickup
                .named_pickup_groups
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default();
    let channel = unsafe { AsteriskChannel::from_raw(channel.cast()) }.map_err(|_| {
        CallFeatureError::InvalidInput {
            operation: "configure pickup policy",
        }
    })?;
    AsteriskCallFeatures::new().configure_pickup(
        &channel,
        call_groups,
        pickup_groups,
        &named_call_groups,
        &named_pickup_groups,
        private_call,
    )
}

#[cfg(test)]
mod allocation_text_tests {
    use super::*;

    #[test]
    fn every_native_channel_text_family_rejects_interior_nul() {
        for field in [
            "line",
            "context",
            "caller number",
            "caller name",
            "media bind address",
            "assigned unique ID",
        ] {
            let error = allocation_text(field, "safe\0suffix").unwrap_err();
            assert!(matches!(
                error,
                ChannelAllocationError::NativeText {
                    field: actual,
                    ..
                } if actual == field
            ));
        }
    }

    #[test]
    fn codec_upgrade_only_moves_toward_the_front_of_the_policy() {
        let configured = [Codec::Wideband256k, Codec::Pcma, Codec::Pcmu];
        assert_eq!(
            codec_upgrade(&configured, Codec::Pcma, Codec::Wideband256k),
            Some(Codec::Wideband256k)
        );
        assert_eq!(
            codec_upgrade(&configured, Codec::Wideband256k, Codec::Pcma),
            None
        );
        assert_eq!(codec_upgrade(&configured, Codec::Pcma, Codec::Pcma), None);
    }

    #[test]
    fn pending_and_empty_capabilities_retain_only_configured_g711_codecs() {
        let configured = vec![Codec::G729, Codec::Pcma, Codec::Wideband256k, Codec::Pcmu];
        let empty = StationMediaCapabilities::default();
        for capabilities in [None, Some(&empty)] {
            let mut codecs = configured.clone();
            constrain_unreported_audio_codecs(&mut codecs, capabilities);
            assert_eq!(codecs, [Codec::Pcma, Codec::Pcmu]);
        }
    }

    #[test]
    fn reported_capabilities_preserve_configured_order_for_negotiation() {
        let capabilities = StationMediaCapabilities::from(vec![MediaCapability {
            codec: Codec::G729,
            max_packet_ms: 80,
            codec_parameters: [0; 8],
        }]);
        let mut codecs = vec![Codec::Pcma, Codec::G729, Codec::Pcmu];
        constrain_unreported_audio_codecs(&mut codecs, Some(&capabilities));
        assert_eq!(codecs, [Codec::Pcma, Codec::G729, Codec::Pcmu]);
    }
}

pub fn remove_channel(access: &Access, pbx_id: PbxCallId) {
    super::backend::reap_conference_tasks(&access.shared);
    let destination = access
        .shared
        .conference_destination_tasks
        .lock_unpoisoned()
        .cancel(pbx_id);
    if let Some(destination) = destination {
        ConferenceTaskCancellation::cancel(destination);
    }
    access.shared.media_runtime.remove_call(pbx_id);
    let _ = access.shared.controller.retire_call_runtime(pbx_id);
    let binding = access.shared.channels.lock_unpoisoned().remove(&pbx_id);
    if let Some(binding) = binding {
        drop(binding.close());
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ChannelAvailability {
    Live,
    Retiring,
    Missing,
}

pub(super) fn channel_availability(access: &Access, pbx_id: PbxCallId) -> ChannelAvailability {
    match channel_binding(access, pbx_id) {
        Some(binding) if binding.is_closed() => ChannelAvailability::Retiring,
        Some(_) => ChannelAvailability::Live,
        None => ChannelAvailability::Missing,
    }
}

pub fn with_channel<T>(
    access: &Access,
    pbx_id: PbxCallId,
    operation: impl FnOnce(*mut sys::ast_channel) -> T,
) -> Option<T> {
    let binding = channel_binding(access, pbx_id)?;
    let permit = binding.enter()?;
    Some(operation(permit.resource().as_ptr()))
}

pub fn channel_binding(access: &Access, pbx_id: PbxCallId) -> Option<Arc<ChannelBinding>> {
    access
        .shared
        .channels
        .lock_unpoisoned()
        .get(&pbx_id)
        .cloned()
}

pub fn with_two_channels<T>(
    access: &Access,
    first: PbxCallId,
    second: PbxCallId,
    operation: impl FnOnce(*mut sys::ast_channel, *mut sys::ast_channel) -> T,
) -> Option<T> {
    let (first_channel, second_channel) = retain_two_channels(access, first, second)?;
    Some(operation(
        first_channel.resource().as_ptr(),
        second_channel.resource().as_ptr(),
    ))
}

pub fn retain_two_channels(
    access: &Access,
    first: PbxCallId,
    second: PbxCallId,
) -> Option<(ChannelOperationPermit, ChannelOperationPermit)> {
    let first = channel_binding(access, first)?;
    let second = channel_binding(access, second)?;
    Some((first.try_enter()?, second.try_enter()?))
}

pub fn with_channels<T>(
    access: &Access,
    call_ids: &[PbxCallId],
    operation: impl FnOnce(&[*mut sys::ast_channel]) -> T,
) -> Option<T> {
    let bindings = call_ids
        .iter()
        .map(|call_id| channel_binding(access, *call_id))
        .collect::<Option<Vec<_>>>()?;
    let permits = bindings
        .into_iter()
        .map(|binding| binding.try_enter())
        .collect::<Option<Vec<_>>>()?;
    let pointers = permits
        .iter()
        .map(|permit| permit.resource().as_ptr())
        .collect::<Vec<_>>();
    Some(operation(&pointers))
}
