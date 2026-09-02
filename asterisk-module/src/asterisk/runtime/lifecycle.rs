use super::{
    Access, AmiEventPublisher, Arc, AsteriskDatabase, AsteriskDialplan, AsteriskHints,
    AsteriskHttp, AsteriskManager, AsteriskParking, AsyncMutex, AtomicU64, BTreeMap,
    BlfSubscriptions, Builder, CallId, CallSelectionOrder, Codec, ConferenceTaskRegistry,
    ConfigReconciliation, ConfigReconciliationTrigger, ConfigurationProvider, Controller, DeviceId,
    Duration, ExternalAddressCache, FeatureStore, ForwardingEntryRegistry, HashMap, HashSet,
    Instant, LineBinding, LineInstance, LogLevel, MODULE, MediaAnchorRegistry, MediaAnchorRestores,
    MediaEndpoint, MobilityRegistry, Module, ModuleConfig, Mutex, MutexExt as _,
    NoAnswerTimerRegistry, ParkingRegistry, PbxCallId, PhoneCommand, PhoneCommandAction,
    RECORDING_TRIGGER_WAKE_CAPACITY, RegistrationFallback, RegistrationRegistryError,
    RegistrationTokenPolicy, ReloadPlan, ReloadSelection, RuntimeCallSignal,
    RuntimeCallSignalDeliveryResult, RuntimeCallSignalKind, RuntimeCallSignalQueue,
    RuntimeCalledPartyProvider, RuntimeChannelQueryProvider, RuntimeCodecPreferenceProvider,
    RuntimeControlProvider, RuntimeDeviceQueryProvider, RuntimeDirectoryProvider,
    RuntimeFeatureControlProvider, RuntimeHandsetCallIndicationProvider,
    RuntimeHandsetMessageProvider, RuntimeInventoryProvider, RuntimeLineQueryProvider,
    RuntimeRecordingTriggerQueue, RuntimeRegistrationContexts, RuntimeServiceProvider, RwLock,
    RwLockExt as _, Semaphore, Server, ServerConfig, ServerIngress, Shared, SignalingQos,
    SignalingSocket, StagedMwiSubscriptions, StationIo, StationTransport, SystemHostResolver,
    adapters, anonymous_hotline_definition, ast_log, configured_mobility_button, controller_step,
    dial_terminator_digit, log_feature_store_error, mobility_device_registered, mpsc,
    native_channel, publish_device_features, publish_feature_changes, publish_line, raw,
    register_called_party_application, register_channel_query,
    register_codec_preference_application, register_control_actions, register_device_query,
    register_directory_http, register_feature_control_actions,
    register_handset_call_indication_application, register_handset_message_application,
    register_inventory_actions, register_line_query, register_runtime_status_actions,
    register_service_control_actions, run_call_signals, run_events, shutdown_conferences,
    shutdown_one_way_microphones, shutdown_remote_hangups, uninstall_blf, uninstall_device_blf,
};
use crate::call::parking::ParkingEventSource as _;
use crate::media::encryption::AudioEncryptionAdmissions;
use crate::runtime::tls::RuntimeTlsAcceptor;
use sccp_protocol::StationSocketQos as _;
use std::net::SocketAddr;
use std::os::fd::AsFd;
use std::path::PathBuf;
use tokio::net::TcpListener;
use tokio::task::JoinSet;

impl From<crate::config::FallbackDecision> for RegistrationFallback {
    fn from(value: crate::config::FallbackDecision) -> Self {
        match value {
            crate::config::FallbackDecision::Reject => Self::Reject,
            crate::config::FallbackDecision::Accept => Self::ReturnToPrimary,
            crate::config::FallbackDecision::DeviceIdOdd => Self::DeviceIdOdd,
            crate::config::FallbackDecision::DeviceIdEven => Self::DeviceIdEven,
        }
    }
}

impl From<crate::config::CallAnswerOrder> for CallSelectionOrder {
    fn from(value: crate::config::CallAnswerOrder) -> Self {
        match value {
            crate::config::CallAnswerOrder::OldestFirst => Self::OldestFirst,
            crate::config::CallAnswerOrder::LastFirst => Self::LastFirst,
        }
    }
}

const MAX_CONCURRENT_SECURE_HANDSHAKES: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AcceptedSocketStage {
    Accept,
    ConfigureNoDelay,
    InspectLocalAddress,
}

fn accepted_socket_error(
    kind: &str,
    stage: AcceptedSocketStage,
    error: &dyn std::fmt::Display,
) -> String {
    match stage {
        AcceptedSocketStage::Accept => format!("{kind} signaling listener failed: {error}"),
        AcceptedSocketStage::ConfigureNoDelay => {
            format!("unable to configure {kind} signaling socket: {error}")
        }
        AcceptedSocketStage::InspectLocalAddress => {
            format!("unable to inspect {kind} signaling socket: {error}")
        }
    }
}

fn secure_handshake_limiter() -> Arc<Semaphore> {
    Arc::new(Semaphore::new(MAX_CONCURRENT_SECURE_HANDSHAKES))
}

