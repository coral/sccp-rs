//! Transport-neutral station admission.
//!
//! Listener owners establish the underlying connection and its security
//! policy, then hand the ready byte stream to the protocol server. Session
//! framing and lifecycle remain independent of the transport implementation.

use std::fmt;
use std::net::SocketAddr;
use std::num::NonZeroU64;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::io::ReadBuf;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

use super::ServerError;
use super::qos::StationSocketQos;
use crate::message::catalog::{ObservationSanitization, observation_sanitization};
use crate::types::{SignalingQos, StationTransport};

/// Bidirectional asynchronous byte stream accepted by a station session.
///
/// The protocol server owns the stream after admission and applies identical
/// framing, backpressure, registration, and shutdown behavior regardless of
/// the underlying transport. A transport adapter may implement this trait with
/// a plain socket, a decrypted secure stream, or an in-memory test stream.
pub trait StationIo: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T> StationIo for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

pub(super) type BoxedStationIo = Box<dyn StationIo>;

/// Identifies one admitted connection for the lifetime of a [`super::Server`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ObservationConnectionId(NonZeroU64);

impl ObservationConnectionId {
    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Direction of one complete decrypted signaling frame.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SignalingDirection {
    StationToServer,
    ServerToStation,
}

/// Describes whether the observed bytes were preserved or sanitized.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SignalingFidelity {
    Exact,
    SecretsRedacted,
    PayloadSuppressed,
    IncompletePayloadSuppressed,
}

/// One bounded frame observed at the decrypted station transport boundary.
///
/// Complete unknown frames are retained exactly. Known credential-capable
/// payloads, media key reservoirs, and every incomplete frame are sanitized
/// before entering the observation queue. `Debug` never renders wire bytes.
#[derive(Clone)]
pub struct SignalingObservation {
    pub connection_id: ObservationConnectionId,
    pub peer: SocketAddr,
    pub local: SocketAddr,
    pub transport: StationTransport,
    pub direction: SignalingDirection,
    pub protocol_header: Option<u32>,
    pub message_id: Option<u32>,
    pub fidelity: SignalingFidelity,
    pub bytes: Vec<u8>,
}

impl fmt::Debug for SignalingObservation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SignalingObservation")
            .field("connection_id", &self.connection_id)
            .field("peer", &self.peer)
            .field("local", &self.local)
            .field("transport", &self.transport)
            .field("direction", &self.direction)
            .field("protocol_header", &self.protocol_header)
            .field("message_id", &self.message_id)
            .field("fidelity", &self.fidelity)
            .field("byte_count", &self.bytes.len())
            .finish()
    }
}

/// One item from the server's bounded, nonblocking observation stream.
///
/// `observation_id` is unique and monotonic but does not define ordering across
/// concurrent connections. `dropped_observations` is a batched loss counter
/// carried by the next item admitted after queue saturation.
#[derive(Clone, Debug)]
pub struct ServerObservation {
    pub observation_id: u64,
    pub observed_at_unix_ms: u64,
    pub dropped_observations: u64,
    pub kind: ServerObservationKind,
}

/// Why an admitted station connection stopped.
///
/// This classification deliberately stays independent of error text so
/// telemetry consumers can aggregate connection outcomes without retaining
/// transport- or protocol-specific details.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum StationDisconnectReason {
    /// The station closed its side of the transport cleanly.
    PeerClosure,
    /// Reading from or writing to the station transport failed.
    IoFailure,
    /// The station sent no valid traffic before its keepalive deadline.
    KeepaliveExpiry,
    /// The server deliberately retired the session.
    ServerRetirement,
    /// The station explicitly requested that its session end.
    StationRequest,
    /// The server rejected the station during registration.
    RegistrationRejected,
    /// Malformed framing or another protocol error ended the session.
    ProtocolFailure,
    /// A non-I/O server failure ended the session.
    ServerFailure,
}

