//! Family-specific codec helpers delegated to by the exhaustive central dispatch.

use super::*;

pub(super) const PRE_19_CONNECTION_STATISTICS_BASE_BYTES: usize = 60;
pub(super) const PRE_19_CONNECTION_STATISTICS_PREFIX_BYTES: usize = 64;
pub(super) const ALIGNED_CONNECTION_STATISTICS_BASE_BYTES: usize = 64;
pub(super) const ALIGNED_CONNECTION_STATISTICS_PREFIX_BYTES: usize = 68;
const PACKED_CONNECTION_STATISTICS_BASE_BYTES: usize = 61;
const PACKED_CONNECTION_STATISTICS_PREFIX_BYTES: usize = 65;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConnectionStatisticsWireLayout {
    Pre19,
    Aligned,
    Packed,
}

fn decode_connection_statistics_quality(
    payload: &[u8],
    prefix_bytes: usize,
    quality_size: u32,
    message_id: u32,
) -> Result<Vec<u8>, CodecError> {
    let quality_size = usize_from_wire(message_id, "quality statistics", quality_size)?;
    if quality_size > CONNECTION_QUALITY_MAX_BYTES {
        return Err(CodecError::CountTooLarge {
            message_id,
            field: "quality statistics",
            count: quality_size,
            maximum: CONNECTION_QUALITY_MAX_BYTES,
        });
    }
    let meaningful_end = prefix_bytes
        .checked_add(quality_size)
        .ok_or(CodecError::InvalidLength(message_id))?;
    if payload.len() < meaningful_end {
        return Err(CodecError::Truncated {
            message_id,
            needed: meaningful_end,
            actual: payload.len(),
        });
    }
    let reservoir_end = prefix_bytes + CONNECTION_QUALITY_MAX_BYTES;
    let suffix = &payload[meaningful_end..];
    let fixed_reservoir_padding = (4 - reservoir_end % 4) % 4;
    if payload.len() >= reservoir_end && payload.len() <= reservoir_end + fixed_reservoir_padding {
        validate_zero_payload(
            &payload[reservoir_end..],
            message_id,
            payload.len() - reservoir_end,
        )?;
    } else if suffix.len() <= 3 {
        validate_zero_payload(suffix, message_id, suffix.len())?;
    } else {
        return Err(CodecError::TrailingBytes {
            message_id,
            count: suffix.len(),
        });
    }
    Ok(payload[prefix_bytes..meaningful_end].to_vec())
}

fn connection_statistics_from_wire<const TEXT_BYTES: usize, const ALIGNMENT_BYTES: usize>(
    directory_number: WireAlignedText<TEXT_BYTES, ALIGNMENT_BYTES>,
    call_reference: u32,
    processing: u32,
    counters: WireConnectionStatisticsCounters,
    quality: Vec<u8>,
    message_id: u32,
) -> Result<ConnectionStatistics, CodecError> {
    directory_number.validate(message_id)?;
    Ok(ConnectionStatistics {
        directory_number: directory_number.text()?,
        call_reference,
        processing: StatisticsProcessing::from(processing),
        packets_sent: counters.packets_sent,
        octets_sent: counters.octets_sent,
        packets_received: counters.packets_received,
        octets_received: counters.octets_received,
        packets_lost: counters.packets_lost,
        jitter_millis: counters.jitter_millis,
        latency_millis: counters.latency_millis,
        quality: ConnectionQualityStatistics::new(quality)?,
    })
}