async fn bind_runtime_listeners(
    clear: std::net::SocketAddr,
    secure: Option<std::net::SocketAddr>,
    signaling_qos: SignalingQos,
) -> Result<(TcpListener, Option<TcpListener>), String> {
    let clear_listener = TcpListener::bind(clear)
        .await
        .map_err(|error| format!("unable to bind clear signaling listener: {error}"))?;
    apply_listener_qos(&clear_listener, signaling_qos, "clear");
    let secure_listener = match secure {
        Some(address) => {
            let listener = TcpListener::bind(address)
                .await
                .map_err(|error| format!("unable to bind secure signaling listener: {error}"))?;
            apply_listener_qos(&listener, signaling_qos, "secure");
            Some(listener)
        }
        None => None,
    };
    Ok((clear_listener, secure_listener))
}

fn apply_listener_qos(listener: &TcpListener, signaling_qos: SignalingQos, kind: &str) {
    let local = match listener.local_addr() {
        Ok(local) => local,
        Err(error) => {
            ast_log(
                LogLevel::Warning,
                &format!("unable to inspect {kind} signaling listener for QoS: {error}"),
            );
            return;
        }
    };
    if let Some(socket) = capture_socket_qos(listener, local, kind) {
        report_socket_qos(&socket, signaling_qos, kind, local);
    }
}

fn capture_socket_qos<S>(socket: &S, local: SocketAddr, kind: &str) -> Option<SignalingSocket>
where
    S: AsFd,
{
    match SignalingSocket::capture(socket, local) {
        Ok(socket) => Some(socket),
        Err(error) => {
            ast_log(
                LogLevel::Warning,
                &format!("unable to retain {kind} signaling socket QoS control: {error}"),
            );
            None
        }
    }
}

fn report_socket_qos(
    socket: &SignalingSocket,
    signaling_qos: SignalingQos,
    kind: &str,
    endpoint: SocketAddr,
) {
    report_socket_qos_failures(socket.apply(signaling_qos), kind, endpoint, |message| {
        ast_log(LogLevel::Warning, &message)
    });
}

fn report_socket_qos_failures(
    report: sccp_protocol::SocketQosReport,
    kind: &str,
    endpoint: SocketAddr,
    mut warn: impl FnMut(String),
) {
    for failure in report.into_failures() {
        warn(format!("{kind} signaling socket {endpoint}: {failure}"));
    }
}

async fn admit_station<S>(
    ingress: &ServerIngress,
    stream: S,
    peer: SocketAddr,
    local: SocketAddr,
    transport: StationTransport,
    socket_qos: Option<SignalingSocket>,
) -> Result<(), String>
where
    S: StationIo + 'static,
{
    match socket_qos {
        Some(socket_qos) => {
            ingress
                .accept_with_socket_qos(stream, peer, local, transport, socket_qos)
                .await
        }
        None => ingress.accept(stream, peer, local, transport).await,
    }
    .map_err(|error| error.to_string())
}

async fn accept_station_socket(
    listener: &TcpListener,
    kind: &'static str,
) -> Result<
    (
        tokio::net::TcpStream,
        SocketAddr,
        SocketAddr,
        Option<SignalingSocket>,
    ),
    String,
> {
    let (stream, peer) = listener
        .accept()
        .await
        .map_err(|error| accepted_socket_error(kind, AcceptedSocketStage::Accept, &error))?;
    stream.set_nodelay(true).map_err(|error| {
        accepted_socket_error(kind, AcceptedSocketStage::ConfigureNoDelay, &error)
    })?;
    let local = stream.local_addr().map_err(|error| {
        accepted_socket_error(kind, AcceptedSocketStage::InspectLocalAddress, &error)
    })?;
    let socket_qos = capture_socket_qos(&stream, local, kind);
    Ok((stream, peer, local, socket_qos))
}

async fn run_clear_listener(listener: TcpListener, ingress: ServerIngress) -> Result<(), String> {
    loop {
        let (stream, peer, local, socket_qos) = accept_station_socket(&listener, "clear").await?;
        admit_station(
            &ingress,
            stream,
            peer,
            local,
            StationTransport::Clear,
            socket_qos,
        )
        .await?;
    }
}

async fn run_secure_listener(
    listener: Option<TcpListener>,
    acceptor: Option<RuntimeTlsAcceptor>,
    ingress: ServerIngress,
) -> Result<(), String> {
    let Some((listener, acceptor)) = listener.zip(acceptor) else {
        return std::future::pending().await;
    };
    let permits = secure_handshake_limiter();
    let mut handshakes = JoinSet::new();
    loop {
        while let Some(result) = handshakes.try_join_next() {
            if let Err(error) = result {
                ast_log(
                    LogLevel::Warning,
                    &format!("secure signaling handshake task failed: {error}"),
                );
            }
        }
        let permit = Arc::clone(&permits)
            .acquire_owned()
            .await
            .map_err(|_| "secure signaling handshake limiter stopped".to_owned())?;
        let (stream, peer, local, socket_qos) = accept_station_socket(&listener, "secure").await?;
        let acceptor = acceptor.clone();
        let ingress = ingress.clone();
        handshakes.spawn(async move {
            let _permit = permit;
            match acceptor.accept(stream).await {
                Ok(stream) => {
                    let _ = admit_station(
                        &ingress,
                        stream,
                        peer,
                        local,
                        StationTransport::Secure,
                        socket_qos,
                    )
                    .await;
                }
                Err(error) => ast_log(LogLevel::Warning, &error.to_string()),
            }
        });
    }
}

