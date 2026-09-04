//! Family-specific codec helpers delegated to by the exhaustive central dispatch.

use super::*;

pub(super) fn push_dynamic_station_text(
    output: &mut Vec<u8>,
    message_id: u32,
    field: &'static str,
    text: &str,
    maximum: usize,
    code_page: Option<LegacyCodePage>,
) -> Result<(), CodecError> {
    let bytes = station_text_bytes(text, code_page)?;
    if bytes.len() > maximum {
        return Err(CodecError::TextTooLong {
            message_id,
            field,
            actual: bytes.len(),
            maximum,
        });
    }
    output.extend_from_slice(&bytes);
    output.push(0);
    Ok(())
}

/// Station-variable payloads are zero-filled through the next 32-bit boundary.
pub(super) fn pad_dynamic_payload(output: &mut Vec<u8>) {
    let padding = (4 - output.len() % 4) % 4;
    output.resize(output.len() + padding, 0);
}

pub(super) fn decode_dynamic_texts(
    message_id: u32,
    payload: &[u8],
    offset: usize,
    count: usize,
) -> Result<Vec<String>, CodecError> {
    if !payload.len().is_multiple_of(4) {
        return Err(CodecError::InvalidAlignment {
            message_id,
            actual: payload.len(),
        });
    }
    let Some(mut remaining) = payload.get(offset..) else {
        return Err(CodecError::Truncated {
            message_id,
            needed: offset,
            actual: payload.len(),
        });
    };
    let mut fields = Vec::with_capacity(count);
    for _ in 0..count {
        let Some(end) = remaining.iter().position(|byte| *byte == 0) else {
            return Err(CodecError::Truncated {
                message_id,
                needed: payload.len() + 1,
                actual: payload.len(),
            });
        };
        fields.push(
            std::str::from_utf8(&remaining[..end])
                .map_err(|_| CodecError::InvalidText)?
                .to_owned(),
        );
        remaining = &remaining[end + 1..];
    }
    validate_dynamic_padding(message_id, remaining)?;
    Ok(fields)
}

pub(super) fn decode_dynamic_text(
    message_id: u32,
    payload: &[u8],
    offset: usize,
) -> Result<String, CodecError> {
    Ok(decode_dynamic_texts(message_id, payload, offset, 1)?
        .pop()
        .expect("one requested dynamic field is returned"))
}

pub(super) fn take_dynamic_text(
    message_id: u32,
    remaining: &mut &[u8],
    field: &'static str,
    maximum: usize,
) -> Result<String, CodecError> {
    let Some(end) = remaining.iter().position(|byte| *byte == 0) else {
        return Err(CodecError::Truncated {
            message_id,
            needed: remaining.len() + 1,
            actual: remaining.len(),
        });
    };
    if end > maximum {
        return Err(CodecError::TextTooLong {
            message_id,
            field,
            actual: end,
            maximum,
        });
    }
    let value = std::str::from_utf8(&remaining[..end])
        .map_err(|_| CodecError::InvalidText)?
        .to_owned();
    *remaining = &remaining[end + 1..];
    Ok(value)
}

pub(super) fn take_dynamic_word(message_id: u32, remaining: &mut &[u8]) -> Result<u32, CodecError> {
    let Some((word, tail)) = remaining.split_first_chunk::<4>() else {
        return Err(CodecError::Truncated {
            message_id,
            needed: 4,
            actual: remaining.len(),
        });
    };
    *remaining = tail;
    Ok(u32::from_le_bytes(*word))
}