pub(super) fn decode_connection_statistics(
    payload: &[u8],
    protocol: u32,
    message_id: u32,
) -> Result<ConnectionStatistics, CodecError> {
    fn select_layout(
        preferred: Result<ConnectionStatistics, CodecError>,
        fallback: Result<ConnectionStatistics, CodecError>,
    ) -> Result<ConnectionStatistics, CodecError> {
        match (preferred, fallback) {
            (Ok(preferred), Ok(_)) => Ok(preferred),
            (Ok(statistics), Err(_)) | (Err(_), Ok(statistics)) => Ok(statistics),
            (Err(error), Err(_)) => Err(error),
        }
    }

    fn decode_layout(
        layout: ConnectionStatisticsWireLayout,
        payload: &[u8],
        message_id: u32,
    ) -> Result<ConnectionStatistics, CodecError> {
        match layout {
            ConnectionStatisticsWireLayout::Pre19 => {
                let base: WireConnectionStatisticsV3Base = decode_prefix(message_id, payload)?;
                let quality = match payload.len() {
                    PRE_19_CONNECTION_STATISTICS_BASE_BYTES => Vec::new(),
                    length if length < PRE_19_CONNECTION_STATISTICS_PREFIX_BYTES => {
                        return Err(CodecError::Truncated {
                            message_id,
                            needed: PRE_19_CONNECTION_STATISTICS_PREFIX_BYTES,
                            actual: length,
                        });
                    }
                    _ => {
                        let value: WireConnectionStatisticsV3Prefix =
                            decode_prefix(message_id, payload)?;
                        decode_connection_statistics_quality(
                            payload,
                            PRE_19_CONNECTION_STATISTICS_PREFIX_BYTES,
                            value.statistics.quality_size,
                            message_id,
                        )?
                    }
                };
                connection_statistics_from_wire(
                    base.directory_number,
                    base.call_reference,
                    base.processing.to_wire(),
                    base.counters,
                    quality,
                    message_id,
                )
            }
            ConnectionStatisticsWireLayout::Aligned => {
                let base: WireConnectionStatisticsV19Base = decode_prefix(message_id, payload)?;
                let quality = match payload.len() {
                    ALIGNED_CONNECTION_STATISTICS_BASE_BYTES => Vec::new(),
                    length if length < ALIGNED_CONNECTION_STATISTICS_PREFIX_BYTES => {
                        return Err(CodecError::Truncated {
                            message_id,
                            needed: ALIGNED_CONNECTION_STATISTICS_PREFIX_BYTES,
                            actual: length,
                        });
                    }
                    _ => {
                        let value: WireConnectionStatisticsV19Prefix =
                            decode_prefix(message_id, payload)?;
                        decode_connection_statistics_quality(
                            payload,
                            ALIGNED_CONNECTION_STATISTICS_PREFIX_BYTES,
                            value.statistics.quality_size,
                            message_id,
                        )?
                    }
                };
                connection_statistics_from_wire(
                    base.directory_number,
                    base.call_reference,
                    base.processing.to_wire(),
                    base.counters,
                    quality,
                    message_id,
                )
            }
            ConnectionStatisticsWireLayout::Packed => {
                let base: WireConnectionStatisticsPackedBase = decode_prefix(message_id, payload)?;
                let quality = match payload.len() {
                    PACKED_CONNECTION_STATISTICS_BASE_BYTES..=64 => {
                        validate_zero_payload(
                            &payload[PACKED_CONNECTION_STATISTICS_BASE_BYTES..],
                            message_id,
                            payload.len() - PACKED_CONNECTION_STATISTICS_BASE_BYTES,
                        )?;
                        Vec::new()
                    }
                    _ => {
                        let value: WireConnectionStatisticsPackedPrefix =
                            decode_prefix(message_id, payload)?;
                        decode_connection_statistics_quality(
                            payload,
                            PACKED_CONNECTION_STATISTICS_PREFIX_BYTES,
                            value.quality_size,
                            message_id,
                        )?
                    }
                };
                connection_statistics_from_wire(
                    base.directory_number,
                    base.call_reference,
                    base.processing.to_wire(),
                    base.counters,
                    quality,
                    message_id,
                )
            }
        }
    }

    match protocol {
        ..=18 => decode_layout(ConnectionStatisticsWireLayout::Pre19, payload, message_id),
        // A 64-byte response can be either the new aligned base or a legacy
        // prefix with an empty quality tail. Protocol 19 defines the former.
        19 => select_layout(
            decode_layout(ConnectionStatisticsWireLayout::Aligned, payload, message_id),
            decode_layout(ConnectionStatisticsWireLayout::Pre19, payload, message_id),
        ),
        20..=21 => decode_layout(ConnectionStatisticsWireLayout::Aligned, payload, message_id),
        // Some v22 stations use the 61-byte packed base and pad it to a word
        // boundary, so the packed interpretation wins an otherwise valid tie.
        22.. => select_layout(
            decode_layout(ConnectionStatisticsWireLayout::Packed, payload, message_id),
            decode_layout(ConnectionStatisticsWireLayout::Aligned, payload, message_id),
        ),
    }
}

