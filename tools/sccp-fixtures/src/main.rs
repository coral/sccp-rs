//! Private, offline importer for durable SCCP wire evidence.
//!
//! The importer asks tshark for raw TCP data, then uses [`FrameDecoder`] so
//! packet fragmentation, coalescing, and retransmission do not affect the
//! per-direction frame ordinal. `inventory` performs direction-specific TCP
//! sequence reassembly and emits chronological NDJSON records with packet
//! frame/time coordinates. `SCCP_TSHARK` overrides binary discovery; the
//! standard macOS Wireshark application path is used before falling back to
//! `tshark` on `PATH`.
//!
//! Always pass `--pcap-sha256` when creating a manifest fixture. `inspect`
//! reports IDs, lengths, hashes, and typed decode success without formatting a
//! message's potentially sensitive Debug representation. `extract` applies an
//! allowlist privacy policy: approved network or station fields are rewritten
//! only through public typed messages, while registration, dialed digits,
//! caller data, credentials, XML, and unknown layouts fail closed. The SCCP
//! wildcard `0.0.0.0:0` is protocol state, not a private endpoint, and is kept
//! byte-exact.
//!
//! Raw PCAPs remain under the ignored `scratch/` tree. The strict TOML v2
//! manifest stores their hashes, extraction coordinates, transformations, and
//! handset observations separately from wire validity.
//!
//! # External physical-validation boundary
//!
//! The final outbound no-`PrepareAnswer` entry in `scratch/run.md` records only
//! deployment and capture startup for module
//! `a36ca37e99b482a5dff83999aa021744e722e945196341eec68650c9d777631a`;
//! it does not record a completed call or recovered PCAP. Physical validation
//! therefore remains under HW-003. The deterministic software contracts are
//! covered by
//! `runtime::controller::tests::early_media_modes_are_explicit_and_answer_reuses_the_stream`
//! and
//! `server::tests::outbound_media_writes_receive_then_transmit_without_an_ack_boundary`.

use std::collections::{BTreeMap, VecDeque};
use std::env;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use sccp_protocol::{
    ClientMessage, Frame, FrameDecoder, MediaEndpoint, MessageId, ProtocolVersion, ServerMessage,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const DOCUMENTATION_ADDRESS: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
const DOCUMENTATION_RTP_PORT: u16 = 4_000;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Direction {
    DeviceToServer,
    ServerToDevice,
}

#[derive(Clone, Debug, Serialize)]
struct InventoryRecord {
    sequence: usize,
    frame_number: u64,
    timestamp_epoch: String,
    direction: Direction,
    direction_ordinal: usize,
    message_id: u32,
    message_name: &'static str,
    header_protocol: u32,
    bytes: usize,
    sha256: String,
    decode: String,
}

#[derive(Clone, Debug)]
struct CapturedFrame {
    record: InventoryRecord,
    frame: Frame,
    bytes: Vec<u8>,
}

#[derive(Clone, Debug)]
struct PacketSegment {
    frame_number: u64,
    timestamp_epoch: String,
    direction: Direction,
    sequence: u32,
    bytes: Vec<u8>,
}

#[derive(Clone, Debug)]
struct ByteOrigin {
    frame_number: u64,
    timestamp_epoch: String,
}

#[derive(Default)]
struct ReassemblyState {
    next_sequence: Option<u64>,
    pending: BTreeMap<u64, PacketSegment>,
    observed_bytes: BTreeMap<u64, u8>,
    decoder: FrameDecoder,
    origins: VecDeque<ByteOrigin>,
    ordinal: usize,
}

impl Direction {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "device_to_server" => Ok(Self::DeviceToServer),
            "server_to_device" => Ok(Self::ServerToDevice),
            _ => Err(format!("invalid direction {value:?}")),
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::DeviceToServer => "device_to_server",
            Self::ServerToDevice => "server_to_device",
        }
    }
}

#[derive(Debug)]
struct FollowStream {
    nodes: [Node; 2],
}

#[derive(Debug, Default)]
struct Node {
    endpoint: String,
    bytes: Vec<u8>,
}

#[derive(Debug)]
struct Options {
    pcap: PathBuf,
    stream: u32,
    protocol: ProtocolVersion,
    server_port: u16,
    direction: Option<Direction>,
    ordinal: Option<usize>,
    output: Option<PathBuf>,
    sanitize_network: bool,
    sanitize_station: bool,
    expected_pcap_sha256: Option<String>,
}