pub(super) fn decode_dynamic_config_status(payload: &[u8]) -> Result<ServerMessage, CodecError> {
    const MESSAGE_ID: u32 = wire_id::CONFIG_STAT_DYNAMIC;
    if !payload.len().is_multiple_of(4) {
        return Err(CodecError::InvalidAlignment {
            message_id: MESSAGE_ID,
            actual: payload.len(),
        });
    }
    let mut remaining = payload;
    let device_name = take_dynamic_text(MESSAGE_ID, &mut remaining, "device name", 64)?;
    let reserved = take_dynamic_word(MESSAGE_ID, &mut remaining)?;
    let instance = take_dynamic_word(MESSAGE_ID, &mut remaining)?;
    let line_count = take_dynamic_word(MESSAGE_ID, &mut remaining)?;
    let speed_dial_count = take_dynamic_word(MESSAGE_ID, &mut remaining)?;
    let user_name = take_dynamic_text(MESSAGE_ID, &mut remaining, "user name", 64)?;
    let server_name = take_dynamic_text(MESSAGE_ID, &mut remaining, "server name", 256)?;
    validate_dynamic_padding(MESSAGE_ID, remaining)?;
    let status = ConfigurationStatus {
        device_name,
        station_user_id: reserved,
        station_instance: instance,
        line_count,
        speed_dial_count,
        user_name,
        server_name,
    };
    Ok(ServerMessage::ConfigStatus(status))
}

pub(super) fn encode_dynamic_config_status(
    value: &ConfigurationStatus,
) -> Result<Vec<u8>, CodecError> {
    const MESSAGE_ID: u32 = wire_id::CONFIG_STAT_DYNAMIC;
    let mut payload = Vec::new();
    push_dynamic_text(
        &mut payload,
        MESSAGE_ID,
        "device name",
        &value.device_name,
        64,
    )?;
    payload.extend_from_slice(&value.station_user_id.to_le_bytes());
    payload.extend_from_slice(&value.station_instance.to_le_bytes());
    payload.extend_from_slice(&value.line_count.to_le_bytes());
    payload.extend_from_slice(&value.speed_dial_count.to_le_bytes());
    push_dynamic_text(&mut payload, MESSAGE_ID, "user name", &value.user_name, 64)?;
    push_dynamic_text(
        &mut payload,
        MESSAGE_ID,
        "server name",
        &value.server_name,
        256,
    )?;
    pad_dynamic_payload(&mut payload);
    Ok(payload)
}

pub(super) fn validate_dynamic_padding(message_id: u32, padding: &[u8]) -> Result<(), CodecError> {
    if padding.len() > 3 || padding.iter().any(|byte| *byte != 0) {
        return Err(CodecError::TrailingBytes {
            message_id,
            count: padding.len(),
        });
    }
    Ok(())
}

pub(super) fn encode_dynamic_line_status(
    instance: u32,
    directory_number: &str,
    fully_qualified_display_name: &str,
    display_label: &str,
    code_page: Option<LegacyCodePage>,
) -> Result<Vec<u8>, CodecError> {
    let mut payload = encode(
        wire_id::LINE_STAT_DYNAMIC,
        &WireLineStatusDynamicHeader {
            line_instance: instance,
            line_type: 15,
        },
    )?;
    push_dynamic_text(
        &mut payload,
        wire_id::LINE_STAT_DYNAMIC,
        "line number",
        directory_number,
        24,
    )?;
    push_dynamic_station_text(
        &mut payload,
        wire_id::LINE_STAT_DYNAMIC,
        "fully qualified display name",
        fully_qualified_display_name,
        120,
        code_page,
    )?;
    push_dynamic_station_text(
        &mut payload,
        wire_id::LINE_STAT_DYNAMIC,
        "line label",
        display_label,
        120,
        code_page,
    )?;
    pad_dynamic_payload(&mut payload);
    Ok(payload)
}

pub(super) fn encode_dynamic_speed_dial_status(
    instance: u32,
    number: &str,
    display_name: &str,
    code_page: Option<LegacyCodePage>,
) -> Result<Vec<u8>, CodecError> {
    const MESSAGE_ID: u32 = wire_id::SPEED_DIAL_STAT_DYNAMIC;
    let mut payload = instance.to_le_bytes().to_vec();
    push_dynamic_text(&mut payload, MESSAGE_ID, "number", number, 23)?;
    push_dynamic_station_text(
        &mut payload,
        MESSAGE_ID,
        "display name",
        display_name,
        39,
        code_page,
    )?;
    pad_dynamic_payload(&mut payload);
    Ok(payload)
}

