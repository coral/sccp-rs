use super::{
    Access, AddressSelectionPolicy, AsteriskBackend, AudioFraming, AudioFramingError,
    AudioProcessingPolicy, CallId, CallState, Codec, DEFAULT_AUDIO_MAX_FRAMES_PER_PACKET,
    DEFAULT_AUDIO_PACKET_MS, DeviceId, DirectMediaCall, DirectMediaPolicy, DtmfMode, IpAddr,
    MediaEndpoint, MediaEndpointAddress, MediaStreamState, MediaTrafficClass, ModuleConfig,
    NonNull, OutboundMediaMode, PbxCallId, PhoneCommand, PhoneCommandAction,
    ResolvedExternalAddresses, canonical_ip_address, native_channel, state_from_channel, sys,
    with_channel,
};
use crate::media::direct::direct_failure_anchor;
use crate::media::encryption::{AudioEncryptionAdmission, MediaEncryptionDecision};
use crate::runtime::backend::MediaBackend as _;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StationPacketLimit {
    Unavailable,
    Pending,
    Maximum(u32),
    Unsupported,
}

fn audio_encryption_admission(
    access: &Access,
    device_id: &DeviceId,
    call_id: CallId,
) -> Result<AudioEncryptionAdmission, String> {
    let snapshot = access.shared.controller.snapshot();
    let call = snapshot
        .call(call_id)
        .filter(|call| &call.device_id == device_id)
        .ok_or_else(|| format!("call {call_id:?} is not owned by device {device_id}"))?;
    if let Some(retained) = snapshot
        .call_runtime_record(call.pbx_id)
        .and_then(|record| record.audio_encryption.clone())
    {
        return Ok(retained);
    }
    let registration = snapshot
        .registered_device(device_id)
        .ok_or_else(|| format!("call {call_id:?} has no registered station"))?;
    let generation = registration.session_generation;
    let station = registration.audio_encryption.clone();
    let config = access.config();
    let binding = access
        .line_binding(device_id, call.line_instance)
        .ok_or_else(|| format!("call {call_id:?} has no configured media binding"))?;
    let policy = config
        .media_for_binding(&binding)
        .map(|media| media.audio_encryption)
        .or_else(|| {
            config
                .guest_hotline_binding(device_id, call.line_instance)
                .map(|_| config.general.audio_encryption.clone())
        })
        .ok_or_else(|| format!("call {call_id:?} has no audio-encryption policy"))?;
    let local = AsteriskBackend::new(access).audio_encryption_capabilities();
    access
        .shared
        .controller
        .retain_audio_encryption(
            call_id,
            device_id.clone(),
            generation,
            AudioEncryptionAdmission::new(policy, station, local),
        )
        .map_err(|error| format!("audio-encryption owner unavailable: {error}"))?
        .ok_or_else(|| format!("call {call_id:?} changed during audio-encryption admission"))
}

pub(super) fn admit_clear_audio_media(
    access: &Access,
    device_id: &DeviceId,
    call_id: CallId,
) -> Result<(), String> {
    match audio_encryption_admission(access, device_id, call_id)?.decide() {
        Ok(MediaEncryptionDecision::Clear) => Ok(()),
        Ok(MediaEncryptionDecision::Encrypted(profile)) => Err(format!(
            "audio media profile {profile} requires a protected stream adapter"
        )),
        Err(error) => Err(format!("audio media admission rejected: {error}")),
    }
}

pub fn station_address_context(
    access: &Access,
    device_id: &DeviceId,
) -> Option<(IpAddr, Option<IpAddr>)> {
    (|controller: &crate::runtime::controller::ControllerSnapshot| {
        let registration = &controller.registered_device(device_id)?.registration;
        Some((
            canonical_ip_address(registration.peer.ip()),
            registration.reported_address_for_peer(),
        ))
    })(access.shared.controller.snapshot().as_ref())
}

pub fn address_selection_policy<'a>(
    config: &'a ModuleConfig,
    device_id: &DeviceId,
    external: ResolvedExternalAddresses,
) -> Option<AddressSelectionPolicy<'a>> {
    let nat = config
        .network_for_device(device_id)
        .map(|device| device.nat)
        .or_else(|| {
            config
                .guest_hotline_binding(device_id, 1)
                .map(|_| config.general.network.nat)
        })?;
    Some(AddressSelectionPolicy {
        nat,
        local_networks: &config.general.network.local_networks,
        advertised: &config.general.network.advertised,
        external,
    })
}