pub(super) fn decode_start_media_ack(
    payload: &[u8],
    protocol: u32,
    message_id: u32,
) -> Result<MediaTransmissionAck, CodecError> {
    match protocol {
        17.. => {
            let (value, extension) = match payload.len() {
                40 => (decode::<WireStartMediaAckV17>(message_id, payload)?, None),
                48 => {
                    let value = decode::<WireStartMediaAckV20>(message_id, payload)?;
                    (value.base, Some(value.extension))
                }
                actual => {
                    return Err(match actual {
                        0..40 => CodecError::Truncated {
                            message_id,
                            needed: 40,
                            actual,
                        },
                        _ => CodecError::TrailingBytes {
                            message_id,
                            count: actual - 40,
                        },
                    });
                }
            };
            media_transmission_ack_from_wire(value, extension, message_id)
        }
        _ => media_transmission_ack_from_wire(
            decode::<WireStartMediaAckV3>(message_id, payload)?,
            None,
            message_id,
        ),
    }
}

fn media_transmission_ack_from_wire<Address: WireIpAddress>(
    value: WireStartMediaAck<Address>,
    extension: Option<[u8; 8]>,
    message_id: u32,
) -> Result<MediaTransmissionAck, CodecError> {
    Ok(MediaTransmissionAck {
        conference_id: value.conference_id,
        passthrough_party_id: value.passthrough_party_id,
        call_reference: value.call_reference,
        status: MediaStatus::from(value.status),
        address: value.address.to_ip(message_id)?,
        port: decode_port(value.port, message_id, "RTP port")?,
        wire: extension.map(|extension| MediaTransmissionAckWire {
            extension: Some(extension),
        }),
    })
}

pub(super) fn decode_user_data(
    payload: &[u8],
    message_id: u32,
) -> Result<UserDataMessage, CodecError> {
    let value: WireUserData = decode_zero_padded(message_id, payload)?;
    if value.data.len() > 2000 {
        return Err(CodecError::CountTooLarge {
            message_id,
            field: "user data",
            count: value.data.len(),
            maximum: 2000,
        });
    }
    Ok(UserDataMessage {
        application_id: value.header.application_id,
        line_instance: value.header.line_instance,
        call_reference: value.header.call_reference,
        transaction_id: value.header.transaction_id,
        data: value.data,
    })
}

pub(super) fn decode_open_multimedia_ack(
    payload: &[u8],
    protocol: u32,
    message_id: u32,
) -> Result<OpenMultimediaReceiveChannelAck, CodecError> {
    let (status, address, port, passthrough_party_id, call_reference) = match protocol {
        17.. => receive_channel_ack_from_wire(
            decode::<WireOpenMultimediaAckFrom17>(message_id, payload)?,
            message_id,
        )?,
        _ => receive_channel_ack_from_wire(
            decode::<WireOpenMultimediaAckPre17>(message_id, payload)?,
            message_id,
        )?,
    };
    Ok(OpenMultimediaReceiveChannelAck {
        status: MediaStatus::from(status),
        endpoint: MediaEndpointAddress {
            address,
            port: decode_port(port, message_id, "multimedia port")?,
        },
        passthrough_party_id: passthrough_party_id.into(),
        call_reference: call_reference.into(),
    })
}

fn receive_channel_ack_from_wire<Address: WireIpAddress>(
    value: WireReceiveChannelAck<Address>,
    message_id: u32,
) -> Result<(u32, IpAddr, u32, u32, u32), CodecError> {
    Ok((
        value.status,
        value.address.to_ip(message_id)?,
        value.port,
        value.passthrough_party_id,
        value.call_reference,
    ))
}

pub(super) fn encode_open_multimedia_ack(
    value: OpenMultimediaReceiveChannelAck,
    protocol: ProtocolVersion,
) -> Result<Vec<u8>, CodecError> {
    match protocol.wire() {
        17.. => encode(
            wire_id::OPEN_MULTIMEDIA_RECEIVE_CHANNEL_ACK,
            &WireOpenMultimediaAckFrom17 {
                status: value.status.wire_value(),
                address: WireExtendedAddress::from_ip(value.endpoint.address),
                port: u32::from(value.endpoint.port),
                passthrough_party_id: value.passthrough_party_id.get(),
                call_reference: value.call_reference.get(),
            },
        ),
        _ => encode(
            wire_id::OPEN_MULTIMEDIA_RECEIVE_CHANNEL_ACK,
            &WireOpenMultimediaAckPre17 {
                status: value.status.wire_value(),
                address: WireIpv4Address::from_ip(
                    value.endpoint.address,
                    wire_id::OPEN_MULTIMEDIA_RECEIVE_CHANNEL_ACK,
                    "IP address family for this protocol version",
                )?,
                port: u32::from(value.endpoint.port),
                passthrough_party_id: value.passthrough_party_id.get(),
                call_reference: value.call_reference.get(),
            },
        ),
    }
}