#[derive(Clone, Copy)]
pub struct ChannelState {
    pub pbx_id: PbxCallId,
    pub sccp_id: CallId,
}

impl From<native_channel::ChannelIdentity> for ChannelState {
    fn from(identity: native_channel::ChannelIdentity) -> Self {
        Self {
            pbx_id: PbxCallId(identity.pbx_id),
            sccp_id: CallId(identity.sccp_id),
        }
    }
}

#[cfg(test)]
mod accepted_socket_tests {
    use super::*;

    #[tokio::test]
    async fn clear_and_secure_accept_paths_share_socket_setup_and_qos_capture() {
        for kind in ["clear", "secure"] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let connect =
                tokio::spawn(async move { tokio::net::TcpStream::connect(address).await });
            let (stream, peer, local, socket_qos) =
                accept_station_socket(&listener, kind).await.unwrap();
            connect.await.unwrap().unwrap();
            assert!(stream.nodelay().unwrap());
            assert_eq!(local, address);
            assert_eq!(
                peer.ip(),
                std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
            );
            assert!(socket_qos.is_some());
        }
    }

    #[test]
    fn clear_and_secure_accept_failures_keep_the_listener_family() {
        for kind in ["clear", "secure"] {
            for (stage, expected) in [
                (
                    AcceptedSocketStage::Accept,
                    format!("{kind} signaling listener failed: closed"),
                ),
                (
                    AcceptedSocketStage::ConfigureNoDelay,
                    format!("unable to configure {kind} signaling socket: closed"),
                ),
                (
                    AcceptedSocketStage::InspectLocalAddress,
                    format!("unable to inspect {kind} signaling socket: closed"),
                ),
            ] {
                assert_eq!(accepted_socket_error(kind, stage, &"closed"), expected);
            }
        }
    }

    #[tokio::test]
    async fn secure_handshake_limiter_holds_the_sixty_fifth_handshake() {
        let permits = secure_handshake_limiter();
        let occupied = Arc::clone(&permits)
            .acquire_many_owned(MAX_CONCURRENT_SECURE_HANDSHAKES as u32)
            .await
            .unwrap();
        assert_eq!(permits.available_permits(), 0);
        let pending = Arc::clone(&permits).acquire_owned();
        tokio::pin!(pending);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut pending)
                .await
                .is_err()
        );
        drop(occupied);
        assert!(
            tokio::time::timeout(Duration::from_secs(1), pending)
                .await
                .unwrap()
                .is_ok()
        );
    }

    #[test]
    fn qos_application_failures_are_reported_with_listener_identity() {
        let report = sccp_protocol::SocketQosReport::failed(
            sccp_protocol::SocketQosMark::Dscp,
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
        );
        let mut warnings = Vec::new();
        report_socket_qos_failures(
            report,
            "secure",
            "127.0.0.1:2000".parse().unwrap(),
            |message| warnings.push(message),
        );
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("secure signaling socket 127.0.0.1:2000"));
        assert!(warnings[0].contains("unable to apply socket DSCP"));
    }
}

impl From<ChannelState> for native_channel::ChannelIdentity {
    fn from(state: ChannelState) -> Self {
        Self {
            pbx_id: state.pbx_id.0,
            sccp_id: state.sccp_id.0,
        }
    }
}

#[derive(Clone)]
pub struct DirectMediaCall {
    pub pbx_id: PbxCallId,
    pub device_id: DeviceId,
    pub call_id: CallId,
    pub line_instance: u32,
    pub codec: Codec,
    pub phone_endpoint: MediaEndpoint,
    pub transmit_endpoint: MediaEndpoint,
}