pub fn normalize_phone_media_endpoint(
    access: &Access,
    device_id: &DeviceId,
    endpoint: &mut MediaEndpoint,
) -> Result<(), String> {
    endpoint.address = normalized_phone_address(access, device_id, endpoint.address)?;
    Ok(())
}

fn normalized_phone_address(
    access: &Access,
    device_id: &DeviceId,
    address: IpAddr,
) -> Result<IpAddr, String> {
    let config = access.config();
    let external = access.shared.external_addresses.current();
    let policy = address_selection_policy(&config, device_id, external)
        .ok_or_else(|| format!("device {device_id} has no address-selection policy"))?;
    let (signaling_peer, registration_reported) = station_address_context(access, device_id)
        .ok_or_else(|| format!("device {device_id} is not registered"))?;
    let (selected, _) = policy
        .phone_peer(signaling_peer, address, registration_reported)
        .map_err(|error| error.to_string())?;
    Ok(selected)
}

pub fn normalize_phone_video_endpoint(
    access: &Access,
    device_id: &DeviceId,
    endpoint: &mut MediaEndpointAddress,
) -> Result<(), String> {
    endpoint.address = normalized_phone_address(access, device_id, endpoint.address)?;
    Ok(())
}

pub fn set_remote_video_endpoint(
    access: &Access,
    pbx_id: PbxCallId,
    endpoint: MediaEndpointAddress,
) -> Result<(), String> {
    with_channel(access, pbx_id, |channel| {
        NonNull::new(channel).ok_or(()).and_then(|channel| unsafe {
            native_channel::video::set_remote_video(
                channel,
                std::net::SocketAddr::new(endpoint.address, endpoint.port),
            )
            .map_err(|_| ())
        })
    })
    .ok_or_else(|| format!("call {pbx_id:?} has no native channel"))?
    .map_err(|()| format!("call {pbx_id:?} has no active video RTP instance"))
}

#[allow(
    dead_code,
    reason = "used when a video command descriptor is available"
)]
pub fn local_video_endpoint(
    access: &Access,
    pbx_id: PbxCallId,
    device_id: &DeviceId,
) -> Option<MediaEndpointAddress> {
    let endpoint = with_channel(access, pbx_id, |channel| {
        NonNull::new(channel).and_then(|channel| unsafe {
            native_channel::video::local_video_endpoint(channel).ok()
        })
    })
    .flatten()?;
    let config = access.config();
    let external = access.shared.external_addresses.current();
    let policy = address_selection_policy(&config, device_id, external)?;
    let (signaling_peer, registration_reported) = station_address_context(access, device_id)?;
    let (address, _) = policy
        .advertised_media(endpoint.address, signaling_peer, registration_reported)
        .ok()?;
    Some(MediaEndpointAddress {
        address,
        port: endpoint.port,
    })
}

pub fn local_media_endpoint(
    access: &Access,
    pbx_id: PbxCallId,
    device_id: &DeviceId,
    codec: Codec,
) -> Option<MediaEndpoint> {
    let endpoint = with_channel(access, pbx_id, |channel| {
        NonNull::new(channel)
            .and_then(|channel| unsafe { native_channel::local_media_endpoint(channel).ok() })
    });
    let endpoint = endpoint.flatten()?;
    let config = access.config();
    let external = access.shared.external_addresses.current();
    let policy = address_selection_policy(&config, device_id, external)?;
    let (signaling_peer, registration_reported) = station_address_context(access, device_id)?;
    let (address, _) = policy
        .advertised_media(endpoint.address, signaling_peer, registration_reported)
        .ok()?;
    Some(MediaEndpoint {
        address,
        rtp_port: endpoint.port,
        rtcp_port: endpoint.port.saturating_add(1),
        codec,
        packet_ms: DEFAULT_AUDIO_PACKET_MS,
        max_frames_per_packet: DEFAULT_AUDIO_MAX_FRAMES_PER_PACKET,
        telephone_event_payload: 101,
    })
}

pub(super) fn retarget_to_anchor(access: &Access, call: &DirectMediaCall) -> bool {
    let Some(retarget) = prepare_anchor_retarget(access, call) else {
        return false;
    };
    if access.phone.try_send(retarget.command()).is_ok() {
        retarget.confirm();
        true
    } else {
        retarget.rollback(access)
    }
}

pub(super) struct PendingMediaRetarget {
    command: PhoneCommand,
    call_id: CallId,
    rollback: MediaRetargetRollback,
}

enum MediaRetargetRollback {
    Anchor(MediaEndpoint),
    Direct(MediaStreamState),
}

impl PendingMediaRetarget {
    pub(super) fn command(&self) -> PhoneCommand {
        self.command.clone()
    }

