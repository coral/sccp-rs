//! Owns bounded diagnostics capture and delivery for the opt-in artifact.
//! The ordinary module build excludes this module and its dependency graph.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sccp_protocol::{ServerObservation, ServerObservationKind};
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::asterisk::boundary::LogLevel;
use crate::asterisk::raw;
use crate::asterisk::runtime::Shared;

use self::capture::PacketCapture;
use self::transport::{PendingBatch, PendingEvent};
use self::wire::DiagnosticType;

mod capture;
mod snapshot;
mod transport;
mod wire;

const LOG_QUEUE_ITEMS: usize = 1024;
const OBSERVATION_QUEUE_ITEMS: usize = 1024;
const REPORT_QUEUE_ITEMS: usize = 16;
const RECENT_LOG_ITEMS: usize = 128;
const MAX_LOG_MESSAGE_BYTES: usize = 8 * 1024;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

static ACTIVE_LOGGER: OnceLock<Mutex<Option<ActiveLogger>>> = OnceLock::new();
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
struct ActiveLogger {
    generation: u64,
    sender: mpsc::Sender<LogEntry>,
}

#[derive(Clone, Serialize)]
pub(super) struct LogEntry {
    observed_at_unix_ms: u64,
    level: &'static str,
    message: String,
}

impl LogEntry {
    fn new(level: LogLevel, message: &str) -> Self {
        Self {
            observed_at_unix_ms: unix_time_ms(),
            level: log_level_name(level),
            message: bounded_text(message, MAX_LOG_MESSAGE_BYTES),
        }
    }

    fn triggers_report(&self) -> bool {
        matches!(self.level, "warning" | "error")
    }

    fn diagnostic_type(&self) -> DiagnosticType {
        match self.level {
            "error" => DiagnosticType::Error,
            _ => DiagnosticType::Warning,
        }
    }
}

pub(super) struct TelemetryReporter {
    generation: u64,
    log_sender: Option<mpsc::Sender<LogEntry>>,
    observation_sender: Option<mpsc::Sender<ServerObservation>>,
    collector_task: Option<JoinHandle<()>>,
    uploader_task: Option<JoinHandle<()>>,
}

impl TelemetryReporter {
    pub(super) fn start(handle: &Handle, shared: Weak<Shared>) -> Option<Self> {
        let host_uuid = raw::pbx_uuid()?;
        let host_hash: [u8; 32] = Sha256::digest(host_uuid.as_bytes()).into();
        let generation = NEXT_GENERATION
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .ok()?;
        let (log_sender, log_receiver) = mpsc::channel(LOG_QUEUE_ITEMS);
        let (observation_sender, observation_receiver) = mpsc::channel(OBSERVATION_QUEUE_ITEMS);
        let (report_sender, report_receiver) = mpsc::channel(REPORT_QUEUE_ITEMS);
        register_logger(generation, log_sender.clone());
        let collector_task = handle.spawn(collect(
            log_receiver,
            observation_receiver,
            report_sender,
            shared,
        ));
        let uploader_task = handle.spawn(transport::upload(
            report_receiver,
            env!("CARGO_PKG_VERSION"),
            host_hash,
        ));
        Some(Self {
            generation,
            log_sender: Some(log_sender),
            observation_sender: Some(observation_sender),
            collector_task: Some(collector_task),
            uploader_task: Some(uploader_task),
        })
    }

    pub(super) fn observation_sender(&self) -> Option<mpsc::Sender<ServerObservation>> {
        self.observation_sender.clone()
    }

    pub(super) async fn shutdown(&mut self) {
        unregister_logger(self.generation);
        self.log_sender.take();
        self.observation_sender.take();
        let deadline = tokio::time::Instant::now() + SHUTDOWN_TIMEOUT;
        finish_task(&mut self.collector_task, deadline).await;
        finish_task(&mut self.uploader_task, deadline).await;
    }
}

impl Drop for TelemetryReporter {
    fn drop(&mut self) {
        unregister_logger(self.generation);
        if let Some(task) = &self.collector_task {
            task.abort();
        }
        if let Some(task) = &self.uploader_task {
            task.abort();
        }
    }
}

pub(super) fn record_log(level: LogLevel, message: &str) {
    let sender = active_logger().and_then(|registry| {
        let active = match registry.lock() {
            Ok(active) => active,
            Err(poisoned) => poisoned.into_inner(),
        };
        active.as_ref().map(|active| active.sender.clone())
    });
    if let Some(sender) = sender {
        let _ = sender.try_send(LogEntry::new(level, message));
    }
}