pub(super) fn decode_start_multimedia_ack(
    payload: &[u8],
    protocol: u32,
    message_id: u32,
) -> Result<StartMultimediaTransmissionAck, CodecError> {
    let (conference_id, party_id, call_reference, address, port, status) = match protocol {
        17.. => start_multimedia_ack_from_wire(
            decode::<WireStartMultimediaAckFrom17>(message_id, payload)?,
            message_id,
        )?,
        _ => start_multimedia_ack_from_wire(
            decode::<WireStartMultimediaAckPre17>(message_id, payload)?,
            message_id,
        )?,
    };
    Ok(StartMultimediaTransmissionAck {
        conference_id: conference_id.into(),
        passthrough_party_id: party_id.into(),
        call_reference: call_reference.into(),
        endpoint: MediaEndpointAddress {
            address,
            port: decode_port(port, message_id, "multimedia port")?,
        },
        status: MediaStatus::from(status),
    })
}

fn start_multimedia_ack_from_wire<Address: WireIpAddress>(
    value: WireStartMediaAck<Address>,
    message_id: u32,
) -> Result<(u32, u32, u32, IpAddr, u32, u32), CodecError> {
    Ok((
        value.conference_id,
        value.passthrough_party_id,
        value.call_reference,
        value.address.to_ip(message_id)?,
        value.port,
        value.status,
    ))
}

pub(super) fn encode_start_multimedia_ack(
    value: StartMultimediaTransmissionAck,
    protocol: ProtocolVersion,
) -> Result<Vec<u8>, CodecError> {
    match protocol.wire() {
        17.. => encode(
            wire_id::START_MULTIMEDIA_TRANSMISSION_ACK,
            &WireStartMultimediaAckFrom17 {
                conference_id: value.conference_id.get(),
                passthrough_party_id: value.passthrough_party_id.get(),
                call_reference: value.call_reference.get(),
                address: WireExtendedAddress::from_ip(value.endpoint.address),
                port: u32::from(value.endpoint.port),
                status: value.status.wire_value(),
            },
        ),
        _ => encode(
            wire_id::START_MULTIMEDIA_TRANSMISSION_ACK,
            &WireStartMultimediaAckPre17 {
                conference_id: value.conference_id.get(),
                passthrough_party_id: value.passthrough_party_id.get(),
                call_reference: value.call_reference.get(),
                address: WireIpv4Address::from_ip(
                    value.endpoint.address,
                    wire_id::START_MULTIMEDIA_TRANSMISSION_ACK,
                    "IP address family for this protocol version",
                )?,
                port: u32::from(value.endpoint.port),
                status: value.status.wire_value(),
            },
        ),
    }
}

pub(super) fn decode_session_transmission(
    payload: &[u8],
    protocol: ProtocolVersion,
    message_id: u32,
) -> Result<SessionTransmission, CodecError> {
    match protocol.wire() {
        17.. => session_transmission_from_wire(
            decode::<WireSessionTransmissionFrom17>(message_id, payload)?,
            message_id,
        ),
        _ => session_transmission_from_wire(
            decode::<WireSessionTransmissionPre17>(message_id, payload)?,
            message_id,
        ),
    }
}

fn session_transmission_from_wire<Address: WireIpAddress>(
    value: WireSessionTransmission<Address>,
    message_id: u32,
) -> Result<SessionTransmission, CodecError> {
    Ok(SessionTransmission {
        remote_address: value.remote_address.to_ip(message_id)?,
        session_type: value.session_type,
    })
}

pub(super) fn encode_session_transmission(
    value: SessionTransmission,
    protocol: ProtocolVersion,
    message_id: u32,
) -> Result<Vec<u8>, CodecError> {
    match protocol.wire() {
        17.. => encode(
            message_id,
            &WireSessionTransmissionFrom17 {
                remote_address: WireExtendedAddress::from_ip(value.remote_address),
                session_type: value.session_type,
            },
        ),
        _ => encode(
            message_id,
            &WireSessionTransmissionPre17 {
                remote_address: WireIpv4Address::from_ip(
                    value.remote_address,
                    message_id,
                    "IP address family for this protocol version",
                )?,
                session_type: value.session_type,
            },
        ),
    }
}