impl Module {
    pub fn start(
        config_provider: Arc<dyn ConfigurationProvider>,
        config: ModuleConfig,
    ) -> Result<Self, String> {
        let runtime = Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("sccp-runtime")
            .enable_all()
            .build()
            .map_err(|error| format!("unable to create SCCP runtime: {error}"))?;
        let listener_policy = config.listener_policy().clone();
        let tls_acceptor = listener_policy
            .tls
            .as_ref()
            .map(RuntimeTlsAcceptor::from_listener)
            .transpose()
            .map_err(|error| error.to_string())?;
        let signaling_qos = SignalingQos::new(
            config.general.qos.signaling.dscp.0,
            config.general.qos.signaling.cos.0,
        );
        let (clear_listener, secure_listener) = runtime.block_on(bind_runtime_listeners(
            listener_policy.clear,
            listener_policy.tls.as_ref().map(|listener| listener.bind),
            signaling_qos,
        ))?;
        let server_config = ServerConfig {
            bind: listener_policy.clear,
            signaling_qos,
            advertised_address: config.general.advertised_address,
            advertised_ipv6_address: config.general.network.advertised.ipv6,
            server_name: config.general.server_name.clone(),
            keepalive_seconds: config.general.keepalive_seconds,
            secondary_keepalive_seconds: config.general.secondary_keepalive_seconds,
            signaling_servers: config.general.signaling_servers.clone(),
            registration_tokens: RegistrationTokenPolicy {
                fallback: config.general.fallback_registration.decision.into(),
                backoff: Duration::from_secs(
                    config.general.fallback_registration.backoff_seconds.into(),
                ),
                server_priority: config.general.fallback_registration.server_priority,
            },
            firmware_version: String::new(),
            dial_terminator: dial_terminator_digit(config.general.dial_terminator.character)?,
            record_dial_terminator: config.general.dial_terminator.record,
            call_answer_order: config.general.call_answer_order.into(),
            timezone_offset_minutes: config.general.timezone_offset_minutes,
            date_template: config.general.date_template.clone(),
            anonymous_hotline: anonymous_hotline_definition(&config)?,
        };
        let definitions = config.device_definitions();
        let feature_store = FeatureStore::new(AsteriskDatabase::new());
        let feature_states = feature_store
            .load_configuration(&config)
            .map_err(|error| format!("unable to restore configured feature state: {error}"))?;
        let (blf_events_tx, blf_events) = mpsc::unbounded_channel();
        let (parking_events_tx, parking_events) = mpsc::unbounded_channel();
        let (control_requests_tx, control_requests) = mpsc::unbounded_channel();
        let (service_requests_tx, service_requests) = mpsc::unbounded_channel();
        let (call_signals_tx, call_signals) = mpsc::unbounded_channel();
        let (recording_trigger_wake, recording_triggers) =
            mpsc::channel(RECORDING_TRIGGER_WAKE_CAPACITY);
        let parking_subscription = AsteriskParking::new()
            .subscribe(move |event| {
                let _ = parking_events_tx.send(event);
            })
            .map_err(|error| format!("unable to subscribe to parking events: {error}"))?;
        let (server, phone, events, ingress) = Server::with_ingress(server_config, definitions)
            .map_err(|error| format!("unable to start SCCP listener: {error}"))?;
        let mut controller = Controller::with_digit_timeouts(
            Duration::from_millis(config.general.first_digit_timeout_ms),
            Duration::from_millis(config.general.interdigit_timeout_ms),
        );
        controller.set_dial_terminator(config.general.dial_terminator.character);
        controller.set_simulated_enbloc(config.general.simulate_enbloc);
        controller.set_overlap_devices(
            config
                .devices
                .values()
                .filter(|device| device.allow_overlap)
                .map(|device| device.id.clone()),
        );
        controller.set_line_dial_tones(
            config
                .line_features
                .iter()
                .map(|(line, features)| (line.clone(), features.dial_tones.clone())),
        );
        controller.set_line_incoming_limits(
            config
                .line_features
                .iter()
                .map(|(line, features)| (line.clone(), features.incoming_limit)),
        );
        controller.replace_feature_states(feature_states);
        let mut external_addresses = ExternalAddressCache::new(SystemHostResolver);
        if let Err(error) =
            external_addresses.refresh(config.general.network.external.as_ref(), Instant::now())
        {
            ast_log(
                LogLevel::Warning,
                &format!("unable to resolve configured external address: {error}"),
            );
        }
        let shared = Arc::new(Shared {
            controller: Mutex::new(controller),
            external_addresses: Mutex::new(external_addresses),
            published_line_states: Mutex::new(HashMap::new()),
            config: RwLock::new(Arc::new(config)),
            config_provider,
            config_reconciliation: Arc::new(ConfigReconciliation::default()),
            config_reloads: Mutex::new(()),
            channels: Mutex::new(HashMap::new()),
            assigned_channel_ids: Mutex::new(HashMap::new()),
            audio_packet_ms: Mutex::new(HashMap::new()),
            audio_preferences: Mutex::new(HashMap::new()),
            audio_encryption_admissions: Mutex::new(AudioEncryptionAdmissions::default()),
            media_anchor_mutations: AsyncMutex::new(()),
            media_anchors: Mutex::new(MediaAnchorRegistry::default()),
            media_anchor_restores: Mutex::new(MediaAnchorRestores::default()),
            conference_announcements: Mutex::new(HashMap::new()),
            conference_announcement_mutations: Mutex::new(()),
            next_conference_announcement_id: AtomicU64::new(1),
            conference_destination_tasks: Mutex::new(ConferenceTaskRegistry::default()),
            bridges: Mutex::new(HashMap::new()),
            barge_bridges: Mutex::new(HashMap::new()),
            forwarded_calls: Mutex::new(HashMap::new()),
            no_answer_plans: Mutex::new(HashMap::new()),
            no_answer_timers: Mutex::new(NoAnswerTimerRegistry::default()),
            forwarding_entries: Mutex::new(ForwardingEntryRegistry::default()),
            mobility: Mutex::new(MobilityRegistry::new()),
            mobility_mutations: AsyncMutex::new(()),
            pending_mobility_prompts: Mutex::new(HashMap::new()),
            next_mobility_prompt_id: AtomicU64::new(1),
            parking_registry: Mutex::new(ParkingRegistry::default()),
            pending_parks: Mutex::new(HashMap::new()),
            pending_retrievals: Mutex::new(HashMap::new()),
            parking_notifications: Mutex::new(Vec::new()),
            mwi_subscriptions: Mutex::new(HashMap::new()),
            blf_subscriptions: Mutex::new(BlfSubscriptions::new(
                AsteriskHints::new(),
                blf_events_tx,
            )),
            feature_store,
            feature_mutations: Mutex::new(()),
            registration_contexts: Mutex::new(RuntimeRegistrationContexts::new()),
            system_message: Mutex::new(None),
            control_requests: control_requests_tx.clone(),
            call_signals: Mutex::new(RuntimeCallSignalQueue {
                next_sequence: 1,
                sender: call_signals_tx,
            }),
            recording_trigger_wake,
            pending_recording_triggers: Mutex::new(RuntimeRecordingTriggerQueue::default()),
            ami_events: AmiEventPublisher::new(AsteriskManager::new()),
            manager_registrations: Mutex::new(Vec::new()),
            dialplan_registrations: Mutex::new(Vec::new()),
            http_registrations: Mutex::new(Vec::new()),
        });
        #[cfg(feature = "telemetry")]
        let telemetry = crate::asterisk::telemetry::TelemetryReporter::start(
            runtime.handle(),
            Arc::downgrade(&shared),
        );
        #[cfg(feature = "telemetry")]
        let server = match telemetry
            .as_ref()
            .and_then(crate::asterisk::telemetry::TelemetryReporter::observation_sender)
        {
            Some(sender) => server.with_observation_sender(sender),
            None => server,
        };
        let directory_registration = register_directory_http(
            RuntimeDirectoryProvider {
                shared: Arc::downgrade(&shared),
            },
            AsteriskHttp::new(),
        )
        .map_err(|error| format!("unable to register phone directory HTTP service: {error}"))?;
        shared
            .http_registrations
            .lock_unpoisoned()
            .push(directory_registration);
        let manager = AsteriskManager::new();
        let mut manager_registrations = register_inventory_actions(
            RuntimeInventoryProvider {
                shared: Arc::downgrade(&shared),
                phone: phone.clone(),
            },
            manager,
        )
        .map_err(|error| format!("unable to register management inventory actions: {error}"))?;
        manager_registrations.extend(
            register_runtime_status_actions(
                RuntimeInventoryProvider {
                    shared: Arc::downgrade(&shared),
                    phone: phone.clone(),
                },
                manager,
            )
            .map_err(|error| format!("unable to register live management actions: {error}"))?,
        );
        manager_registrations.extend(
            register_feature_control_actions(
                RuntimeFeatureControlProvider {
                    shared: Arc::downgrade(&shared),
                    handle: runtime.handle().clone(),
                    phone: phone.clone(),
                },
                manager,
            )
            .map_err(|error| format!("unable to register feature-control actions: {error}"))?,
        );
        manager_registrations.extend(
            register_control_actions(
                RuntimeControlProvider {
                    requests: control_requests_tx,
                },
                manager,
            )
            .map_err(|error| format!("unable to register management controls: {error}"))?,
        );
        manager_registrations.extend(
            register_service_control_actions(
                RuntimeServiceProvider {
                    requests: service_requests_tx,
                },
                manager,
            )
            .map_err(|error| format!("unable to register management service controls: {error}"))?,
        );
        *shared.manager_registrations.lock_unpoisoned() = manager_registrations;
        let dialplan = AsteriskDialplan::new();
        let device_query_registration = register_device_query(
            RuntimeDeviceQueryProvider {
                shared: Arc::downgrade(&shared),
            },
            dialplan,
        )
        .map_err(|error| format!("unable to register device query function: {error}"))?;
        let line_query_registration = register_line_query(
            RuntimeLineQueryProvider {
                shared: Arc::downgrade(&shared),
            },
            dialplan,
        )
        .map_err(|error| format!("unable to register line query function: {error}"))?;
        let channel_query_registration = register_channel_query(
            RuntimeChannelQueryProvider {
                shared: Arc::downgrade(&shared),
            },
            dialplan,
        )
        .map_err(|error| format!("unable to register channel query function: {error}"))?;
        let codec_preference_registration = register_codec_preference_application(
            RuntimeCodecPreferenceProvider {
                shared: Arc::downgrade(&shared),
            },
            dialplan,
        )
        .map_err(|error| format!("unable to register codec preference application: {error}"))?;
        let called_party_registration = register_called_party_application(
            RuntimeCalledPartyProvider {
                shared: Arc::downgrade(&shared),
                phone: phone.clone(),
            },
            dialplan,
        )
        .map_err(|error| format!("unable to register called-party application: {error}"))?;
        let handset_message_registration = register_handset_message_application(
            RuntimeHandsetMessageProvider {
                shared: Arc::downgrade(&shared),
                phone: phone.clone(),
            },
            dialplan,
        )
        .map_err(|error| format!("unable to register handset-message application: {error}"))?;
        let handset_call_indication_registration = register_handset_call_indication_application(
            RuntimeHandsetCallIndicationProvider {
                shared: Arc::downgrade(&shared),
                phone: phone.clone(),
            },
            dialplan,
        )
        .map_err(|error| format!("unable to register call-indication application: {error}"))?;
        *shared.dialplan_registrations.lock_unpoisoned() = vec![
            device_query_registration,
            line_query_registration,
            channel_query_registration,
            codec_preference_registration,
            called_party_registration,
            handset_message_registration,
            handset_call_indication_registration,
        ];
        let handle = runtime.handle().clone();
        let server_task = runtime.spawn(async move {
            let secure_ingress = ingress.clone();
            tokio::select! {
                result = server.run() => {
                    if let Err(error) = result {
                        ast_log(LogLevel::Error, &format!("SCCP server stopped: {error}"));
                    }
                }
                result = run_clear_listener(clear_listener, ingress) => {
                    if let Err(error) = result {
                        ast_log(LogLevel::Error, &error);
                    }
                }
                result = run_secure_listener(secure_listener, tls_acceptor, secure_ingress) => {
                    if let Err(error) = result {
                        ast_log(LogLevel::Error, &error);
                    }
                }
            }
        });
        let access = Access {
            handle,
            phone,
            shared,
        };
        let event_access = access.clone();
        let signal_access = access.clone();
        let event_task = runtime.spawn(async move {
            tokio::join!(
                run_events(
                    event_access,
                    events,
                    blf_events,
                    parking_events,
                    control_requests,
                    service_requests,
                    recording_triggers,
                ),
                run_call_signals(signal_access, call_signals),
            );
        });
        Ok(Self {
            runtime,
            access,
            server_task,
            event_task,
            parking_subscription,
            sorcery_registration: None,
            #[cfg(feature = "telemetry")]
            telemetry,
        })
    }