/// Connection lifecycle and signaling records emitted by the server.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum ServerObservationKind {
    Connected {
        connection_id: ObservationConnectionId,
        peer: SocketAddr,
        local: SocketAddr,
        transport: StationTransport,
    },
    Signaling(SignalingObservation),
    Identified {
        connection_id: ObservationConnectionId,
        device_id: crate::types::DeviceId,
        session_generation: crate::types::SessionGeneration,
    },
    Disconnected {
        connection_id: ObservationConnectionId,
        reason: StationDisconnectReason,
    },
}

#[derive(Clone, Default)]
pub(super) struct ObservationSink {
    sender: Option<mpsc::Sender<ServerObservation>>,
    next_observation_id: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
}

impl ObservationSink {
    pub(super) fn new(sender: mpsc::Sender<ServerObservation>) -> Self {
        Self {
            sender: Some(sender),
            next_observation_id: Arc::new(AtomicU64::new(1)),
            dropped: Arc::new(AtomicU64::new(0)),
        }
    }

    pub(super) fn observe(&self, kind: ServerObservationKind) {
        let Some(sender) = &self.sender else {
            return;
        };
        let Some(observation_id) = self
            .next_observation_id
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .ok()
        else {
            return;
        };
        let dropped_observations = self.dropped.swap(0, Ordering::Relaxed);
        let observation = ServerObservation {
            observation_id,
            observed_at_unix_ms: unix_time_ms(),
            dropped_observations,
            kind,
        };
        if sender.try_send(observation).is_err() {
            self.dropped
                .fetch_add(dropped_observations.saturating_add(1), Ordering::Relaxed);
        }
    }

    pub(super) fn is_active(&self) -> bool {
        self.sender.is_some()
    }
}

impl fmt::Debug for ObservationSink {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ObservationSink")
            .field(&self.sender.as_ref().map(|_| "<registered>"))
            .finish()
    }
}

pub(super) struct ObservedStationIo {
    inner: BoxedStationIo,
    sink: ObservationSink,
    connection_id: ObservationConnectionId,
    peer: SocketAddr,
    local: SocketAddr,
    transport: StationTransport,
    station_to_server: SignalingFrameBuffer,
    server_to_station: SignalingFrameBuffer,
}

impl ObservedStationIo {
    pub(super) fn new(
        inner: BoxedStationIo,
        sink: ObservationSink,
        connection_id: ObservationConnectionId,
        peer: SocketAddr,
        local: SocketAddr,
        transport: StationTransport,
    ) -> Self {
        Self {
            inner,
            sink,
            connection_id,
            peer,
            local,
            transport,
            station_to_server: SignalingFrameBuffer::new(),
            server_to_station: SignalingFrameBuffer::new(),
        }
    }

    fn record(&mut self, direction: SignalingDirection, bytes: &[u8]) {
        let frames = match direction {
            SignalingDirection::StationToServer => self.station_to_server.push(bytes),
            SignalingDirection::ServerToStation => self.server_to_station.push(bytes),
        };
        for frame in frames {
            self.sink
                .observe(ServerObservationKind::Signaling(SignalingObservation {
                    connection_id: self.connection_id,
                    peer: self.peer,
                    local: self.local,
                    transport: self.transport,
                    direction,
                    protocol_header: frame.protocol_header,
                    message_id: frame.message_id,
                    fidelity: frame.fidelity,
                    bytes: frame.bytes,
                }));
        }
    }
}

impl AsyncRead for ObservedStationIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let previous = buffer.filled().len();
        let result = Pin::new(&mut *self.inner).poll_read(context, buffer);
        if matches!(result, Poll::Ready(Ok(()))) {
            self.record(
                SignalingDirection::StationToServer,
                &buffer.filled()[previous..],
            );
        }
        result
    }
}

impl AsyncWrite for ObservedStationIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        let result = Pin::new(&mut *self.inner).poll_write(context, bytes);
        if let Poll::Ready(Ok(written)) = result {
            self.record(SignalingDirection::ServerToStation, &bytes[..written]);
        }
        result
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut *self.inner).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut *self.inner).poll_shutdown(context)
    }
}