pub(super) fn fixed_bounded_bytes<const MAX: usize>(value: &BoundedBytes<MAX>) -> [u8; MAX] {
    let mut bytes = [0; MAX];
    bytes[..value.len()].copy_from_slice(value.as_bytes());
    bytes
}

pub(super) fn bounded_from_fixed<const MAX: usize>(value: [u8; MAX]) -> BoundedBytes<MAX> {
    BoundedBytes::try_from(value.as_slice()).expect("fixed wire field fits its public bound")
}

pub(super) fn multimedia_capability_words(
    value: [u8; MULTIMEDIA_CAPABILITY_BYTES],
) -> [u32; MULTIMEDIA_CAPABILITY_BYTES / 4] {
    std::array::from_fn(|index| {
        let offset = index * 4;
        u32::from_le_bytes([
            value[offset],
            value[offset + 1],
            value[offset + 2],
            value[offset + 3],
        ])
    })
}

pub(super) fn multimedia_capability_bytes(
    words: [u32; MULTIMEDIA_CAPABILITY_BYTES / 4],
) -> [u8; MULTIMEDIA_CAPABILITY_BYTES] {
    let mut bytes = [0; MULTIMEDIA_CAPABILITY_BYTES];
    for (word, target) in words.iter().zip(bytes.as_chunks_mut::<4>().0) {
        target.copy_from_slice(&word.to_le_bytes());
    }
    bytes
}

pub(super) fn decode_multimedia_descriptor(
    value: WireMultimediaPayloadDescriptor,
    message_id: u32,
) -> Result<MultimediaPayloadDescriptor, CodecError> {
    let payload_number =
        RtpPayloadNumber::new(value.payload_type).map_err(|error| CodecError::InvalidValue {
            message_id,
            field: "RTP payload number",
            value: u64::from(error.actual),
        })?;
    Ok(MultimediaPayloadDescriptor::new(
        value.payload_rfc_number,
        payload_number,
    ))
}

pub(super) fn decoded_multimedia_capability(
    value: [u8; MULTIMEDIA_CAPABILITY_BYTES],
    codec: Codec,
) -> MultimediaCapabilityState {
    let words = multimedia_capability_words(value);
    let Ok(picture_count) = usize::try_from(words[1]) else {
        return MultimediaCapabilityState::Preserved(value);
    };
    if picture_count > MAX_MULTIMEDIA_PICTURE_FORMATS {
        return MultimediaCapabilityState::Preserved(value);
    }
    let arm = match codec {
        Codec::H261 if words[15..].iter().all(|word| *word == 0) => {
            MultimediaVideoCapabilityArm::H261 {
                temporal_spatial_trade_off_capability: words[13],
                still_image_transmission: words[14],
            }
        }
        Codec::H263 if words[15..].iter().all(|word| *word == 0) => {
            MultimediaVideoCapabilityArm::H263 {
                capability_bitfield: words[13],
                annex_n_and_w_future_use: words[14],
            }
        }
        Codec::H263Plus if words[15..].iter().all(|word| *word == 0) => {
            MultimediaVideoCapabilityArm::H263Plus {
                model_number: words[13],
                bandwidth: words[14],
            }
        }
        Codec::H264 => MultimediaVideoCapabilityArm::H264 {
            profile: words[13],
            level: words[14],
            custom_max_mbps: words[15],
            custom_max_fs: words[16],
            custom_max_dpb: words[17],
            custom_max_br_and_cpb: words[18],
        },
        _ => {
            return MultimediaCapabilityState::Preserved(value);
        }
    };
    let picture_formats = (0..picture_count)
        .map(|index| MultimediaPictureFormat {
            format: VideoFormat::from(words[2 + index * 2]),
            minimum_picture_interval: words[3 + index * 2],
        })
        .collect();
    MultimediaCapabilityState::Video(MultimediaVideoCapability {
        bit_rate: words[0],
        picture_formats,
        conference_service_number: words[12],
        arm,
        preserved_wire: Some(value),
    })
}