    pub fn stop(mut self) {
        self.access.shared.ami_events.close();
        self.access
            .shared
            .manager_registrations
            .lock_unpoisoned()
            .clear();
        self.access
            .shared
            .http_registrations
            .lock_unpoisoned()
            .clear();
        self.access
            .shared
            .dialplan_registrations
            .lock_unpoisoned()
            .clear();
        uninstall_blf(&self.access);
        // Stop accepting handset mutations before the controller snapshot is
        // drained. The server stays alive long enough to deliver the terminal
        // handset cleanup commands below.
        self.event_task.abort();
        let phone = self.access.phone.clone();
        self.runtime.block_on(async {
            let _ = tokio::time::timeout(Duration::from_secs(2), &mut self.event_task).await;
            shutdown_conferences(&self.access).await;
            shutdown_remote_hangups(&self.access).await;
            shutdown_one_way_microphones(&self.access).await;
            let _ = phone.shutdown().await;
            self.access
                .shared
                .conference_announcements
                .lock_unpoisoned()
                .clear();
            let _ = tokio::time::timeout(Duration::from_secs(2), &mut self.server_task).await;
        });
        self.server_task.abort();
        self.event_task.abort();
        if let Err(error) = self
            .access
            .shared
            .registration_contexts
            .lock_unpoisoned()
            .registry
            .clear()
        {
            ast_log(
                LogLevel::Error,
                &format!("unable to remove registration-context extensions during unload: {error}"),
            );
        }
        self.parking_subscription.unsubscribe();
        #[cfg(feature = "telemetry")]
        if let Some(telemetry) = &mut self.telemetry {
            self.runtime.block_on(telemetry.shutdown());
        }
        self.runtime.shutdown_timeout(Duration::from_secs(1));
    }
}