impl Drop for ObservedStationIo {
    fn drop(&mut self) {
        for (direction, frame) in [
            (
                SignalingDirection::StationToServer,
                self.station_to_server.take_incomplete(),
            ),
            (
                SignalingDirection::ServerToStation,
                self.server_to_station.take_incomplete(),
            ),
        ] {
            if let Some(frame) = frame {
                self.sink
                    .observe(ServerObservationKind::Signaling(SignalingObservation {
                        connection_id: self.connection_id,
                        peer: self.peer,
                        local: self.local,
                        transport: self.transport,
                        direction,
                        protocol_header: frame.protocol_header,
                        message_id: frame.message_id,
                        fidelity: frame.fidelity,
                        bytes: frame.bytes,
                    }));
            }
        }
    }
}

struct ObservedFrame {
    protocol_header: Option<u32>,
    message_id: Option<u32>,
    fidelity: SignalingFidelity,
    bytes: Vec<u8>,
}

struct SignalingFrameBuffer {
    state: SignalingFrameBufferState,
}

enum SignalingFrameBufferState {
    Active(Vec<u8>),
    Disabled,
}

impl SignalingFrameBuffer {
    fn new() -> Self {
        Self {
            state: SignalingFrameBufferState::Active(Vec::new()),
        }
    }

    fn push(&mut self, bytes: &[u8]) -> Vec<ObservedFrame> {
        let SignalingFrameBufferState::Active(buffer) = &mut self.state else {
            return Vec::new();
        };
        if !append_signaling_bytes(buffer, bytes) {
            self.state = SignalingFrameBufferState::Disabled;
            return Vec::new();
        }
        let mut frames = Vec::new();
        let mut disable = false;
        loop {
            if buffer.len() < 12 {
                break;
            }
            let wire_length = u32::from_le_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]);
            let Some(total_bytes) = usize::try_from(wire_length)
                .ok()
                .and_then(|length| length.checked_add(8))
            else {
                frames.push(sanitize_frame(std::mem::take(buffer), true));
                disable = true;
                break;
            };
            if wire_length < 4 || total_bytes > crate::message::wire::MAX_FRAME_SIZE {
                frames.push(sanitize_frame(std::mem::take(buffer), true));
                disable = true;
                break;
            }
            if buffer.len() < total_bytes {
                break;
            }
            let bytes = buffer[..total_bytes].to_vec();
            let remaining = buffer[total_bytes..].to_vec();
            buffer.fill(0);
            *buffer = remaining;
            frames.push(sanitize_frame(bytes, false));
        }
        if disable {
            self.state = SignalingFrameBufferState::Disabled;
        }
        frames
    }

    fn take_incomplete(&mut self) -> Option<ObservedFrame> {
        let state = std::mem::replace(&mut self.state, SignalingFrameBufferState::Disabled);
        match state {
            SignalingFrameBufferState::Active(bytes) if !bytes.is_empty() => {
                Some(sanitize_frame(bytes, true))
            }
            SignalingFrameBufferState::Active(_) | SignalingFrameBufferState::Disabled => None,
        }
    }
}

fn append_signaling_bytes(buffer: &mut Vec<u8>, bytes: &[u8]) -> bool {
    let Some(combined_bytes) = buffer.len().checked_add(bytes.len()) else {
        buffer.fill(0);
        return false;
    };
    let mut combined = Vec::new();
    if combined.try_reserve_exact(combined_bytes).is_err() {
        buffer.fill(0);
        return false;
    }
    combined.extend_from_slice(buffer);
    combined.extend_from_slice(bytes);
    buffer.fill(0);
    *buffer = combined;
    true
}

impl Drop for SignalingFrameBuffer {
    fn drop(&mut self) {
        if let SignalingFrameBufferState::Active(bytes) = &mut self.state {
            bytes.fill(0);
        }
    }
}

fn sanitize_frame(mut bytes: Vec<u8>, incomplete: bool) -> ObservedFrame {
    let protocol_header = read_u32(&bytes, 4);
    let message_id = read_u32(&bytes, 8);
    let fidelity = if incomplete {
        bytes.fill(0);
        SignalingFidelity::IncompletePayloadSuppressed
    } else {
        match observation_sanitization(message_id, protocol_header) {
            ObservationSanitization::Preserve => SignalingFidelity::Exact,
            ObservationSanitization::Redact { start, end } => redact_range(&mut bytes, start..end),
            ObservationSanitization::SuppressPayload => suppress_payload(&mut bytes),
        }
    };
    ObservedFrame {
        protocol_header,
        message_id,
        fidelity,
        bytes,
    }
}