pub(super) fn multimedia_capability_to_wire(
    payload: &MultimediaPayload,
) -> [u8; MULTIMEDIA_CAPABILITY_BYTES] {
    let capability = match &payload.capability {
        MultimediaCapabilityState::Preserved(bytes) => return *bytes,
        MultimediaCapabilityState::Video(capability) => capability,
    };
    if let Some(value) = capability.preserved_wire {
        return value;
    }

    let mut words = [0; MULTIMEDIA_CAPABILITY_BYTES / 4];
    words[0] = capability.bit_rate;
    words[1] = capability.picture_formats.len() as u32;
    for (index, format) in capability.picture_formats.iter().enumerate() {
        words[2 + index * 2] = format.format.wire_value();
        words[3 + index * 2] = format.minimum_picture_interval;
    }
    words[12] = capability.conference_service_number;
    match capability.arm {
        MultimediaVideoCapabilityArm::H261 {
            temporal_spatial_trade_off_capability,
            still_image_transmission,
        } => {
            words[13] = temporal_spatial_trade_off_capability;
            words[14] = still_image_transmission;
        }
        MultimediaVideoCapabilityArm::H263 {
            capability_bitfield,
            annex_n_and_w_future_use,
        } => {
            words[13] = capability_bitfield;
            words[14] = annex_n_and_w_future_use;
        }
        MultimediaVideoCapabilityArm::H263Plus {
            model_number,
            bandwidth,
        } => {
            words[13] = model_number;
            words[14] = bandwidth;
        }
        MultimediaVideoCapabilityArm::H264 {
            profile,
            level,
            custom_max_mbps,
            custom_max_fs,
            custom_max_dpb,
            custom_max_br_and_cpb,
        } => {
            words[13] = profile;
            words[14] = level;
            words[15] = custom_max_mbps;
            words[16] = custom_max_fs;
            words[17] = custom_max_dpb;
            words[18] = custom_max_br_and_cpb;
        }
    }
    multimedia_capability_bytes(words)
}

pub(super) fn decode_multimedia_payload(
    descriptor: WireMultimediaPayloadDescriptor,
    capability: [u8; MULTIMEDIA_CAPABILITY_BYTES],
    compression_codec: Codec,
    direction: MultimediaPayloadDirection,
    protocol: ProtocolVersion,
    message_id: u32,
) -> Result<MultimediaPayload, CodecError> {
    let descriptor = decode_multimedia_descriptor(descriptor, message_id)?;
    let capability = decoded_multimedia_capability(capability, compression_codec);
    Ok(MultimediaPayload::from_decoded(
        descriptor,
        capability,
        direction,
        protocol,
        compression_codec,
    ))
}

pub(super) fn validate_multimedia_payload(
    payload: &MultimediaPayload,
    direction: MultimediaPayloadDirection,
    protocol: ProtocolVersion,
    message_id: u32,
) -> Result<(), CodecError> {
    if payload.is_valid_for(direction, protocol) {
        Ok(())
    } else {
        Err(CodecError::InvalidValue {
            message_id,
            field: "multimedia payload provenance",
            value: 1,
        })
    }
}

pub(super) fn open_multimedia_from_common(
    value: WireOpenMultimediaV11,
    source: MediaEndpointAddress,
    requested_address_type: IpAddressType,
    protocol: ProtocolVersion,
    message_id: u32,
) -> Result<OpenMultimediaChannel, CodecError> {
    let codec = Codec::from(value.compression_type);
    Ok(OpenMultimediaChannel {
        conference_id: value.conference_id.into(),
        passthrough_party_id: value.passthrough_party_id.into(),
        line_instance: value.line_instance,
        call_reference: value.call_reference.into(),
        payload: decode_multimedia_payload(
            value.payload_type,
            value.capability,
            codec,
            MultimediaPayloadDirection::Receive,
            protocol,
            message_id,
        )?,
        conference_creator: decode_bool_word(
            value.conference_creator,
            message_id,
            "conference creator",
        )?,
        encryption: value.encryption.to_public(message_id)?,
        stream_passthrough_id: value.stream_passthrough_id,
        associated_stream_id: value.associated_stream_id,
        source,
        requested_address_type,
    })
}