    pub(super) fn confirm(self) {}

    pub(super) fn rollback(self, access: &Access) -> bool {
        {
            match self.rollback {
                MediaRetargetRollback::Anchor(previous) => access
                    .shared
                    .controller
                    .media_retarget_enqueue_failed(self.call_id, previous)
                    .unwrap_or_else(|_| false),
                MediaRetargetRollback::Direct(previous) => access
                    .shared
                    .controller
                    .media_retarget_compensation_enqueue_failed(self.call_id, previous)
                    .unwrap_or_else(|_| false),
            }
        }
    }
}

pub(super) fn prepare_anchor_retarget(
    access: &Access,
    call: &DirectMediaCall,
) -> Option<PendingMediaRetarget> {
    let mut endpoint = local_media_endpoint(access, call.pbx_id, &call.device_id, call.codec)?;
    let framing = audio_framing(access, &call.device_id, call.call_id, call.codec).ok()?;
    endpoint.packet_ms = framing.packet_ms;
    endpoint.max_frames_per_packet = framing.max_frames_per_packet;
    let dtmf_mode = configured_dtmf_mode(access, &call.device_id, call.call_id);
    let audio_processing = configured_audio_processing(access, &call.device_id, call.call_id);
    let traffic_class = configured_audio_traffic_class(access, &call.device_id)?;
    let previous = access
        .shared
        .controller
        .media_retarget_started(call.call_id)
        .unwrap_or_else(|_| None)?;
    Some(PendingMediaRetarget {
        command: PhoneCommand::new(
            call.device_id.clone(),
            PhoneCommandAction::StartMedia {
                call_id: call.call_id,
                endpoint,
                dtmf_mode,
                audio_processing,
                traffic_class,
            },
        ),
        call_id: call.call_id,
        rollback: MediaRetargetRollback::Anchor(previous),
    })
}

pub(super) fn retarget_to_direct(access: &Access, call: &DirectMediaCall) -> bool {
    let Some(retarget) = prepare_direct_retarget(access, call) else {
        return false;
    };
    if access.phone.try_send(retarget.command()).is_ok() {
        retarget.confirm();
        true
    } else {
        retarget.rollback(access)
    }
}