fn suppress_payload(bytes: &mut [u8]) -> SignalingFidelity {
    if let Some(payload) = bytes.get_mut(12..) {
        payload.fill(0);
    }
    SignalingFidelity::PayloadSuppressed
}

fn redact_range(bytes: &mut [u8], range: std::ops::Range<usize>) -> SignalingFidelity {
    if range.start >= bytes.len() {
        bytes.fill(0);
        return SignalingFidelity::PayloadSuppressed;
    }
    let end = range.end.min(bytes.len());
    bytes[range.start..end].fill(0);
    SignalingFidelity::SecretsRedacted
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let value = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

pub(super) struct AcceptedStation {
    pub stream: BoxedStationIo,
    pub peer: SocketAddr,
    pub local: SocketAddr,
    pub transport: StationTransport,
    pub socket_qos: Option<Box<dyn StationSocketQos>>,
}

impl fmt::Debug for AcceptedStation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AcceptedStation")
            .field("stream", &"<station I/O>")
            .field("peer", &self.peer)
            .field("local", &self.local)
            .field("transport", &self.transport)
            .field(
                "socket_qos",
                &self.socket_qos.as_ref().map(|_| "<socket QoS control>"),
            )
            .finish()
    }
}

#[derive(Clone, Debug)]
/// Cloneable admission endpoint returned by [`super::Server::with_ingress`].
///
/// A listener owner performs transport-specific setup first, then submits the
/// ready byte stream with its actual peer address, accepted local address, and
/// transport classification. Clones share one bounded queue, so awaiting
/// [`Self::accept`] propagates server backpressure instead of creating
/// unbounded session work.
pub struct ServerIngress {
    sender: mpsc::Sender<AcceptedStation>,
    signaling_qos: SignalingQos,
}

impl ServerIngress {
    pub(super) fn channel(
        capacity: usize,
        signaling_qos: SignalingQos,
    ) -> (Self, mpsc::Receiver<AcceptedStation>) {
        let (sender, receiver) = mpsc::channel(capacity);
        (
            Self {
                sender,
                signaling_qos,
            },
            receiver,
        )
    }

    /// Transfer ownership of an accepted stream to the server run loop.
    ///
    /// `peer` identifies the remote station for events and address policy;
    /// `local` is the concrete local endpoint used for server-list responses.
    /// `transport` must describe the already-established stream because it is
    /// checked against the device definition during registration. For secure
    /// admission, complete the handshake and any certificate policy before
    /// calling this method.
    ///
    /// The method waits for capacity in the ingress queue. It returns
    /// [`ServerError::Stopped`] without starting a session if the run loop has
    /// ended.
    pub async fn accept<S>(
        &self,
        stream: S,
        peer: SocketAddr,
        local: SocketAddr,
        transport: StationTransport,
    ) -> Result<(), ServerError>
    where
        S: StationIo + 'static,
    {
        self.admit(Box::new(stream), peer, local, transport, None)
            .await
    }

    /// Admit a stream while retaining control of its underlying TCP markings.
    ///
    /// The server reapplies the selected station's signaling policy after the
    /// registration message identifies it. Marking failures are logged while
    /// registration and subsequent protocol traffic continue normally.
    pub async fn accept_with_socket_qos<S, Q>(
        &self,
        stream: S,
        peer: SocketAddr,
        local: SocketAddr,
        transport: StationTransport,
        socket_qos: Q,
    ) -> Result<(), ServerError>
    where
        S: StationIo + 'static,
        Q: StationSocketQos + 'static,
    {
        super::report_socket_qos(None, peer, socket_qos.apply(self.signaling_qos));
        self.admit(
            Box::new(stream),
            peer,
            local,
            transport,
            Some(Box::new(socket_qos)),
        )
        .await
    }