impl Access {
    pub fn control_provider(&self) -> RuntimeControlProvider {
        RuntimeControlProvider {
            requests: self.shared.control_requests.clone(),
        }
    }

    pub fn feature_control_provider(&self) -> RuntimeFeatureControlProvider {
        RuntimeFeatureControlProvider {
            shared: Arc::downgrade(&self.shared),
            handle: self.handle.clone(),
            phone: self.phone.clone(),
        }
    }

    pub fn enqueue_call_signal(&self, pbx_id: PbxCallId, kind: RuntimeCallSignalKind) -> bool {
        self.enqueue_call_signal_inner(pbx_id, kind)
    }

    pub fn enqueue_confirmed_answer_signal(
        &self,
        pbx_id: PbxCallId,
    ) -> Option<std::sync::mpsc::Receiver<RuntimeCallSignalDeliveryResult>> {
        let (completion, receipt) = std::sync::mpsc::sync_channel(1);
        self.enqueue_call_signal_inner(pbx_id, RuntimeCallSignalKind::Answer { completion })
            .then_some(receipt)
    }

    fn enqueue_call_signal_inner(&self, pbx_id: PbxCallId, kind: RuntimeCallSignalKind) -> bool {
        let mut queue = self.shared.call_signals.lock_unpoisoned();
        let Some(next_sequence) = queue.next_sequence.checked_add(1) else {
            ast_log(
                LogLevel::Error,
                "SCCP call-signal sequence space is exhausted",
            );
            return false;
        };
        let signal = RuntimeCallSignal {
            sequence: queue.next_sequence,
            pbx_id,
            kind,
        };
        if queue.sender.send(signal).is_err() {
            return false;
        }
        queue.next_sequence = next_sequence;
        true
    }

    pub fn spawn_phone(&self, command: PhoneCommand) {
        if let Err(error) = self.phone.try_send(command) {
            ast_log(
                LogLevel::Warning,
                &format!("unable to enqueue SCCP command: {error}"),
            );
        }
    }

    pub fn config(&self) -> Arc<ModuleConfig> {
        self.shared.config.read_unpoisoned().clone()
    }

    pub fn line_binding(&self, device_id: &DeviceId, line_instance: u32) -> Option<LineBinding> {
        runtime_line_binding(&self.shared, device_id, line_instance)
    }