fn main() -> ExitCode {
    match run(env::args_os().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("sccp-fixtures: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(arguments: Vec<OsString>) -> Result<(), String> {
    let (command, options) = parse_options(arguments)?;
    let pcap = fs::read(&options.pcap)
        .map_err(|error| format!("could not read {}: {error}", options.pcap.display()))?;
    let pcap_sha256 = verify_pcap_sha256(&pcap, options.expected_pcap_sha256.as_deref())?;
    eprintln!("source_pcap_sha256={pcap_sha256}");
    if command == "inventory" {
        return inventory(&options);
    }
    let tshark = tshark_command();
    let output = Command::new(&tshark)
        .args([
            OsString::from("-r"),
            options.pcap.as_os_str().to_owned(),
            OsString::from("-q"),
            OsString::from("-z"),
            OsString::from(format!("follow,tcp,raw,{}", options.stream)),
        ])
        .output()
        .map_err(|error| format!("could not execute {}: {error}", tshark.display()))?;
    if !output.status.success() {
        return Err(format!(
            "tshark failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let followed = String::from_utf8(output.stdout)
        .map_err(|_| "tshark follow output was not UTF-8".to_owned())?;
    let stream = parse_follow_stream(&followed)?;
    let server_node = stream
        .nodes
        .iter()
        .position(|node| endpoint_port(&node.endpoint) == Some(options.server_port))
        .ok_or_else(|| {
            format!(
                "neither followed endpoint uses port {}",
                options.server_port
            )
        })?;

    match command.as_str() {
        "inspect" => inspect(&stream, server_node, options.protocol),
        "extract" => extract(&stream, server_node, &options),
        _ => Err(format!("unknown command {command:?}")),
    }
}

fn verify_pcap_sha256(pcap: &[u8], expected: Option<&str>) -> Result<String, String> {
    let actual = sha256(pcap);
    if let Some(expected) = expected
        && expected != actual
    {
        return Err(format!(
            "PCAP SHA-256 mismatch: expected {expected}, found {actual}"
        ));
    }
    Ok(actual)
}

fn tshark_command() -> PathBuf {
    env::var_os("SCCP_TSHARK").map_or_else(
        || {
            let macos = PathBuf::from("/Applications/Wireshark.app/Contents/MacOS/tshark");
            if macos.is_file() {
                macos
            } else {
                PathBuf::from("tshark")
            }
        },
        PathBuf::from,
    )
}

fn parse_options(arguments: Vec<OsString>) -> Result<(String, Options), String> {
    let mut arguments = arguments.into_iter();
    let command = arguments
        .next()
        .and_then(|value| value.into_string().ok())
        .ok_or_else(usage)?;
    let mut pcap = None;
    let mut stream = None;
    let mut protocol = None;
    let mut server_port = 2_000;
    let mut direction = None;
    let mut ordinal = None;
    let mut output = None;
    let mut sanitize_network = false;
    let mut sanitize_station = false;
    let mut expected_pcap_sha256 = None;
    while let Some(flag) = arguments.next() {
        let flag = flag
            .into_string()
            .map_err(|_| "arguments must be valid UTF-8".to_owned())?;
        let mut value = || {
            arguments
                .next()
                .ok_or_else(|| format!("{flag} requires a value"))
        };
        match flag.as_str() {
            "--pcap" => pcap = Some(PathBuf::from(value()?)),
            "--stream" => stream = Some(parse_number(value()?, "TCP stream")?),
            "--protocol" => {
                let wire = parse_number(value()?, "protocol")?;
                protocol = Some(
                    ProtocolVersion::negotiate(wire)
                        .map_err(|error| format!("invalid protocol: {error}"))?,
                );
            }
            "--server-port" => server_port = parse_number(value()?, "server port")?,
            "--direction" => {
                direction = Some(Direction::parse(
                    &value()?
                        .into_string()
                        .map_err(|_| "direction must be UTF-8".to_owned())?,
                )?);
            }
            "--ordinal" => ordinal = Some(parse_number(value()?, "ordinal")?),
            "--output" => output = Some(PathBuf::from(value()?)),
            "--sanitize-network" => sanitize_network = true,
            "--sanitize-station" => sanitize_station = true,
            "--pcap-sha256" => {
                expected_pcap_sha256 = Some(
                    value()?
                        .into_string()
                        .map_err(|_| "PCAP SHA-256 must be UTF-8".to_owned())?,
                );
            }
            "--help" | "-h" => return Err(usage()),
            _ => return Err(format!("unknown option {flag:?}\n{}", usage())),
        }
    }
    if command != "inspect" && command != "extract" && command != "inventory" {
        return Err(usage());
    }
    if command == "extract" && (direction.is_none() || ordinal.is_none() || output.is_none()) {
        return Err(format!(
            "extract requires --direction, --ordinal, and --output\n{}",
            usage()
        ));
    }
    if command == "extract" && expected_pcap_sha256.is_none() {
        return Err(format!("extract requires --pcap-sha256\n{}", usage()));
    }
    Ok((
        command,
        Options {
            pcap: pcap.ok_or_else(usage)?,
            stream: stream.ok_or_else(usage)?,
            protocol: protocol.ok_or_else(usage)?,
            server_port,
            direction,
            ordinal,
            output,
            sanitize_network,
            sanitize_station,
            expected_pcap_sha256,
        },
    ))
}

fn usage() -> String {
    "usage: sccp-fixtures inspect --pcap FILE --stream N --protocol N [--pcap-sha256 HEX] [--server-port 2000]\n       sccp-fixtures inventory --pcap FILE --stream N --protocol N [--pcap-sha256 HEX] [--server-port 2000]\n       sccp-fixtures extract --pcap FILE --pcap-sha256 HEX --stream N --protocol N --direction device_to_server|server_to_device --ordinal N --output FILE [--sanitize-network] [--sanitize-station]".to_owned()
}

fn parse_number<T>(value: OsString, name: &str) -> Result<T, String>
where
    T: std::str::FromStr,
{
    value
        .into_string()
        .map_err(|_| format!("{name} must be UTF-8"))?
        .parse()
        .map_err(|_| format!("invalid {name}"))
}

fn parse_follow_stream(source: &str) -> Result<FollowStream, String> {
    let mut nodes = [Node::default(), Node::default()];
    let mut saw_node = [false; 2];
    for line in source.lines() {
        if let Some(endpoint) = line.strip_prefix("Node 0: ") {
            nodes[0].endpoint = endpoint.trim().to_owned();
            saw_node[0] = true;
            continue;
        }
        if let Some(endpoint) = line.strip_prefix("Node 1: ") {
            nodes[1].endpoint = endpoint.trim().to_owned();
            saw_node[1] = true;
            continue;
        }
        let node = usize::from(line.starts_with('\t'));
        let encoded = line.trim();
        if encoded.is_empty()
            || encoded.starts_with('=')
            || encoded.starts_with("Follow:")
            || encoded.starts_with("Filter:")
        {
            continue;
        }
        if encoded.len() % 2 == 0 && encoded.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            decode_hex_into(encoded, &mut nodes[node].bytes)?;
        }
    }
    if !saw_node.into_iter().all(|seen| seen) {
        return Err("tshark did not report both followed TCP endpoints".to_owned());
    }
    Ok(FollowStream { nodes })
}

fn decode_hex_into(source: &str, output: &mut Vec<u8>) -> Result<(), String> {
    for index in (0..source.len()).step_by(2) {
        output.push(
            u8::from_str_radix(&source[index..index + 2], 16)
                .map_err(|_| "tshark emitted invalid hexadecimal data".to_owned())?,
        );
    }
    Ok(())
}

fn endpoint_port(endpoint: &str) -> Option<u16> {
    endpoint.rsplit_once(':')?.1.parse().ok()
}

fn node_direction(node: usize, server_node: usize) -> Direction {
    if node == server_node {
        Direction::ServerToDevice
    } else {
        Direction::DeviceToServer
    }
}

fn frames(node: &Node) -> Result<Vec<(Frame, Vec<u8>)>, String> {
    let mut decoder = FrameDecoder::new();
    let decoded = decoder
        .push(&node.bytes)
        .map_err(|error| format!("SCCP stream framing failed: {error}"))?;
    if decoder.buffered_len() != 0 {
        return Err(format!(
            "SCCP stream ended with {} incomplete bytes",
            decoder.buffered_len()
        ));
    }
    decoded
        .into_iter()
        .map(|frame| {
            let bytes = frame
                .encode()
                .map_err(|error| format!("could not reconstruct captured frame: {error}"))?;
            Ok((frame, bytes))
        })
        .collect()
}

fn inventory(options: &Options) -> Result<(), String> {
    for captured in capture_inventory(options)? {
        if captured.record.message_id != captured.frame.message_id
            || captured.record.sha256 != sha256(&captured.bytes)
        {
            return Err("internal inventory record disagrees with reconstructed frame".to_owned());
        }
        println!(
            "{}",
            serde_json::to_string(&captured.record)
                .map_err(|error| format!("could not serialize inventory: {error}"))?
        );
    }
    Ok(())
}

fn capture_inventory(options: &Options) -> Result<Vec<CapturedFrame>, String> {
    let tshark = tshark_command();
    let filter = format!("tcp.stream == {} && tcp.len > 0", options.stream);
    let output = Command::new(&tshark)
        .args([
            OsString::from("-r"),
            options.pcap.as_os_str().to_owned(),
            OsString::from("-Y"),
            OsString::from(filter),
            OsString::from("-T"),
            OsString::from("fields"),
            OsString::from("-E"),
            OsString::from("separator=/t"),
            OsString::from("-E"),
            OsString::from("occurrence=f"),
            OsString::from("-e"),
            OsString::from("frame.number"),
            OsString::from("-e"),
            OsString::from("frame.time_epoch"),
            OsString::from("-e"),
            OsString::from("tcp.stream"),
            OsString::from("-e"),
            OsString::from("tcp.srcport"),
            OsString::from("-e"),
            OsString::from("tcp.dstport"),
            OsString::from("-e"),
            OsString::from("tcp.seq_raw"),
            OsString::from("-e"),
            OsString::from("tcp.len"),
            OsString::from("-e"),
            OsString::from("tcp.payload"),
        ])
        .output()
        .map_err(|error| format!("could not execute {}: {error}", tshark.display()))?;
    if !output.status.success() {
        return Err(format!(
            "tshark inventory failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let source = String::from_utf8(output.stdout)
        .map_err(|_| "tshark inventory output was not UTF-8".to_owned())?;
    let mut segments = Vec::new();
    for line in source.lines().filter(|line| !line.is_empty()) {
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.len() != 8 {
            return Err(format!("unexpected tshark inventory row: {line:?}"));
        }
        let stream = fields[2]
            .parse::<u32>()
            .map_err(|_| "invalid tshark TCP stream".to_owned())?;
        if stream != options.stream {
            return Err("tshark returned a different TCP stream".to_owned());
        }
        let source_port = fields[3]
            .parse::<u16>()
            .map_err(|_| "invalid tshark source port".to_owned())?;
        let destination_port = fields[4]
            .parse::<u16>()
            .map_err(|_| "invalid tshark destination port".to_owned())?;
        let direction = match (
            source_port == options.server_port,
            destination_port == options.server_port,
        ) {
            (true, false) => Direction::ServerToDevice,
            (false, true) => Direction::DeviceToServer,
            _ => return Err("inventory packet has ambiguous SCCP direction".to_owned()),
        };
        let mut bytes = Vec::new();
        decode_hex_into(&fields[7].replace(':', ""), &mut bytes)?;
        let tcp_len = fields[6]
            .parse::<usize>()
            .map_err(|_| "invalid tshark TCP length".to_owned())?;
        if bytes.len() != tcp_len {
            return Err(format!(
                "tshark TCP length {tcp_len} disagrees with {} payload bytes",
                bytes.len()
            ));
        }
        parse_epoch_nanos(fields[1])?;
        segments.push(PacketSegment {
            frame_number: fields[0]
                .parse()
                .map_err(|_| "invalid tshark frame number".to_owned())?,
            timestamp_epoch: fields[1].to_owned(),
            direction,
            sequence: fields[5]
                .parse()
                .map_err(|_| "invalid tshark TCP sequence".to_owned())?,
            bytes,
        });
    }
    chronological_frames(segments, options.protocol)
}

fn chronological_frames(
    segments: Vec<PacketSegment>,
    protocol: ProtocolVersion,
) -> Result<Vec<CapturedFrame>, String> {
    let mut states = [ReassemblyState::default(), ReassemblyState::default()];
    for direction in [Direction::DeviceToServer, Direction::ServerToDevice] {
        let index = usize::from(direction == Direction::ServerToDevice);
        states[index].next_sequence = segments
            .iter()
            .filter(|segment| segment.direction == direction)
            .map(|segment| u64::from(segment.sequence))
            .min();
    }
    let mut captured = Vec::new();
    for segment in segments {
        let index = usize::from(segment.direction == Direction::ServerToDevice);
        accept_segment(&mut states[index], segment, protocol, &mut captured)?;
    }
    for state in &states {
        if !state.pending.is_empty() || state.decoder.buffered_len() != 0 {
            return Err("TCP stream ended with a gap or incomplete SCCP frame".to_owned());
        }
    }
    captured.sort_by(|left, right| {
        parse_epoch_nanos(&left.record.timestamp_epoch)
            .unwrap()
            .cmp(&parse_epoch_nanos(&right.record.timestamp_epoch).unwrap())
            .then(left.record.frame_number.cmp(&right.record.frame_number))
    });
    for (sequence, frame) in captured.iter_mut().enumerate() {
        frame.record.sequence = sequence + 1;
    }
    Ok(captured)
}

fn accept_segment(
    state: &mut ReassemblyState,
    segment: PacketSegment,
    protocol: ProtocolVersion,
    captured: &mut Vec<CapturedFrame>,
) -> Result<(), String> {
    let sequence = u64::from(segment.sequence);
    if state.next_sequence.is_none() {
        state.next_sequence = Some(sequence);
    }
    for (offset, byte) in segment.bytes.iter().copied().enumerate() {
        let position = sequence + offset as u64;
        if let Some(previous) = state.observed_bytes.insert(position, byte)
            && previous != byte
        {
            return Err(format!(
                "conflicting TCP retransmission at sequence {position}"
            ));
        }
    }
    match state.pending.entry(sequence) {
        std::collections::btree_map::Entry::Vacant(entry) => {
            entry.insert(segment);
        }
        std::collections::btree_map::Entry::Occupied(mut entry) => {
            if segment.bytes.len() > entry.get().bytes.len() {
                entry.insert(segment);
            }
        }
    }
    loop {
        let next = state.next_sequence.expect("initialized sequence");
        let Some((&sequence, _)) = state.pending.first_key_value() else {
            break;
        };
        if sequence > next {
            break;
        }
        let segment = state.pending.remove(&sequence).unwrap();
        let consumed = usize::try_from(next.saturating_sub(sequence))
            .map_err(|_| "TCP overlap is not representable".to_owned())?;
        if consumed >= segment.bytes.len() {
            continue;
        }
        let bytes = &segment.bytes[consumed..];
        append_contiguous(state, &segment, bytes, protocol, captured)?;
        state.next_sequence = Some(next + bytes.len() as u64);
    }
    Ok(())
}

fn append_contiguous(
    state: &mut ReassemblyState,
    segment: &PacketSegment,
    bytes: &[u8],
    protocol: ProtocolVersion,
    captured: &mut Vec<CapturedFrame>,
) -> Result<(), String> {
    state.origins.extend((0..bytes.len()).map(|_| ByteOrigin {
        frame_number: segment.frame_number,
        timestamp_epoch: segment.timestamp_epoch.clone(),
    }));
    for frame in state
        .decoder
        .push(bytes)
        .map_err(|error| format!("SCCP chronology framing failed: {error}"))?
    {
        let encoded = frame
            .encode()
            .map_err(|error| format!("could not reconstruct inventory frame: {error}"))?;
        let origin = state
            .origins
            .front()
            .cloned()
            .ok_or_else(|| "missing SCCP frame origin".to_owned())?;
        for _ in 0..encoded.len() {
            state
                .origins
                .pop_front()
                .ok_or_else(|| "SCCP frame exceeded origin bytes".to_owned())?;
        }
        let direction = segment.direction;
        let record = InventoryRecord {
            sequence: 0,
            frame_number: origin.frame_number,
            timestamp_epoch: origin.timestamp_epoch,
            direction,
            direction_ordinal: state.ordinal,
            message_id: frame.message_id,
            message_name: frame.message_type().name(),
            header_protocol: frame.protocol_version,
            bytes: encoded.len(),
            sha256: sha256(&encoded),
            decode: decode_summary(direction, frame.clone(), protocol),
        };
        state.ordinal += 1;
        captured.push(CapturedFrame {
            record,
            frame,
            bytes: encoded,
        });
    }
    Ok(())
}

fn parse_epoch_nanos(value: &str) -> Result<u128, String> {
    let (seconds, fraction) = value
        .split_once('.')
        .ok_or_else(|| format!("invalid epoch timestamp {value:?}"))?;
    if fraction.is_empty()
        || fraction.len() > 9
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(format!("invalid epoch timestamp {value:?}"));
    }
    let seconds = seconds
        .parse::<u128>()
        .map_err(|_| format!("invalid epoch timestamp {value:?}"))?;
    let nanos = fraction
        .parse::<u128>()
        .map_err(|_| format!("invalid epoch timestamp {value:?}"))?
        * 10_u128.pow(9 - fraction.len() as u32);
    Ok(seconds * 1_000_000_000 + nanos)
}

fn inspect(
    stream: &FollowStream,
    server_node: usize,
    protocol: ProtocolVersion,
) -> Result<(), String> {
    for (node_index, node) in stream.nodes.iter().enumerate() {
        let direction = node_direction(node_index, server_node);
        for (ordinal, (frame, bytes)) in frames(node)?.into_iter().enumerate() {
            let decode = decode_summary(direction, frame.clone(), protocol);
            println!(
                "{} ordinal={} id=0x{:04x} name={} header_protocol={} bytes={} sha256={} decode={}",
                direction.label(),
                ordinal,
                frame.message_id,
                frame.message_type().name(),
                frame.protocol_version,
                bytes.len(),
                sha256(&bytes),
                decode
            );
        }
    }
    Ok(())
}

fn decode_summary(direction: Direction, frame: Frame, protocol: ProtocolVersion) -> String {
    let result = match direction {
        Direction::DeviceToServer => {
            ClientMessage::decode_with_version(frame, protocol).map(|_| "ok:typed".to_owned())
        }
        Direction::ServerToDevice => {
            ServerMessage::decode(frame, protocol).map(|_| "ok:typed".to_owned())
        }
    };
    result.unwrap_or_else(|error| format!("error:{error}"))
}

fn extract(stream: &FollowStream, server_node: usize, options: &Options) -> Result<(), String> {
    let direction = options.direction.expect("validated extract direction");
    let node_index = if direction == Direction::ServerToDevice {
        server_node
    } else {
        1 - server_node
    };
    let ordinal = options.ordinal.expect("validated extract ordinal");
    let (frame, bytes) = frames(&stream.nodes[node_index])?
        .into_iter()
        .nth(ordinal)
        .ok_or_else(|| format!("no {} frame at ordinal {ordinal}", direction.label()))?;
    let (bytes, transformations) = sanitize(
        direction,
        frame,
        bytes,
        options.protocol,
        options.sanitize_network,
        options.sanitize_station,
    )?;
    let output = options.output.as_ref().expect("validated output path");
    write_hex(output, &bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    eprintln!("sha256={}", sha256(&bytes));
    for transformation in transformations {
        eprintln!("sanitized={transformation}");
    }
    Ok(())
}

fn sanitize(
    direction: Direction,
    frame: Frame,
    original: Vec<u8>,
    protocol: ProtocolVersion,
    sanitize_network: bool,
    sanitize_station: bool,
) -> Result<(Vec<u8>, Vec<&'static str>), String> {
    match privacy_class(frame.message_id) {
        PrivacyClass::Safe => Ok((original, Vec::new())),
        PrivacyClass::Network if network_is_wildcard(direction, &frame, protocol)? => {
            Ok((original, Vec::new()))
        }
        PrivacyClass::Network if sanitize_network => {
            let encoded = match direction {
                Direction::DeviceToServer => {
                    let mut message = ClientMessage::decode_with_version(frame, protocol)
                        .map_err(|error| format!("cannot sanitize undecodable client frame: {error}"))?;
                    sanitize_client_network(&mut message)?;
                    message.encode(protocol)
                }
                Direction::ServerToDevice => {
                    let mut message = ServerMessage::decode(frame, protocol)
                        .map_err(|error| format!("cannot sanitize undecodable server frame: {error}"))?;
                    sanitize_server_network(&mut message)?;
                    message.encode(protocol)
                }
            }
            .map_err(|error| format!("could not encode sanitized frame: {error}"))?;
            Ok((encoded, vec!["network_address", "network_port"]))
        }
        PrivacyClass::Network => Err(
            "frame contains network coordinates; pass --sanitize-network to replace them".into(),
        ),
        PrivacyClass::Station if sanitize_station => {
            let mut message = match direction {
                Direction::ServerToDevice => ServerMessage::decode(frame, protocol)
                    .map_err(|error| format!("cannot sanitize station frame: {error}"))?,
                Direction::DeviceToServer => {
                    return Err("station-text sanitizer supports server messages only".into());
                }
            };
            let ServerMessage::LineStatus {
                directory_number,
                fully_qualified_display_name,
                display_label,
                ..
            } = &mut message
            else {
                return Err("station-sensitive frame has no typed sanitizer".into());
            };
            *directory_number = "1001".to_owned();
            *fully_qualified_display_name = "1001".to_owned();
            *display_label = "1001".to_owned();
            let encoded = message
                .encode(protocol)
                .map_err(|error| format!("could not encode sanitized frame: {error}"))?;
            Ok((
                encoded,
                vec![
                    "directory_number",
                    "fully_qualified_display_name",
                    "display_label",
                ],
            ))
        }
        PrivacyClass::Station => Err(
            "frame contains station text; pass --sanitize-station to replace approved typed fields"
                .into(),
        ),
        PrivacyClass::Sensitive => Err(
            "frame may contain station identity, dialed digits, caller data, XML, or credentials; no approved typed sanitizer exists".into(),
        ),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PrivacyClass {
    Safe,
    Network,
    Station,
    Sensitive,
}

fn privacy_class(message_id: u32) -> PrivacyClass {
    match MessageId::from(message_id) {
        MessageId::KeepAlive
        | MessageId::OffHook
        | MessageId::OnHook
        | MessageId::HookFlash
        | MessageId::CapabilitiesResponse
        | MessageId::UpdateCapabilities
        | MessageId::UpdateCapabilitiesV2
        | MessageId::UpdateCapabilitiesV3
        | MessageId::HeadsetStatus
        | MessageId::MediaPathEvent
        | MessageId::KeepAliveAck
        | MessageId::StopMediaTransmission
        | MessageId::CloseReceiveChannel => PrivacyClass::Safe,
        MessageId::OpenReceiveChannelAck
        | MessageId::MediaTransmissionFailure
        | MessageId::StartMediaTransmissionAck
        | MessageId::OpenReceiveChannel
        | MessageId::StartMediaTransmission => PrivacyClass::Network,
        MessageId::LineStatusDynamic => PrivacyClass::Station,
        _ => PrivacyClass::Sensitive,
    }
}

fn network_is_wildcard(
    direction: Direction,
    frame: &Frame,
    protocol: ProtocolVersion,
) -> Result<bool, String> {
    if direction != Direction::ServerToDevice
        || frame.message_id != MessageId::OpenReceiveChannel.wire_value()
    {
        return Ok(false);
    }
    let message = ServerMessage::decode(frame.clone(), protocol)
        .map_err(|error| format!("cannot inspect network frame: {error}"))?;
    Ok(matches!(
        message,
        ServerMessage::OpenReceiveChannel {
            source_address,
            source_port: 0,
            ..
        } if source_address.is_unspecified()
    ))
}

fn sanitize_client_network(message: &mut ClientMessage) -> Result<(), String> {
    match message {
        ClientMessage::OpenReceiveChannelAck { address, port, .. }
        | ClientMessage::MediaTransmissionFailure { address, port, .. } => {
            *address = DOCUMENTATION_ADDRESS;
            *port = DOCUMENTATION_RTP_PORT;
        }
        ClientMessage::StartMediaTransmissionAck(ack) => {
            ack.address = DOCUMENTATION_ADDRESS;
            ack.port = DOCUMENTATION_RTP_PORT;
        }
        _ => return Err("network-sensitive client frame has no typed sanitizer".into()),
    }
    Ok(())
}

fn sanitize_server_network(message: &mut ServerMessage) -> Result<(), String> {
    match message {
        ServerMessage::OpenReceiveChannel {
            source_address,
            source_port,
            ..
        } => {
            *source_address = DOCUMENTATION_ADDRESS;
            *source_port = DOCUMENTATION_RTP_PORT;
        }
        ServerMessage::StartMediaTransmission { endpoint, .. } => {
            *endpoint = MediaEndpoint {
                address: DOCUMENTATION_ADDRESS,
                rtp_port: DOCUMENTATION_RTP_PORT,
                rtcp_port: DOCUMENTATION_RTP_PORT + 1,
                ..*endpoint
            };
        }
        _ => return Err("network-sensitive server frame has no typed sanitizer".into()),
    }
    Ok(())
}

fn write_hex(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut output = String::new();
    for (index, byte) in bytes.iter().enumerate() {
        if index != 0 {
            output.push(if index % 16 == 0 { '\n' } else { ' ' });
        }
        write!(output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output.push('\n');
    fs::write(path, output)
}

fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[derive(Debug, Deserialize)]
    struct ReplayManifest {
        capture: Vec<ReplayCapture>,
        fixture: Vec<ReplayFixture>,
        trace: Vec<ReplayTrace>,
    }

    #[derive(Debug, Deserialize)]
    struct ReplayCapture {
        id: String,
        source_kind: String,
        source_file: Option<String>,
        source_sha256: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    struct ReplayFixture {
        name: String,
        file: String,
        capture: String,
        direction: Direction,
        message_id: u32,
        header_protocol: u32,
        decode_protocol: u32,
        sha256: String,
        extraction: Option<ReplayExtraction>,
        #[serde(default)]
        sanitization: Vec<ReplaySanitization>,
    }

    #[derive(Debug, Deserialize)]
    struct ReplayExtraction {
        tool: String,
        version: u32,
        tcp_stream: u32,
        direction_ordinal: usize,
    }

    #[derive(Debug, Deserialize)]
    struct ReplaySanitization {
        field: String,
    }

    #[derive(Debug, Deserialize)]
    struct ReplayTrace {
        capture: String,
        tcp_stream: u32,
        step: Vec<ReplayStep>,
    }

    #[derive(Debug, Deserialize)]
    struct ReplayStep {
        fixture: String,
        frame_number: u64,
        timestamp_epoch: String,
        inventory_sequence: usize,
    }

    fn arguments(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    fn frame(message_id: u32) -> Vec<u8> {
        Frame::new(22, message_id, vec![1, 2, 3, 4])
            .encode()
            .unwrap()
    }

    #[test]
    fn parses_follow_output_and_preserves_direction_chunks() {
        let first = frame(MessageId::KeepAlive.wire_value());
        let second = frame(MessageId::KeepAliveAck.wire_value());
        let input = format!(
            "===================================================================\nFollow: tcp,raw\nFilter: tcp.stream eq 3\nNode 0: 192.0.2.10:50000\nNode 1: 192.0.2.20:2000\n{}\n\t{}\n===================================================================\n",
            hex(&first),
            hex(&second)
        );
        let parsed = parse_follow_stream(&input).unwrap();
        assert_eq!(parsed.nodes[0].bytes, first);
        assert_eq!(parsed.nodes[1].bytes, second);
        assert_eq!(endpoint_port(&parsed.nodes[1].endpoint), Some(2_000));
    }

    #[test]
    fn extract_requires_a_pinned_pcap_hash_and_mismatch_fails_before_tshark() {
        let error = parse_options(arguments(&[
            "extract",
            "--pcap",
            "capture.pcap",
            "--stream",
            "0",
            "--protocol",
            "22",
            "--direction",
            "device_to_server",
            "--ordinal",
            "0",
            "--output",
            "fixture.hex",
        ]))
        .unwrap_err();
        assert!(error.contains("extract requires --pcap-sha256"));

        let error = verify_pcap_sha256(b"captured bytes", Some(&"0".repeat(64))).unwrap_err();
        assert!(error.starts_with("PCAP SHA-256 mismatch:"));
        assert!(!error.contains("captured bytes"));
    }

    #[test]
    fn privacy_policy_fails_closed() {
        assert_eq!(
            privacy_class(MessageId::UpdateCapabilities.wire_value()),
            PrivacyClass::Safe
        );
        assert_eq!(
            privacy_class(MessageId::OpenReceiveChannel.wire_value()),
            PrivacyClass::Network
        );
        assert_eq!(
            privacy_class(MessageId::LineStatusDynamic.wire_value()),
            PrivacyClass::Station
        );
        assert_eq!(
            privacy_class(MessageId::Register.wire_value()),
            PrivacyClass::Sensitive
        );
        assert_eq!(
            privacy_class(MessageId::UserToDeviceDataV1.wire_value()),
            PrivacyClass::Sensitive
        );
    }

    #[test]
    fn wildcard_open_receive_is_preserved_without_network_rewrite() {
        let message = ServerMessage::OpenReceiveChannel {
            call_reference: 1,
            passthrough_party_id: 1,
            packet_ms: 20,
            codec: sccp_protocol::Codec::Pcma,
            echo_cancellation: sccp_protocol::EchoCancellation::Off,
            telephone_event_payload: 101,
            source_address: "0.0.0.0".parse().unwrap(),
            source_port: 0,
            encryption: None,
            wire: None,
        };
        let bytes = message.encode(ProtocolVersion::V22).unwrap();
        let frame = frames(&Node {
            endpoint: String::new(),
            bytes: bytes.clone(),
        })
        .unwrap()
        .remove(0)
        .0;
        assert_eq!(
            sanitize(
                Direction::ServerToDevice,
                frame,
                bytes.clone(),
                ProtocolVersion::V22,
                false,
                false,
            )
            .unwrap(),
            (bytes, Vec::new())
        );
    }

    #[test]
    fn inspection_never_formats_typed_message_contents() {
        let bytes = ClientMessage::KeypadButton {
            button: sccp_protocol::Digit::Number(9),
            line_instance: 1,
            call_reference: 2,
            wire_layout: None,
        }
        .encode(ProtocolVersion::V22)
        .unwrap();
        let frame = frames(&Node {
            endpoint: String::new(),
            bytes,
        })
        .unwrap()
        .remove(0)
        .0;
        assert_eq!(
            decode_summary(Direction::DeviceToServer, frame, ProtocolVersion::V22),
            "ok:typed"
        );
    }

    #[test]
    fn station_sanitizer_rewrites_only_the_approved_typed_fields() {
        let bytes = ServerMessage::LineStatus {
            instance: 1,
            directory_number: "private-number".into(),
            fully_qualified_display_name: "private-display-name".into(),
            display_label: "private-label".into(),
        }
        .encode(ProtocolVersion::V22)
        .unwrap();
        let frame = frames(&Node {
            endpoint: String::new(),
            bytes,
        })
        .unwrap()
        .remove(0)
        .0;
        let (sanitized, fields) = sanitize(
            Direction::ServerToDevice,
            frame,
            Vec::new(),
            ProtocolVersion::V22,
            false,
            true,
        )
        .unwrap();
        assert_eq!(
            fields,
            [
                "directory_number",
                "fully_qualified_display_name",
                "display_label"
            ]
        );
        let frame = frames(&Node {
            endpoint: String::new(),
            bytes: sanitized,
        })
        .unwrap()
        .remove(0)
        .0;
        assert!(matches!(
            ServerMessage::decode(frame, ProtocolVersion::V22).unwrap(),
            ServerMessage::LineStatus {
                directory_number,
                fully_qualified_display_name,
                display_label,
                ..
            } if directory_number == "1001"
                && fully_qualified_display_name == "1001"
                && display_label == "1001"
        ));
    }

    #[test]
    fn fragmented_and_coalesced_follow_chunks_produce_deterministic_frames() {
        let first = Frame::new(0, MessageId::KeepAlive.wire_value(), Vec::new())
            .encode()
            .unwrap();
        let second = Frame::new(22, MessageId::OffHook.wire_value(), vec![0; 8])
            .encode()
            .unwrap();
        let mut bytes = first.clone();
        bytes.extend_from_slice(&second);
        let node = Node {
            endpoint: "192.0.2.10:50000".into(),
            bytes,
        };
        let decoded = frames(&node).unwrap();
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].1, first);
        assert_eq!(decoded[1].1, second);
    }

    #[test]
    fn bidirectional_inventory_orders_frame_origins_across_tcp_reassembly() {
        let first = frame(MessageId::KeepAlive.wire_value());
        let second = frame(MessageId::OffHook.wire_value());
        let response = frame(MessageId::KeepAliveAck.wire_value());
        let mut final_server_segment = first[5..].to_vec();
        final_server_segment.extend_from_slice(&second);
        let segments = vec![
            PacketSegment {
                frame_number: 10,
                timestamp_epoch: "1000.000000001".into(),
                direction: Direction::ServerToDevice,
                sequence: 100,
                bytes: first[..5].to_vec(),
            },
            PacketSegment {
                frame_number: 11,
                timestamp_epoch: "1000.000000002".into(),
                direction: Direction::DeviceToServer,
                sequence: 500,
                bytes: response,
            },
            PacketSegment {
                frame_number: 12,
                timestamp_epoch: "1000.000000003".into(),
                direction: Direction::ServerToDevice,
                sequence: 100,
                bytes: first[..5].to_vec(),
            },
            PacketSegment {
                frame_number: 13,
                timestamp_epoch: "1000.000000004".into(),
                direction: Direction::ServerToDevice,
                sequence: 105,
                bytes: final_server_segment,
            },
        ];
        let inventory = chronological_frames(segments, ProtocolVersion::V22).unwrap();
        assert_eq!(
            inventory
                .iter()
                .map(|frame| (
                    frame.record.sequence,
                    frame.record.frame_number,
                    frame.record.direction,
                    frame.record.direction_ordinal,
                    frame.record.message_id,
                ))
                .collect::<Vec<_>>(),
            [
                (
                    1,
                    10,
                    Direction::ServerToDevice,
                    0,
                    MessageId::KeepAlive.wire_value()
                ),
                (
                    2,
                    11,
                    Direction::DeviceToServer,
                    0,
                    MessageId::KeepAliveAck.wire_value()
                ),
                (
                    3,
                    13,
                    Direction::ServerToDevice,
                    1,
                    MessageId::OffHook.wire_value()
                ),
            ]
        );
    }

    #[test]
    fn conflicting_tcp_retransmission_fails_closed() {
        let segments = vec![
            PacketSegment {
                frame_number: 1,
                timestamp_epoch: "1000.000000001".into(),
                direction: Direction::ServerToDevice,
                sequence: 100,
                bytes: vec![1, 2, 3],
            },
            PacketSegment {
                frame_number: 2,
                timestamp_epoch: "1000.000000002".into(),
                direction: Direction::ServerToDevice,
                sequence: 100,
                bytes: vec![1, 9, 3],
            },
        ];
        assert_eq!(
            chronological_frames(segments, ProtocolVersion::V22).unwrap_err(),
            "conflicting TCP retransmission at sequence 101"
        );
    }

    #[test]
    fn external_manifest_coordinates_replay_to_the_committed_sanitized_bytes() {
        let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let fixture_root = repository.join("sccp-protocol/tests/fixtures/golden");
        let manifest: ReplayManifest =
            toml::from_str(&fs::read_to_string(fixture_root.join("manifest.toml")).unwrap())
                .unwrap();
        let captures = manifest
            .capture
            .iter()
            .map(|capture| (capture.id.as_str(), capture))
            .collect::<BTreeMap<_, _>>();
        let mut inventories = BTreeMap::<(String, u32, u32), Vec<CapturedFrame>>::new();
        let mut replayed = BTreeMap::<String, InventoryRecord>::new();

        for fixture in &manifest.fixture {
            let capture = captures[fixture.capture.as_str()];
            if capture.source_kind != "external_pcap" {
                continue;
            }
            let source_file = repository.join(capture.source_file.as_ref().unwrap());
            if !source_file.is_file() {
                // Raw captures are intentionally ignored and absent in normal CI.
                continue;
            }
            let expected_pcap_sha256 = capture.source_sha256.as_ref().unwrap();
            let pcap = fs::read(&source_file).unwrap();
            assert_eq!(
                verify_pcap_sha256(&pcap, Some(expected_pcap_sha256)).unwrap(),
                expected_pcap_sha256.as_str(),
                "{} source provenance",
                fixture.name
            );
            let extraction = fixture.extraction.as_ref().unwrap();
            assert_eq!(extraction.tool, "sccp_fixtures_tshark");
            assert_eq!(extraction.version, 1);
            let key = (
                fixture.capture.clone(),
                extraction.tcp_stream,
                fixture.decode_protocol,
            );
            if !inventories.contains_key(&key) {
                let options = Options {
                    pcap: source_file.clone(),
                    stream: extraction.tcp_stream,
                    protocol: ProtocolVersion::negotiate(fixture.decode_protocol).unwrap(),
                    server_port: 2_000,
                    direction: None,
                    ordinal: None,
                    output: None,
                    sanitize_network: false,
                    sanitize_station: false,
                    expected_pcap_sha256: Some(expected_pcap_sha256.clone()),
                };
                inventories.insert(key.clone(), capture_inventory(&options).unwrap());
            }
            let captured = inventories[&key]
                .iter()
                .find(|captured| {
                    captured.record.direction == fixture.direction
                        && captured.record.direction_ordinal == extraction.direction_ordinal
                })
                .unwrap_or_else(|| panic!("missing replay coordinate for {}", fixture.name));
            assert_eq!(
                captured.record.message_id, fixture.message_id,
                "{} ID",
                fixture.name
            );
            assert_eq!(
                captured.record.header_protocol, fixture.header_protocol,
                "{} header protocol",
                fixture.name
            );
            let sanitize_network = fixture
                .sanitization
                .iter()
                .any(|entry| entry.field.starts_with("network_"));
            let sanitize_station = fixture.sanitization.iter().any(|entry| {
                matches!(
                    entry.field.as_str(),
                    "directory_number" | "fully_qualified_display_name" | "display_label"
                )
            });
            let (sanitized, transformations) = sanitize(
                fixture.direction,
                captured.frame.clone(),
                captured.bytes.clone(),
                ProtocolVersion::negotiate(fixture.decode_protocol).unwrap(),
                sanitize_network,
                sanitize_station,
            )
            .unwrap();
            let expected_transformations = fixture
                .sanitization
                .iter()
                .map(|entry| entry.field.as_str())
                .collect::<BTreeSet<_>>();
            assert_eq!(
                transformations.into_iter().collect::<BTreeSet<_>>(),
                expected_transformations,
                "{} sanitization inventory",
                fixture.name
            );
            assert_eq!(sha256(&sanitized), fixture.sha256, "{} hash", fixture.name);
            assert_eq!(
                sanitized,
                read_hex_file(&fixture_root.join(&fixture.file)),
                "{} committed bytes",
                fixture.name
            );
            replayed.insert(fixture.name.clone(), captured.record.clone());
        }

        for trace in &manifest.trace {
            for step in &trace.step {
                let Some(record) = replayed.get(&step.fixture) else {
                    continue;
                };
                let fixture = manifest
                    .fixture
                    .iter()
                    .find(|fixture| fixture.name == step.fixture)
                    .unwrap();
                assert_eq!(fixture.capture, trace.capture);
                assert_eq!(
                    fixture.extraction.as_ref().unwrap().tcp_stream,
                    trace.tcp_stream
                );
                assert_eq!(
                    record.frame_number, step.frame_number,
                    "{} frame",
                    step.fixture
                );
                assert_eq!(
                    record.timestamp_epoch, step.timestamp_epoch,
                    "{} timestamp",
                    step.fixture
                );
                assert_eq!(
                    record.sequence, step.inventory_sequence,
                    "{} inventory order",
                    step.fixture
                );
            }
        }
    }

    fn read_hex_file(path: &Path) -> Vec<u8> {
        let source = fs::read_to_string(path).unwrap();
        let compact = source.split_ascii_whitespace().collect::<String>();
        let mut bytes = Vec::new();
        decode_hex_into(&compact, &mut bytes).unwrap();
        bytes
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}