    async fn admit(
        &self,
        stream: BoxedStationIo,
        peer: SocketAddr,
        local: SocketAddr,
        transport: StationTransport,
        socket_qos: Option<Box<dyn StationSocketQos>>,
    ) -> Result<(), ServerError> {
        self.sender
            .send(AcceptedStation {
                stream,
                peer,
                local,
                transport,
                socket_qos,
            })
            .await
            .map_err(|_| ServerError::Stopped)
    }
}

#[cfg(test)]
mod observation_tests {
    use super::*;
    use crate::message::catalog::MessageId;
    use crate::message::values::ProtocolVersion;

    fn frame(protocol: u32, message_id: u32, payload_bytes: usize) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(payload_bytes + 12);
        bytes.extend_from_slice(&u32::try_from(payload_bytes + 4).unwrap().to_le_bytes());
        bytes.extend_from_slice(&protocol.to_le_bytes());
        bytes.extend_from_slice(&message_id.to_le_bytes());
        bytes.extend(std::iter::repeat_n(0x5a, payload_bytes));
        bytes
    }

    #[test]
    fn frame_buffer_handles_every_transport_boundary() {
        let first = frame(
            ProtocolVersion::V22.wire(),
            MessageId::KeepAlive.wire_value(),
            0,
        );
        let second = frame(
            ProtocolVersion::V22.wire(),
            MessageId::Register.wire_value(),
            16,
        );
        let mut stream = first.clone();
        stream.extend_from_slice(&second);
        for boundary in 0..=stream.len() {
            let mut buffer = SignalingFrameBuffer::new();
            let mut observed = buffer.push(&stream[..boundary]);
            observed.extend(buffer.push(&stream[boundary..]));
            assert_eq!(observed.len(), 2, "boundary {boundary}");
            assert_eq!(observed[0].bytes, first, "boundary {boundary}");
            assert_eq!(observed[1].bytes, second, "boundary {boundary}");
        }
    }

    #[test]
    fn unknown_frames_remain_exact_and_debug_is_metadata_only() {
        let bytes = frame(ProtocolVersion::V22.wire(), 0xfeed_beef, 16);
        let observed = sanitize_frame(bytes.clone(), false);
        assert_eq!(observed.fidelity, SignalingFidelity::Exact);
        assert_eq!(observed.bytes, bytes);
        let observation = SignalingObservation {
            connection_id: ObservationConnectionId::new(1).unwrap(),
            peer: "192.0.2.10:2000".parse().unwrap(),
            local: "192.0.2.1:2000".parse().unwrap(),
            transport: StationTransport::Secure,
            direction: SignalingDirection::StationToServer,
            protocol_header: observed.protocol_header,
            message_id: observed.message_id,
            fidelity: observed.fidelity,
            bytes: observed.bytes,
        };
        let debug = format!("{observation:?}");
        assert!(debug.contains("byte_count: 28"));
        assert!(!debug.contains("90, 90"));
    }

    #[test]
    fn every_media_secret_layout_redacts_only_its_fixed_reservoirs() {
        for (protocol, message_id, range) in [
            (
                ProtocolVersion::V3.wire(),
                MessageId::OpenReceiveChannel.wire_value(),
                48..80,
            ),
            (
                ProtocolVersion::V22.wire(),
                MessageId::OpenReceiveChannel.wire_value(),
                48..80,
            ),
            (
                ProtocolVersion::V16.wire(),
                MessageId::StartMediaTransmission.wire_value(),
                64..96,
            ),
            (
                ProtocolVersion::V17.wire(),
                MessageId::StartMediaTransmission.wire_value(),
                80..112,
            ),
            (
                ProtocolVersion::V3.wire(),
                MessageId::OpenMultimediaChannel.wire_value(),
                128..160,
            ),
            (
                ProtocolVersion::V22.wire(),
                MessageId::OpenMultimediaChannel.wire_value(),
                128..160,
            ),
            (
                ProtocolVersion::V16.wire(),
                MessageId::StartMultimediaTransmission.wire_value(),
                132..164,
            ),
            (
                ProtocolVersion::V17.wire(),
                MessageId::StartMultimediaTransmission.wire_value(),
                148..180,
            ),
        ] {
            let observed = sanitize_frame(frame(protocol, message_id, 192), false);
            assert_eq!(observed.fidelity, SignalingFidelity::SecretsRedacted);
            assert!(observed.bytes[range.clone()].iter().all(|byte| *byte == 0));
            assert_eq!(observed.bytes[range.start - 1], 0x5a);
            assert_eq!(observed.bytes[range.end], 0x5a);
        }
    }

    #[test]
    fn fragmented_secret_frame_is_redacted_after_every_transport_boundary() {
        let bytes = frame(
            ProtocolVersion::V22.wire(),
            MessageId::OpenReceiveChannel.wire_value(),
            192,
        );
        for boundary in 0..=bytes.len() {
            let mut buffer = SignalingFrameBuffer::new();
            let mut observed = buffer.push(&bytes[..boundary]);
            observed.extend(buffer.push(&bytes[boundary..]));
            assert_eq!(observed.len(), 1, "boundary {boundary}");
            assert_eq!(
                observed[0].fidelity,
                SignalingFidelity::SecretsRedacted,
                "boundary {boundary}"
            );
            assert!(
                observed[0].bytes[48..80].iter().all(|byte| *byte == 0),
                "boundary {boundary}"
            );
        }
    }

    #[test]
    fn incomplete_secret_frame_suppresses_all_available_bytes() {
        let observed = sanitize_frame(
            frame(
                ProtocolVersion::V22.wire(),
                MessageId::OpenReceiveChannel.wire_value(),
                8,
            ),
            true,
        );
        assert_eq!(
            observed.fidelity,
            SignalingFidelity::IncompletePayloadSuppressed
        );
        assert!(observed.bytes.iter().all(|byte| *byte == 0));
        assert_eq!(observed.protocol_header, Some(22));
        assert_eq!(
            observed.message_id,
            Some(MessageId::OpenReceiveChannel.wire_value())
        );
    }

    #[test]
    fn malformed_framing_suppresses_the_buffer_and_disables_observation() {
        let mut malformed = vec![0x5a; 32];
        malformed[..4].copy_from_slice(&u32::MAX.to_le_bytes());
        let mut buffer = SignalingFrameBuffer::new();
        let observed = buffer.push(&malformed);
        assert_eq!(observed.len(), 1);
        assert_eq!(
            observed[0].fidelity,
            SignalingFidelity::IncompletePayloadSuppressed
        );
        assert!(observed[0].bytes.iter().all(|byte| *byte == 0));

        let valid = frame(
            ProtocolVersion::V22.wire(),
            MessageId::KeepAlive.wire_value(),
            0,
        );
        assert!(buffer.push(&valid).is_empty());
    }

    #[test]
    fn station_service_submissions_never_publish_credential_capable_payloads() {
        for message_id in [
            MessageId::DeviceToUserData,
            MessageId::DeviceToUserDataResponse,
            MessageId::DeviceToUserDataV1,
            MessageId::DeviceToUserDataResponseV1,
        ] {
            let message_id = message_id.wire_value();
            let observed =
                sanitize_frame(frame(ProtocolVersion::V22.wire(), message_id, 64), false);
            assert_eq!(observed.fidelity, SignalingFidelity::PayloadSuppressed);
            assert!(observed.bytes[12..].iter().all(|byte| *byte == 0));
            assert_eq!(observed.message_id, Some(message_id));
        }
    }

    #[test]
    fn queue_overflow_is_reported_on_the_next_delivered_observation() {
        let (sender, mut receiver) = mpsc::channel(1);
        let sink = ObservationSink::new(sender);
        let disconnected = || ServerObservationKind::Disconnected {
            connection_id: ObservationConnectionId::new(1).unwrap(),
            reason: StationDisconnectReason::PeerClosure,
        };
        sink.observe(disconnected());
        sink.observe(disconnected());
        assert_eq!(receiver.try_recv().unwrap().dropped_observations, 0);
        sink.observe(disconnected());
        assert_eq!(receiver.try_recv().unwrap().dropped_observations, 1);
    }
}