pub(super) fn decode_open_multimedia(
    payload: &[u8],
    protocol: ProtocolVersion,
    message_id: u32,
) -> Result<OpenMultimediaChannel, CodecError> {
    match protocol.wire() {
        17.. => {
            let value: WireOpenMultimediaV17 = decode(message_id, payload)?;
            open_multimedia_from_common(
                value.base.base,
                MediaEndpointAddress {
                    address: value.base.source_address.to_ip(message_id)?,
                    port: decode_port(
                        value.base.source_port,
                        message_id,
                        "multimedia source port",
                    )?,
                },
                IpAddressType::from(value.requested_address_type),
                protocol,
                message_id,
            )
        }
        12..=16 => {
            let value: WireOpenMultimediaV12 = decode(message_id, payload)?;
            open_multimedia_from_common(
                value.base,
                MediaEndpointAddress {
                    address: value.source_address.to_ip(message_id)?,
                    port: decode_port(value.source_port, message_id, "multimedia source port")?,
                },
                IpAddressType::Ipv4,
                protocol,
                message_id,
            )
        }
        _ => {
            let value: WireOpenMultimediaV11 = decode(message_id, payload)?;
            open_multimedia_from_common(
                value,
                MediaEndpointAddress {
                    address: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    port: 0,
                },
                IpAddressType::Ipv4,
                protocol,
                message_id,
            )
        }
    }
}

pub(super) fn encode_open_multimedia(
    value: &OpenMultimediaChannel,
    protocol: ProtocolVersion,
) -> Result<Vec<u8>, CodecError> {
    validate_multimedia_payload(
        &value.payload,
        MultimediaPayloadDirection::Receive,
        protocol,
        wire_id::OPEN_MULTIMEDIA_CHANNEL,
    )?;
    let common = WireOpenMultimediaV11 {
        conference_id: value.conference_id.get(),
        passthrough_party_id: value.passthrough_party_id.get(),
        compression_type: value.payload.compression_codec().wire_value(),
        line_instance: value.line_instance,
        call_reference: value.call_reference.get(),
        payload_type: value.payload.descriptor().into(),
        conference_creator: u32::from(value.conference_creator),
        capability: multimedia_capability_to_wire(&value.payload),
        encryption: WireEncryptionInfo::from_public(value.encryption.as_ref()),
        stream_passthrough_id: value.stream_passthrough_id,
        associated_stream_id: value.associated_stream_id,
    };
    match protocol.wire() {
        17.. => encode(
            wire_id::OPEN_MULTIMEDIA_CHANNEL,
            &WireOpenMultimediaV17 {
                base: WireOpenMultimediaAddressed {
                    base: common,
                    source_address: WireExtendedAddress::from_ip(value.source.address),
                    source_port: u32::from(value.source.port),
                },
                requested_address_type: value.requested_address_type.wire_value(),
            },
        ),
        12..=16 => encode(
            wire_id::OPEN_MULTIMEDIA_CHANNEL,
            &WireOpenMultimediaV12 {
                base: common,
                source_address: WireIpv4Address::from_ip(
                    value.source.address,
                    wire_id::OPEN_MULTIMEDIA_CHANNEL,
                    "IP address family for this protocol version",
                )?,
                source_port: u32::from(value.source.port),
            },
        ),
        _ => encode(wire_id::OPEN_MULTIMEDIA_CHANNEL, &common),
    }
}

pub(super) fn decode_start_multimedia(
    payload: &[u8],
    protocol: ProtocolVersion,
    message_id: u32,
) -> Result<StartMultimediaTransmission, CodecError> {
    match protocol.wire() {
        17.. => start_multimedia_from_wire(
            decode::<WireStartMultimediaFrom17>(message_id, payload)?,
            protocol,
            message_id,
        ),
        _ => start_multimedia_from_wire(
            decode::<WireStartMultimediaPre17>(message_id, payload)?,
            protocol,
            message_id,
        ),
    }
}

fn start_multimedia_from_wire<Address: WireIpAddress>(
    value: WireStartMultimedia<Address>,
    protocol: ProtocolVersion,
    message_id: u32,
) -> Result<StartMultimediaTransmission, CodecError> {
    Ok(StartMultimediaTransmission {
        conference_id: value.conference_id.into(),
        passthrough_party_id: value.passthrough_party_id.into(),
        endpoint: MediaEndpointAddress {
            address: value.remote_address.to_ip(message_id)?,
            port: decode_port(value.remote_port, message_id, "multimedia port")?,
        },
        call_reference: value.call_reference.into(),
        payload: decode_multimedia_payload(
            value.payload_type,
            value.capability,
            Codec::from(value.compression_type),
            MultimediaPayloadDirection::Transmit,
            protocol,
            message_id,
        )?,
        traffic_class: MediaTrafficClass::from_wire(u8::try_from(value.dscp).map_err(|_| {
            CodecError::InvalidValue {
                message_id,
                field: "multimedia traffic class",
                value: u64::from(value.dscp),
            }
        })?),
        encryption: value.encryption.to_public(message_id)?,
        stream_passthrough_id: value.stream_passthrough_id,
        associated_stream_id: value.associated_stream_id,
    })
}