    pub fn inbound_line_bindings(&self, address: &str) -> Vec<LineBinding> {
        let config = self.config();
        if address.split('/').count() == 2 {
            if let Some(binding) = config.dial_target(address) {
                return vec![binding.clone()];
            }
            let mut parts = address.split('/').map(str::trim);
            let Some(device) = parts.next().and_then(|value| DeviceId::new(value).ok()) else {
                return Vec::new();
            };
            let Some(line) = parts.next() else {
                return Vec::new();
            };
            return self
                .shared
                .mobility
                .lock_unpoisoned()
                .appearances_for_device(&device)
                .filter(|appearance| appearance.binding.line.number == line)
                .map(|appearance| appearance.binding.clone())
                .collect();
        }
        let Some(target) = config.dial_target(address) else {
            return Vec::new();
        };
        let mut bindings = config
            .appearances_for_line(&target.line.number)
            .cloned()
            .collect::<Vec<_>>();
        bindings.extend(
            self.shared
                .mobility
                .lock_unpoisoned()
                .appearances_for_line(&target.line.number)
                .map(|appearance| appearance.binding.clone()),
        );
        bindings
    }
}

pub fn runtime_line_binding(
    shared: &Shared,
    device_id: &DeviceId,
    line_instance: u32,
) -> Option<LineBinding> {
    let config = shared.config.read_unpoisoned().clone();
    config
        .line_for_device(device_id, line_instance)
        .cloned()
        .or_else(|| config.guest_hotline_binding(device_id, line_instance))
        .or_else(|| {
            shared
                .mobility
                .lock_unpoisoned()
                .binding_for_device(device_id, line_instance)
                .cloned()
        })
}

pub fn registered_device_ids(shared: &Shared) -> Vec<DeviceId> {
    controller_step(&shared.controller, |controller| {
        controller
            .registered_devices()
            .map(|(device, _)| device.clone())
            .collect()
    })
}

pub fn reconcile_registration_contexts(
    shared: &Shared,
    config: &ModuleConfig,
    registered_devices: &[DeviceId],
) -> Result<(), RegistrationRegistryError> {
    shared
        .registration_contexts
        .lock_unpoisoned()
        .reconcile(config, registered_devices)
}

pub fn module_access() -> Option<Access> {
    MODULE
        .lock_unpoisoned()
        .as_ref()
        .map(|module| module.access.clone())
}

pub fn config_path() -> PathBuf {
    if let Some(path) = std::env::var_os("SCCP_CONFIG") {
        return PathBuf::from(path);
    }
    adapters::config_directory()
        .unwrap_or_else(|| PathBuf::from("/etc/asterisk"))
        .join("sccp.conf")
}

pub fn reload(access: &Access) -> Result<(), String> {
    reload_selected(access, ReloadSelection::Complete)
}

pub fn reload_selected(access: &Access, selection: ReloadSelection) -> Result<(), String> {
    if access.config().general.configuration_source == crate::config::ConfigurationSource::Sorcery {
        return tracked_sorcery_reload(access, ConfigReconciliationTrigger::reload(), || {
            reload_selected_inner(access, selection)
        });
    }
    reload_selected_inner(access, selection)
}

pub fn reload_sorcery(access: &Access, trigger: ConfigReconciliationTrigger) -> Result<(), String> {
    tracked_sorcery_reload(access, trigger, || {
        reload_selected_inner(access, ReloadSelection::Complete)
    })
}

fn tracked_sorcery_reload<F>(
    access: &Access,
    trigger: ConfigReconciliationTrigger,
    apply: F,
) -> Result<(), String>
where
    F: FnOnce() -> Result<(), String>,
{
    let reconciliation = Arc::clone(&access.shared.config_reconciliation);
    reconciliation.reconcile_with(trigger, apply, |status| {
        publish_config_reconciliation_status(&status)
    })
}

fn publish_config_reconciliation_status(
    status: &crate::config::convergence::ConfigReconciliationStatus,
) {
    match serde_json::to_string(status) {
        Ok(status) => {
            if raw::system::set_global_variable(raw::system::CONFIG_STATUS_VARIABLE, Some(&status))
                .is_err()
            {
                ast_log(
                    LogLevel::Warning,
                    "unable to publish SCCP configuration convergence status",
                );
            }
        }
        Err(error) => ast_log(
            LogLevel::Warning,
            &format!("unable to serialize SCCP configuration convergence status: {error}"),
        ),
    }
}