pub(super) fn prepare_direct_retarget(
    access: &Access,
    call: &DirectMediaCall,
) -> Option<PendingMediaRetarget> {
    let previous = access
        .shared
        .controller
        .media_retarget_compensation_started(call.call_id)
        .unwrap_or_else(|_| None)?;
    let dtmf_mode = configured_dtmf_mode(access, &call.device_id, call.call_id);
    let audio_processing = configured_audio_processing(access, &call.device_id, call.call_id);
    let traffic_class = configured_audio_traffic_class(access, &call.device_id)?;
    let framing = audio_framing(access, &call.device_id, call.call_id, call.codec).ok()?;
    let mut endpoint = call.transmit_endpoint;
    endpoint.packet_ms = framing.packet_ms;
    endpoint.max_frames_per_packet = framing.max_frames_per_packet;
    Some(PendingMediaRetarget {
        command: PhoneCommand::new(
            call.device_id.clone(),
            PhoneCommandAction::StartMedia {
                call_id: call.call_id,
                endpoint,
                dtmf_mode,
                audio_processing,
                traffic_class,
            },
        ),
        call_id: call.call_id,
        rollback: MediaRetargetRollback::Direct(previous),
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaFailureDisposition {
    Ignored,
    Retrying,
    Hangup,
}

pub fn recover_failed_media_transmission(
    access: &Access,
    device_id: &DeviceId,
    call_id: CallId,
    failed_endpoint: MediaEndpoint,
) -> MediaFailureDisposition {
    let call = (|controller: &crate::runtime::controller::ControllerSnapshot| {
        let call = controller.call(call_id)?;
        if &call.device_id != device_id
            || call.state != CallState::Connected
            || call.audio_transmit != MediaStreamState::Open(failed_endpoint)
        {
            return None;
        }
        let MediaStreamState::Open(phone_endpoint) = call.audio else {
            return None;
        };
        Some(DirectMediaCall {
            pbx_id: call.pbx_id,
            device_id: call.device_id.clone(),
            call_id,
            line_instance: call.line_instance,
            codec: call.codec,
            phone_endpoint,
            transmit_endpoint: failed_endpoint,
        })
    })(access.shared.controller.snapshot().as_ref());
    let Some(call) = call else {
        return MediaFailureDisposition::Ignored;
    };
    let anchoring_required = access.shared.media_runtime.is_anchored(call.pbx_id);
    let anchor = local_media_endpoint(access, call.pbx_id, &call.device_id, call.codec).and_then(
        |mut endpoint| {
            let framing = audio_framing(access, &call.device_id, call.call_id, call.codec).ok()?;
            endpoint.packet_ms = framing.packet_ms;
            endpoint.max_frames_per_packet = framing.max_frames_per_packet;
            Some(endpoint)
        },
    );
    let Some(anchor) = direct_failure_anchor(failed_endpoint, anchor, anchoring_required) else {
        return MediaFailureDisposition::Hangup;
    };
    let dtmf_mode = configured_dtmf_mode(access, &call.device_id, call.call_id);
    let audio_processing = configured_audio_processing(access, &call.device_id, call.call_id);
    if enqueue_media_retarget(access, &call, anchor, dtmf_mode, audio_processing) {
        MediaFailureDisposition::Retrying
    } else {
        MediaFailureDisposition::Hangup
    }
}

pub fn enqueue_media_retarget(
    access: &Access,
    call: &DirectMediaCall,
    endpoint: MediaEndpoint,
    dtmf_mode: DtmfMode,
    audio_processing: AudioProcessingPolicy,
) -> bool {
    let Some(traffic_class) = configured_audio_traffic_class(access, &call.device_id) else {
        return false;
    };
    let previous = access
        .shared
        .controller
        .media_retarget_started(call.call_id)
        .unwrap_or_else(|_| None);
    let Some(previous) = previous else {
        return false;
    };
    if access
        .phone
        .try_send(PhoneCommand::new(
            call.device_id.clone(),
            PhoneCommandAction::StartMedia {
                call_id: call.call_id,
                endpoint,
                dtmf_mode,
                audio_processing,
                traffic_class,
            },
        ))
        .is_ok()
    {
        return true;
    }
    access
        .shared
        .controller
        .media_retarget_enqueue_failed(call.call_id, previous)
        .unwrap_or_else(|_| false)
}

pub(super) fn audio_framing(
    access: &Access,
    device: &DeviceId,
    call_id: CallId,
    codec: Codec,
) -> Result<AudioFraming, AudioFramingError> {
    let packet_limit = (|controller: &crate::runtime::controller::ControllerSnapshot| {
        let Some(state) = controller.registered_device(device) else {
            return StationPacketLimit::Unavailable;
        };
        let Some(capabilities) = state.capabilities.as_ref() else {
            return StationPacketLimit::Pending;
        };
        capabilities
            .audio()
            .iter()
            .find(|capability| capability.codec == codec)
            .map_or(StationPacketLimit::Unsupported, |capability| {
                StationPacketLimit::Maximum(capability.max_packet_ms)
            })
    })(access.shared.controller.snapshot().as_ref());
    let pbx_id = access.shared.controller.snapshot().call_pbx_id(call_id);
    let requested_packet_ms = pbx_id
        .and_then(|pbx_id| {
            access
                .shared
                .controller
                .snapshot()
                .call_runtime_record(pbx_id)
                .and_then(|record| record.audio_packet_ms)
        })
        .unwrap_or(DEFAULT_AUDIO_PACKET_MS);
    if requested_packet_ms == 0 {
        return Err(AudioFramingError::InvalidRequestedPacketDuration);
    }
    match packet_limit {
        StationPacketLimit::Unavailable => {
            return Err(AudioFramingError::StationUnavailable);
        }
        StationPacketLimit::Pending => {}
        StationPacketLimit::Maximum(maximum) if requested_packet_ms <= maximum => {}
        StationPacketLimit::Maximum(maximum) => {
            return Err(AudioFramingError::PacketDurationExceedsStationMaximum {
                requested_packet_ms,
                maximum_packet_ms: maximum,
            });
        }
        StationPacketLimit::Unsupported => {
            return Err(AudioFramingError::UnsupportedCodec { codec });
        }
    }
    Ok(AudioFraming {
        packet_ms: requested_packet_ms,
        max_frames_per_packet: DEFAULT_AUDIO_MAX_FRAMES_PER_PACKET,
    })
}

pub fn configured_dtmf_mode(access: &Access, device: &DeviceId, call_id: CallId) -> DtmfMode {
    let line_instance = access
        .shared
        .controller
        .snapshot()
        .call_line_instance(call_id);
    line_instance
        .and_then(|line_instance| {
            let config = access.config();
            access
                .line_binding(device, line_instance)
                .as_ref()
                .and_then(|binding| config.media_for_binding(binding))
                .map(|media| media.dtmf_mode)
        })
        .unwrap_or(DtmfMode::Auto)
}

pub fn configured_audio_processing(
    access: &Access,
    device: &DeviceId,
    call_id: CallId,
) -> AudioProcessingPolicy {
    let line_instance = access
        .shared
        .controller
        .snapshot()
        .call_line_instance(call_id);
    line_instance
        .and_then(|line_instance| {
            let config = access.config();
            access
                .line_binding(device, line_instance)
                .as_ref()
                .and_then(|binding| config.media_for_binding(binding))
                .map(|media| media.audio_processing)
        })
        .unwrap_or_default()
}

fn configured_traffic_class(
    access: &Access,
    device: &DeviceId,
    select: impl FnOnce(crate::config::QosPolicy) -> crate::config::Dscp,
) -> Option<MediaTrafficClass> {
    let config = access.config();
    let qos = config
        .network_for_device(device)
        .map(|network| network.qos)
        .unwrap_or(config.general.qos);
    let dscp = select(qos);
    MediaTrafficClass::from_dscp(dscp.0)
}

pub fn configured_audio_traffic_class(
    access: &Access,
    device: &DeviceId,
) -> Option<MediaTrafficClass> {
    configured_traffic_class(access, device, |qos| qos.audio.dscp)
}

pub fn configured_video_traffic_class(
    access: &Access,
    device: &DeviceId,
) -> Option<MediaTrafficClass> {
    configured_traffic_class(access, device, |qos| qos.video.dscp)
}

pub fn configured_early_media(access: &Access, device: &DeviceId, call_id: CallId) -> bool {
    let line_instance = access
        .shared
        .controller
        .snapshot()
        .call_line_instance(call_id);
    line_instance
        .and_then(|line_instance| {
            let config = access.config();
            access
                .line_binding(device, line_instance)
                .as_ref()
                .and_then(|binding| config.media_for_binding(binding))
                .map(|media| media.early_media)
        })
        .unwrap_or(true)
}

pub fn direct_media_call(
    access: &Access,
    channel: *mut sys::ast_channel,
) -> Option<DirectMediaCall> {
    let state = unsafe { state_from_channel(channel) }?;
    if access.shared.media_runtime.is_anchored(state.pbx_id) {
        return None;
    }
    (|controller: &crate::runtime::controller::ControllerSnapshot| {
        if controller.conference_session_by_pbx(state.pbx_id).is_some()
            || controller.barge_session_by_pbx(state.pbx_id).is_some()
        {
            return None;
        }
        let call_id = controller.active_call_id(state.pbx_id)?;
        let call = controller.call(call_id)?;
        if call.pbx_id != state.pbx_id || call.state != CallState::Connected {
            return None;
        }
        let MediaStreamState::Open(phone_endpoint) = call.audio else {
            return None;
        };
        let MediaStreamState::Open(transmit_endpoint) = call.audio_transmit else {
            return None;
        };
        Some(DirectMediaCall {
            pbx_id: call.pbx_id,
            device_id: call.device_id.clone(),
            call_id,
            line_instance: call.line_instance,
            codec: call.codec,
            phone_endpoint,
            transmit_endpoint,
        })
    })(access.shared.controller.snapshot().as_ref())
}

pub fn direct_media_policy<'a>(
    access: &Access,
    config: &'a ModuleConfig,
    call: &DirectMediaCall,
) -> Option<DirectMediaPolicy<'a>> {
    let binding = access.line_binding(&call.device_id, call.line_instance)?;
    let media = config.media_for_binding(&binding)?;
    let device = config.devices.get(&call.device_id)?;
    Some(DirectMediaPolicy {
        enabled: media.direct_media,
        forced_jitter_buffer: config.general.jitter_buffer.forced,
        nat: device.network.nat,
        local_networks: &config.general.network.local_networks,
    })
}

pub fn station_nat_active(
    access: &Access,
    config: &ModuleConfig,
    device_id: &DeviceId,
) -> Option<bool> {
    let policy = address_selection_policy(config, device_id, ResolvedExternalAddresses::default())?;
    let (signaling_peer, registration_reported) = station_address_context(access, device_id)?;
    Some(
        policy
            .nat_decision(signaling_peer, registration_reported)
            .active,
    )
}

/// Resolve the handset media transaction from the same registration and NAT
/// policy used for address selection. Unknown or incomplete station state
/// fails closed to the ordinary acknowledgement-gated transaction.
pub fn outbound_media_mode(access: &Access, device_id: &DeviceId) -> OutboundMediaMode {
    let config = access.config();
    if station_nat_active(access, &config, device_id).unwrap_or(false) {
        OutboundMediaMode::Coupled
    } else {
        OutboundMediaMode::Staged
    }
}