pub(super) fn decode_dynamic_speed_dial_status(
    payload: &[u8],
) -> Result<ServerMessage, CodecError> {
    const MESSAGE_ID: u32 = wire_id::SPEED_DIAL_STAT_DYNAMIC;
    if !payload.len().is_multiple_of(4) {
        return Err(CodecError::InvalidAlignment {
            message_id: MESSAGE_ID,
            actual: payload.len(),
        });
    }
    let mut remaining = payload;
    let instance = take_dynamic_word(MESSAGE_ID, &mut remaining)?;
    let number = take_dynamic_text(MESSAGE_ID, &mut remaining, "number", 23)?;
    let display_name = take_dynamic_text(MESSAGE_ID, &mut remaining, "display name", 39)?;
    validate_dynamic_padding(MESSAGE_ID, remaining)?;
    Ok(ServerMessage::SpeedDialStatus {
        instance,
        number,
        display_name,
    })
}

pub(super) fn decode_dynamic_line_status(payload: &[u8]) -> Result<ServerMessage, CodecError> {
    const HEADER_SIZE: usize = 8;
    if payload.len() < HEADER_SIZE {
        return Err(CodecError::Truncated {
            message_id: wire_id::LINE_STAT_DYNAMIC,
            needed: HEADER_SIZE,
            actual: payload.len(),
        });
    }
    let header: WireLineStatusDynamicHeader =
        decode(wire_id::LINE_STAT_DYNAMIC, &payload[..HEADER_SIZE])?;
    let fields = decode_dynamic_texts(wire_id::LINE_STAT_DYNAMIC, payload, HEADER_SIZE, 3)?;
    Ok(ServerMessage::LineStatus {
        instance: header.line_instance,
        directory_number: fields[0].clone(),
        fully_qualified_display_name: fields[1].clone(),
        display_label: fields[2].clone(),
    })
}

pub(super) fn encode_dynamic_service_url_status(
    index: u32,
    url: &str,
    label: &str,
    extension_text: &str,
    protocol: ProtocolVersion,
    code_page: Option<LegacyCodePage>,
) -> Result<Vec<u8>, CodecError> {
    let mut payload = encode(
        wire_id::SERVICE_URL_STAT_DYNAMIC,
        &WireOneWord { value: index },
    )?;
    push_dynamic_text(
        &mut payload,
        wire_id::SERVICE_URL_STAT_DYNAMIC,
        "service URL",
        url,
        255,
    )?;
    push_dynamic_station_text(
        &mut payload,
        wire_id::SERVICE_URL_STAT_DYNAMIC,
        "service label",
        label,
        120,
        code_page,
    )?;
    match protocol.wire() {
        19.. => push_dynamic_station_text(
            &mut payload,
            wire_id::SERVICE_URL_STAT_DYNAMIC,
            "service extension text",
            extension_text,
            120,
            code_page,
        ),
        _ => Ok(()),
    }?;
    pad_dynamic_payload(&mut payload);
    Ok(payload)
}

pub(super) fn decode_dynamic_service_url_status(
    payload: &[u8],
    protocol: ProtocolVersion,
) -> Result<ServerMessage, CodecError> {
    const HEADER_SIZE: usize = 4;
    if payload.len() < HEADER_SIZE {
        return Err(CodecError::Truncated {
            message_id: wire_id::SERVICE_URL_STAT_DYNAMIC,
            needed: HEADER_SIZE,
            actual: payload.len(),
        });
    }
    let header: WireOneWord = decode(wire_id::SERVICE_URL_STAT_DYNAMIC, &payload[..HEADER_SIZE])?;
    let field_count = match protocol.wire() {
        19.. => 3,
        _ => 2,
    };
    let fields = decode_dynamic_texts(
        wire_id::SERVICE_URL_STAT_DYNAMIC,
        payload,
        HEADER_SIZE,
        field_count,
    )?;
    Ok(ServerMessage::ServiceUrlStatus {
        index: header.value,
        url: fields[0].clone(),
        label: fields[1].clone(),
        extension_text: fields.get(2).cloned().unwrap_or_default(),
    })
}