fn start_multimedia_to_wire<Address: WireIpAddress>(
    value: &StartMultimediaTransmission,
) -> Result<WireStartMultimedia<Address>, CodecError> {
    Ok(WireStartMultimedia {
        conference_id: value.conference_id.get(),
        passthrough_party_id: value.passthrough_party_id.get(),
        compression_type: value.payload.compression_codec().wire_value(),
        remote_address: Address::from_ip(
            value.endpoint.address,
            wire_id::START_MULTIMEDIA_TRANSMISSION,
            "IP address family for this protocol version",
        )?,
        remote_port: u32::from(value.endpoint.port),
        call_reference: value.call_reference.get(),
        payload_type: value.payload.descriptor().into(),
        dscp: u32::from(value.traffic_class),
        capability: multimedia_capability_to_wire(&value.payload),
        encryption: WireEncryptionInfo::from_public(value.encryption.as_ref()),
        stream_passthrough_id: value.stream_passthrough_id,
        associated_stream_id: value.associated_stream_id,
    })
}

pub(super) fn encode_start_multimedia(
    value: &StartMultimediaTransmission,
    protocol: ProtocolVersion,
) -> Result<Vec<u8>, CodecError> {
    validate_multimedia_payload(
        &value.payload,
        MultimediaPayloadDirection::Transmit,
        protocol,
        wire_id::START_MULTIMEDIA_TRANSMISSION,
    )?;
    match protocol.wire() {
        17.. => encode(
            wire_id::START_MULTIMEDIA_TRANSMISSION,
            &start_multimedia_to_wire::<WireExtendedAddress>(value)?,
        ),
        _ => encode(
            wire_id::START_MULTIMEDIA_TRANSMISSION,
            &start_multimedia_to_wire::<WireIpv4Address>(value)?,
        ),
    }
}

pub(super) fn decode_miscellaneous_command(
    payload: &[u8],
    message_id: u32,
) -> Result<MiscellaneousCommand, CodecError> {
    let value: WireMiscellaneousCommand = decode(message_id, payload)?;
    Ok(MiscellaneousCommand {
        conference_id: value.conference_id.into(),
        passthrough_party_id: value.passthrough_party_id.into(),
        call_reference: value.call_reference.into(),
        command: values::MiscCommandType::from(value.command),
        data: bounded_from_fixed(value.data),
    })
}

pub(super) fn encode_miscellaneous_command(
    value: &MiscellaneousCommand,
) -> Result<Vec<u8>, CodecError> {
    encode(
        wire_id::MISCELLANEOUS_COMMAND,
        &WireMiscellaneousCommand {
            conference_id: value.conference_id.get(),
            passthrough_party_id: value.passthrough_party_id.get(),
            call_reference: value.call_reference.get(),
            command: value.command.wire_value(),
            data: fixed_bounded_bytes(&value.data),
        },
    )
}

pub(super) fn dtmf_payload_identity_from_wire(
    value: WireDtmfPayloadIdentity,
) -> DtmfPayloadIdentity {
    DtmfPayloadIdentity {
        payload_type: value.payload_type,
        conference_id: value.conference_id,
        passthrough_party_id: value.passthrough_party_id,
    }
}

pub(super) fn dtmf_payload_identity_to_wire(value: DtmfPayloadIdentity) -> WireDtmfPayloadIdentity {
    WireDtmfPayloadIdentity {
        payload_type: value.payload_type,
        conference_id: value.conference_id,
        passthrough_party_id: value.passthrough_party_id,
    }
}

pub(super) fn dtmf_payload_request_from_wire(value: WireDtmfPayloadRequest) -> DtmfPayloadRequest {
    DtmfPayloadRequest {
        payload_type: value.payload_type,
        conference_id: value.conference_id,
        passthrough_party_id: value.passthrough_party_id,
        dtmf_type: value.dtmf_type,
    }
}

pub(super) fn dtmf_payload_request_to_wire(value: DtmfPayloadRequest) -> WireDtmfPayloadRequest {
    WireDtmfPayloadRequest {
        payload_type: value.payload_type,
        conference_id: value.conference_id,
        passthrough_party_id: value.passthrough_party_id,
        dtmf_type: value.dtmf_type,
    }
}