fn reload_selected_inner(access: &Access, selection: ReloadSelection) -> Result<(), String> {
    let _reload_guard = access.shared.config_reloads.lock_unpoisoned();
    let _mobility_guard = access
        .handle
        .block_on(access.shared.mobility_mutations.lock());
    let next = access
        .shared
        .config_provider
        .refresh()
        .map_err(|error| error.to_string())?;
    let previous = access.config();
    let plan = ReloadPlan::build(&previous, &next);
    selection
        .validate(&previous, &next, &plan)
        .map_err(|error| error.to_string())?;
    if !plan.restart_required.is_empty() {
        let settings = plan
            .restart_required
            .iter()
            .map(|change| change.name())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!("{settings} changes require a module restart"));
    }
    if access
        .shared
        .mobility
        .lock_unpoisoned()
        .has_pending_transaction()
    {
        return Err("a mobility mutation is in progress; retry reload".into());
    }
    let feature_guard = access.shared.feature_mutations.lock_unpoisoned();
    let feature_states = access
        .shared
        .feature_store
        .load_configuration(&next)
        .map_err(|error| format!("unable to restore reloaded feature state: {error}"))?;
    let staged_mwi = StagedMwiSubscriptions::new(&plan.mwi_add)?;
    let registered_before = registered_device_ids(&access.shared);
    let affected: HashSet<_> = plan.affected_devices().cloned().collect();
    let phone_reconfigure_devices = plan
        .affected_devices()
        .chain(&plan.added)
        .cloned()
        .collect::<Vec<_>>();
    let registered_after = registered_before
        .iter()
        .filter(|device| !affected.contains(*device))
        .cloned()
        .collect::<Vec<_>>();
    reconcile_registration_contexts(&access.shared, &next, &registered_after)
        .map_err(|error| format!("unable to apply registration-context extensions: {error}"))?;
    let definitions = next.device_definitions();
    let anonymous_hotline = anonymous_hotline_definition(&next)?;
    let applied = match access
        .handle
        .block_on(access.phone.reconfigure_station_policy(
            definitions,
            phone_reconfigure_devices,
            anonymous_hotline,
        )) {
        Ok(applied) => applied,
        Err(error) => {
            let rollback =
                reconcile_registration_contexts(&access.shared, &previous, &registered_before);
            return Err(if rollback.is_ok() {
                format!("unable to apply SCCP definitions: {error}")
            } else {
                format!(
                    "unable to apply SCCP definitions: {error}; registration-context rollback failed"
                )
            });
        }
    };
    debug_assert_eq!(applied.added, plan.added);
    debug_assert_eq!(applied.changed, plan.changed);
    debug_assert_eq!(applied.removed, plan.removed);
    access
        .shared
        .registration_contexts
        .lock_unpoisoned()
        .suppressed_devices
        .extend(affected);
    let (registered, previous_feature_states) =
        controller_step(&access.shared.controller, |controller| {
            let previous_feature_states = registered_after
                .iter()
                .filter_map(|device| {
                    controller
                        .feature_state(device)
                        .cloned()
                        .map(|state| (device.clone(), state))
                })
                .collect::<BTreeMap<_, _>>();
            controller
                .set_interdigit_timeout(Duration::from_millis(next.general.interdigit_timeout_ms));
            controller.set_first_digit_timeout(Duration::from_millis(
                next.general.first_digit_timeout_ms,
            ));
            controller.set_simulated_enbloc(next.general.simulate_enbloc);
            controller.set_overlap_devices(
                next.devices
                    .values()
                    .filter(|device| device.allow_overlap)
                    .map(|device| device.id.clone()),
            );
            controller.set_line_dial_tones(
                next.line_features
                    .iter()
                    .map(|(line, features)| (line.clone(), features.dial_tones.clone())),
            );
            controller.set_line_incoming_limits(
                next.line_features
                    .iter()
                    .map(|(line, features)| (line.clone(), features.incoming_limit)),
            );
            controller.replace_feature_states(feature_states.clone());
            let registered = controller
                .registered_devices()
                .filter(|(device, _)| registered_after.contains(device))
                .map(|(device, _)| device.clone())
                .collect::<Vec<_>>();
            (registered, previous_feature_states)
        });
    access
        .phone
        .set_call_answer_order(next.general.call_answer_order.into());
    *access.shared.config.write_unpoisoned() = Arc::new(next);
    reconcile_mobility_after_reload(access);
    staged_mwi.commit(access, &plan.mwi_remove);
    if let Err(error) = access
        .shared
        .feature_store
        .reconcile_configuration(&access.config(), &feature_states)
    {
        log_feature_store_error("reconcile feature state after reload", None, &error);
    }
    for device in plan.affected_devices() {
        uninstall_device_blf(access, device);
    }
    for device in &registered {
        if let Some(state) = feature_states.get(device) {
            publish_device_features(access, device, state);
        }
    }
    drop(feature_guard);
    for device in registered {
        if let (Some(previous), Some(current)) = (
            previous_feature_states.get(&device),
            feature_states.get(&device),
        ) {
            publish_feature_changes(access, &device, previous, current);
        }
    }
    if let Err(error) = access.shared.config_provider.activated(&access.config()) {
        ast_log(
            LogLevel::Warning,
            &format!("SCCP configuration converged but activation persistence failed: {error}"),
        );
    }
    Ok(())
}

pub fn reconcile_mobility_after_reload(access: &Access) {
    access
        .shared
        .pending_mobility_prompts
        .lock_unpoisoned()
        .clear();
    let config = access.config();
    let removed = access
        .shared
        .mobility
        .lock_unpoisoned()
        .remove_invalid(|appearance| {
            let slot = &appearance.slot;
            let line = &appearance.binding.line.number;
            configured_mobility_button(&config, slot)
                && config.lines.contains_key(line)
                && config
                    .mobility_for_line(line)
                    .is_some_and(|mobility| mobility.pin.is_some())
                && !config
                    .appearances_for_device(&slot.device_id)
                    .any(|binding| {
                        binding.line.number == *line
                            || binding.line_instance == appearance.binding.line_instance
                    })
        })
        .unwrap_or_default();
    for appearance in removed {
        if mobility_device_registered(access, &appearance.slot.device_id)
            && access
                .handle
                .block_on(access.phone.send_confirmed(PhoneCommand::new(
                    appearance.slot.device_id.clone(),
                    PhoneCommandAction::SetMobilityAppearance {
                        mobility_instance: LineInstance::new(appearance.slot.button_instance),
                        appearance: None,
                    },
                )))
                .is_err()
        {
            ast_log(
                LogLevel::Warning,
                "unable to remove an invalid roaming mobility appearance after reload",
            );
        }
        publish_line(access, &appearance.binding.line.number);
    }
}