pub(super) fn encode_dynamic_call_info(
    info: &CallInfo,
    line_instance: u32,
    call_reference: u32,
    protocol: ProtocolVersion,
) -> Result<Vec<u8>, CodecError> {
    let mut payload = encode(
        wire_id::CALL_INFO_DYNAMIC,
        &WireCallInfoDynamicHeader {
            line_instance,
            call_reference,
            call_type: match info.direction {
                crate::types::CallDirection::Inbound => 1,
                crate::types::CallDirection::Outbound => 2,
            },
            original_redirect_reason: info.original_redirect_reason,
            last_redirect_reason: info.last_redirect_reason,
            call_instance: 1,
            security_status: 0,
            party_restrictions: info.party_restrictions,
        },
    )?;

    let fields: &[(&'static str, &str, usize)] = match protocol.dynamic_call_info_layout() {
        DynamicCallInfoLayout::Fields15 => &[
            ("calling number", &info.calling_number, 24),
            ("original calling number", "", 24),
            ("called number", &info.called_number, 24),
            ("original called number", &info.original_called_number, 24),
            ("last redirecting number", &info.last_redirecting_number, 24),
            ("calling voicemail", "", 24),
            ("called voicemail", "", 24),
            ("original called voicemail", "", 24),
            ("last redirecting voicemail", "", 24),
            ("calling name", &info.calling_name, 120),
            ("called name", &info.called_name, 120),
            ("original called name", &info.original_called_name, 120),
            ("last redirecting name", &info.last_redirecting_name, 120),
            ("hunt pilot number", "", 24),
            ("hunt pilot name", "", 120),
        ],
        DynamicCallInfoLayout::Fields13 => &[
            ("calling number", &info.calling_number, 24),
            ("original calling number", "", 24),
            ("called number", &info.called_number, 24),
            ("original called number", &info.original_called_number, 24),
            ("last redirecting number", &info.last_redirecting_number, 24),
            ("calling voicemail", "", 24),
            ("called voicemail", "", 24),
            ("original called voicemail", "", 24),
            ("last redirecting voicemail", "", 24),
            ("calling name", &info.calling_name, 120),
            ("called name", &info.called_name, 120),
            ("original called name", &info.original_called_name, 120),
            ("last redirecting name", &info.last_redirecting_name, 120),
        ],
        DynamicCallInfoLayout::Fields12 => &[
            ("calling number", &info.calling_number, 24),
            ("called number", &info.called_number, 24),
            ("original called number", &info.original_called_number, 24),
            ("last redirecting number", &info.last_redirecting_number, 24),
            ("calling voicemail", "", 24),
            ("called voicemail", "", 24),
            ("original called voicemail", "", 24),
            ("last redirecting voicemail", "", 24),
            ("calling name", &info.calling_name, 120),
            ("called name", &info.called_name, 120),
            ("original called name", &info.original_called_name, 120),
            ("last redirecting name", &info.last_redirecting_name, 120),
        ],
    };
    for &(field, text, maximum) in fields {
        push_dynamic_text(
            &mut payload,
            wire_id::CALL_INFO_DYNAMIC,
            field,
            text,
            maximum,
        )?;
    }
    pad_dynamic_payload(&mut payload);
    Ok(payload)
}

pub(super) fn decode_dynamic_call_info(
    payload: &[u8],
    protocol: ProtocolVersion,
) -> Result<ServerMessage, CodecError> {
    const HEADER_SIZE: usize = 32;
    if payload.len() < HEADER_SIZE {
        return Err(CodecError::Truncated {
            message_id: wire_id::CALL_INFO_DYNAMIC,
            needed: HEADER_SIZE,
            actual: payload.len(),
        });
    }
    let header: WireCallInfoDynamicHeader =
        decode(wire_id::CALL_INFO_DYNAMIC, &payload[..HEADER_SIZE])?;
    let fields = decode_dynamic_texts(
        wire_id::CALL_INFO_DYNAMIC,
        payload,
        HEADER_SIZE,
        protocol.dynamic_call_info_layout().string_count(),
    )?;
    let (
        calling_number,
        called_number,
        original_called_number,
        last_redirecting_number,
        calling_name,
        called_name,
        original_called_name,
        last_redirecting_name,
    ) = if matches!(
        protocol.dynamic_call_info_layout(),
        DynamicCallInfoLayout::Fields13 | DynamicCallInfoLayout::Fields15
    ) {
        (
            &fields[0],
            &fields[2],
            &fields[3],
            &fields[4],
            &fields[9],
            &fields[10],
            &fields[11],
            &fields[12],
        )
    } else {
        (
            &fields[0],
            &fields[1],
            &fields[2],
            &fields[3],
            &fields[8],
            &fields[9],
            &fields[10],
            &fields[11],
        )
    };
    Ok(ServerMessage::CallInfo {
        info: CallInfo {
            direction: if header.call_type == 1 {
                crate::types::CallDirection::Inbound
            } else {
                crate::types::CallDirection::Outbound
            },
            calling_name: calling_name.clone(),
            calling_number: calling_number.clone(),
            called_name: called_name.clone(),
            called_number: called_number.clone(),
            original_called_name: original_called_name.clone(),
            original_called_number: original_called_number.clone(),
            last_redirecting_name: last_redirecting_name.clone(),
            last_redirecting_number: last_redirecting_number.clone(),
            original_redirect_reason: header.original_redirect_reason,
            last_redirect_reason: header.last_redirect_reason,
            party_restrictions: header.party_restrictions,
        },
        line_instance: header.line_instance,
        call_reference: header.call_reference,
    })
}

pub(super) fn decode_port(
    value: u32,
    message_id: u32,
    field: &'static str,
) -> Result<u16, CodecError> {
    u16::try_from(value).map_err(|_| CodecError::InvalidValue {
        message_id,
        field,
        value: u64::from(value),
    })
}

pub(super) fn decode_server_endpoints(
    message_id: u32,
    names: [WireFixedText<48>; MAX_SIGNALING_SERVERS],
    ports: [u32; MAX_SIGNALING_SERVERS],
    addresses: Vec<IpAddr>,
) -> Result<Vec<SignalingServerEndpoint>, CodecError> {
    let mut servers = Vec::with_capacity(MAX_SIGNALING_SERVERS);
    for ((name, port), address) in names.into_iter().zip(ports).zip(addresses) {
        let name = name.text()?;
        let empty = name.is_empty() && port == 0 && address.is_unspecified();
        if empty {
            continue;
        }
        if port == 0 || address.is_unspecified() || address.is_multicast() {
            return Err(CodecError::InvalidValue {
                message_id,
                field: "server endpoint",
                value: u64::from(port),
            });
        }
        servers.push(SignalingServerEndpoint {
            name,
            address,
            port: NonZeroU16::new(decode_port(port, message_id, "server port")?).ok_or(
                CodecError::InvalidValue {
                    message_id,
                    field: "server port",
                    value: 0,
                },
            )?,
        });
    }
    Ok(servers)
}

pub(super) fn decode_bool_word(
    value: u32,
    message_id: u32,
    field: &'static str,
) -> Result<bool, CodecError> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        value => Err(CodecError::InvalidValue {
            message_id,
            field,
            value: u64::from(value),
        }),
    }
}