async fn collect(
    mut logs: mpsc::Receiver<LogEntry>,
    mut observations: mpsc::Receiver<ServerObservation>,
    reports: mpsc::Sender<PendingBatch>,
    shared: Weak<Shared>,
) {
    let mut logs_open = true;
    let mut observations_open = true;
    let mut recent_logs = VecDeque::with_capacity(RECENT_LOG_ITEMS);
    let mut packets = PacketCapture::new();
    while logs_open || observations_open {
        tokio::select! {
            log = logs.recv(), if logs_open => match log {
                Some(log) => handle_log(log, &mut recent_logs, &packets, &reports, &shared),
                None => logs_open = false,
            },
            observation = observations.recv(), if observations_open => match observation {
                Some(observation) => handle_observation(observation, &mut packets),
                None => observations_open = false,
            },
        }
    }
}

fn handle_log(
    log: LogEntry,
    recent_logs: &mut VecDeque<LogEntry>,
    packets: &PacketCapture,
    reports: &mpsc::Sender<PendingBatch>,
    shared: &Weak<Shared>,
) {
    retain_log(recent_logs, log.clone());
    if !log.triggers_report() {
        return;
    }
    let diagnostic_event_id = Uuid::new_v4().to_string();
    let packet_snapshot = packets.snapshot();
    let packet_event_id = (!packet_snapshot.is_empty()).then(|| Uuid::new_v4().to_string());
    let diagnostic = PendingEvent::new(
        diagnostic_event_id.clone(),
        log.diagnostic_type() as i32,
        "application/json",
        snapshot::diagnostic_body(
            &log,
            &recent_logs.iter().cloned().collect::<Vec<_>>(),
            shared,
            packet_event_id.as_deref(),
        ),
    );
    let Some(diagnostic) = diagnostic else {
        return;
    };
    let mut events = vec![diagnostic];
    if let Some(packet_event_id) = packet_event_id
        && let Some(packet) = PendingEvent::new(
            packet_event_id,
            DiagnosticType::PacketCapture as i32,
            "application/vnd.chan-sccp2.signaling+json",
            snapshot::packet_body(&packet_snapshot, &diagnostic_event_id),
        )
    {
        events.push(packet);
    }
    let _ = reports.try_send(PendingBatch { events });
}

fn handle_observation(observation: ServerObservation, packets: &mut PacketCapture) {
    let observation_id = observation.observation_id;
    let observed_at_unix_ms = observation.observed_at_unix_ms;
    packets.record_dropped(observation.dropped_observations);
    match observation.kind {
        ServerObservationKind::Connected {
            connection_id,
            peer,
            local,
            transport,
        } => packets.connected(connection_id, peer, local, transport),
        ServerObservationKind::Signaling(signaling) => {
            packets.signaling(observation_id, observed_at_unix_ms, signaling)
        }
        ServerObservationKind::Identified {
            connection_id,
            device_id,
            session_generation,
        } => packets.identified(
            connection_id,
            device_id.to_string(),
            session_generation.get(),
        ),
        ServerObservationKind::Disconnected {
            connection_id,
            reason,
        } => packets.disconnected(observation_id, observed_at_unix_ms, connection_id, reason),
        _ => {}
    }
}

fn retain_log(logs: &mut VecDeque<LogEntry>, log: LogEntry) {
    if logs.len() == RECENT_LOG_ITEMS {
        logs.pop_front();
    }
    logs.push_back(log);
}

async fn finish_task(task: &mut Option<JoinHandle<()>>, deadline: tokio::time::Instant) {
    let Some(handle) = task.as_mut() else {
        return;
    };
    if tokio::time::timeout_at(deadline, &mut *handle)
        .await
        .is_err()
    {
        handle.abort();
    }
    task.take();
}

fn register_logger(generation: u64, sender: mpsc::Sender<LogEntry>) {
    let registry = ACTIVE_LOGGER.get_or_init(|| Mutex::new(None));
    let mut active = match registry.lock() {
        Ok(active) => active,
        Err(poisoned) => poisoned.into_inner(),
    };
    *active = Some(ActiveLogger { generation, sender });
}

fn unregister_logger(generation: u64) {
    let Some(registry) = active_logger() else {
        return;
    };
    let mut active = match registry.lock() {
        Ok(active) => active,
        Err(poisoned) => poisoned.into_inner(),
    };
    if active
        .as_ref()
        .is_some_and(|active| active.generation == generation)
    {
        *active = None;
    }
}

fn active_logger() -> Option<&'static Mutex<Option<ActiveLogger>>> {
    ACTIVE_LOGGER.get()
}

const fn log_level_name(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Error => "error",
        LogLevel::Warning => "warning",
        LogLevel::Notice => "notice",
        LogLevel::Debug => "debug",
    }
}

fn bounded_text(value: &str, maximum_bytes: usize) -> String {
    if value.len() <= maximum_bytes {
        return value.to_owned();
    }
    let mut end = maximum_bytes;
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    value[..end].to_owned()
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}
