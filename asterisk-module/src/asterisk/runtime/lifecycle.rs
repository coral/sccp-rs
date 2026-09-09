use std::net::SocketAddr;
use std::os::fd::AsFd;
use std::path::PathBuf;

use sccp_protocol::StationSocketQos as _;
use tokio::net::TcpListener;
use tokio::task::JoinSet;

use super::{
    Access, Arc, AsteriskDatabase, AsteriskDialplan, AsteriskHttp, AsteriskManager,
    AsteriskParking, Builder, CallId, CallSelectionOrder, Codec, ConferenceTaskRegistry,
    ConfigReconciliation, ConfigReconciliationTrigger, ConfigurationProvider, Controller, DeviceId,
    Duration, ExternalAddressCache, FeatureStore, HashMap, HashSet, Instant, LineBinding,
    LineInstance, LogLevel, MODULE, MediaEndpoint, Module, ModuleConfig, Mutex, MutexExt as _,
    PbxCallId, PhoneCommand, PhoneCommandAction, RECORDING_TRIGGER_WAKE_CAPACITY,
    RegistrationFallback, RegistrationTokenPolicy, ReloadPlan, ReloadSelection,
    RuntimeCallSignalDeliveryResult, RuntimeCallSignalKind, RuntimeCalledPartyProvider,
    RuntimeChannelQueryProvider, RuntimeCodecPreferenceProvider, RuntimeControlProvider,
    RuntimeDeviceQueryProvider, RuntimeDirectoryProvider, RuntimeFeatureControlProvider,
    RuntimeHandsetCallIndicationProvider, RuntimeHandsetMessageProvider, RuntimeInventoryProvider,
    RuntimeLineQueryProvider, RuntimeRecordingTriggerQueue, RuntimeServiceProvider, RwLock,
    RwLockExt as _, Semaphore, Server, ServerConfig, ServerIngress, Shared, SignalingQos,
    SignalingSocket, StagedMwiSubscriptions, StationIo, StationTransport, SystemHostResolver,
    adapters, anonymous_hotline_definition, ast_log, dial_terminator_digit,
    install_reloaded_dnd_schedules, log_feature_store_error, mobility_device_registered, mpsc,
    native_channel, publish_device_features, publish_feature_changes, publish_line, raw,
    register_called_party_application, register_channel_query,
    register_codec_preference_application, register_control_actions, register_device_query,
    register_directory_http, register_feature_control_actions,
    register_handset_call_indication_application, register_handset_message_application,
    register_inventory_actions, register_line_query, register_runtime_status_actions,
    register_service_control_actions, run_call_signals, run_dnd_schedule_tick, run_events,
    shutdown_conferences, shutdown_one_way_microphones, shutdown_remote_hangups,
    uninstall_device_blf,
};
use crate::call::parking::ParkingEventSource as _;
use crate::runtime::mailbox::{RUNTIME_MAILBOX_CAPACITY, mailbox};
use crate::runtime::tls::RuntimeTlsAcceptor;
use crate::state::background::BackgroundStore;

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
    handshakes: &mut JoinSet<()>,
) -> Result<(), String> {
    let Some((listener, acceptor)) = listener.zip(acceptor) else {
        return std::future::pending().await;
    };
    let permits = secure_handshake_limiter();
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
            timezone: config.general.timezone,
            date_template: config.general.date_template.clone(),
            anonymous_hotline: anonymous_hotline_definition(&config)?,
        };
        let definitions = config.device_definitions();
        let feature_store = FeatureStore::new(AsteriskDatabase::new());
        let dnd_schedule_store =
            crate::state::dnd_schedule::DndScheduleStore::new(AsteriskDatabase::new());
        let background_store = BackgroundStore::new(AsteriskDatabase::new());
        let dnd_schedules = super::DndScheduleRegistry::load(&config, &dnd_schedule_store)
            .map_err(|error| format!("unable to restore configured DND schedules: {error}"))?;
        let feature_states = feature_store
            .load_configuration(&config)
            .map_err(|error| format!("unable to restore configured feature state: {error}"))?;
        let (schedule_runtime, schedule_mailbox) = super::dnd_schedule::schedule_channel();
        let (presence, presence_mailbox) = super::presence_owner::presence_channel();
        let (workers, worker_owner) = crate::runtime::workers::workers();
        let (parking_events_tx, mut parking_events) =
            crate::runtime::parking_events::parking_events(RUNTIME_MAILBOX_CAPACITY);
        let parking_callback = parking_events_tx.clone();
        let (control_requests_tx, control_requests) = mailbox(RUNTIME_MAILBOX_CAPACITY);
        let (service_requests_tx, service_requests) = mailbox(RUNTIME_MAILBOX_CAPACITY);
        let (background_runtime, background_mailbox) =
            super::background::background_runtime_channel();
        let (call_signals_tx, call_signals) =
            crate::runtime::call_queue::call_queue(RUNTIME_MAILBOX_CAPACITY);
        let (recording_trigger_wake, recording_triggers) =
            mpsc::channel(RECORDING_TRIGGER_WAKE_CAPACITY);
        let parking_subscription = AsteriskParking::new()
            .subscribe(move |event| {
                if let Err(error) = parking_callback.publish(event) {
                    ast_log(
                        LogLevel::Warning,
                        &format!("parking event admission failed: {error}"),
                    );
                }
            })
            .map_err(|error| format!("unable to subscribe to parking events: {error}"))?;
        let (mut server, phone, events, ingress) = Server::with_ingress(server_config, definitions)
            .map_err(|error| format!("unable to start SCCP listener: {error}"))?;
        let priority_events = server
            .enable_priority_events(RUNTIME_MAILBOX_CAPACITY)
            .map_err(|error| format!("unable to reserve SCCP completion delivery: {error}"))?;
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
        let initial_external_policy = config.general.network.external.clone();
        let external_addresses = runtime
            .block_on(runtime.spawn_blocking(move || {
                let mut cache = ExternalAddressCache::new(SystemHostResolver);
                if let Err(error) = cache.refresh(initial_external_policy.as_ref(), Instant::now())
                {
                    ast_log(
                        LogLevel::Warning,
                        &format!("unable to resolve configured external address: {error}"),
                    );
                }
                cache
            }))
            .map_err(|error| format!("external-address initialization failed: {error}"))?;
        let (external_addresses, resolver_owner) =
            crate::runtime::resolver::ExternalAddressOwner::new(
                external_addresses,
                config.general.network.external.clone(),
                |error| {
                    ast_log(
                        LogLevel::Warning,
                        &format!("unable to refresh configured external address: {error}"),
                    )
                },
            );
        let (ami_events, publication_owner) =
            crate::runtime::publication::publication_runtime(AsteriskManager::new(), |error| {
                ast_log(
                    LogLevel::Warning,
                    &format!("unable to publish a management event: {error}"),
                )
            });
        let (bridge_runtime, bridge_owner) = super::bridge_owner::bridge_runtime();
        let bridge_task = runtime.spawn(bridge_owner.run());
        let close_bridges = bridge_runtime.clone();
        let bridge_task = crate::runtime::startup::StartupTask::new(
            runtime.handle().clone(),
            bridge_task,
            move || close_bridges.close(),
        );
        let (media_runtime, media_task) = super::media_owner::MediaOwner::start()
            .map_err(|error| format!("media owner initialization failed: {error}"))?;
        let close_media = media_runtime.clone();
        let media_task =
            crate::runtime::startup::StartupThread::new(media_task, move || close_media.close());
        let (controller, controller_owner) =
            crate::runtime::controller::ownership::ControllerOwner::new(controller).map_err(
                |error| format!("unable to reserve controller completion capacity: {error}"),
            )?;
        let controller_runtime = runtime.handle().clone();
        let controller_task = std::thread::Builder::new()
            .name("sccp-controller".into())
            .spawn(move || controller_runtime.block_on(controller_owner.run()))
            .map_err(|error| format!("unable to start controller owner: {error}"))?;
        let close_controller = controller.clone();
        let controller_task =
            crate::runtime::startup::StartupThread::new(controller_task, move || {
                close_controller.close()
            });
        let (configuration_transactions, configuration_owner) =
            crate::runtime::configuration_transaction::configuration_transactions();
        let configuration_runtime =
            Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| {
                    format!("unable to create configuration transaction runtime: {error}")
                })?;
        let configuration_task = std::thread::Builder::new()
            .name("sccp-config".into())
            .spawn(move || {
                configuration_runtime.block_on(configuration_owner.run());
            })
            .map_err(|error| format!("unable to start configuration transaction owner: {error}"))?;
        let close_configuration = configuration_transactions.clone();
        let configuration_task =
            crate::runtime::startup::StartupThread::new(configuration_task, move || {
                close_configuration.close()
            });
        let shared = Arc::new(Shared {
            controller,
            event_diagnostics: RwLock::new(Arc::new(super::services::EventDiagnostics::default())),
            external_addresses,
            presence,
            parking_events: parking_events_tx,
            workers,
            config: RwLock::new(Arc::new(config)),
            config_provider,
            config_reconciliation: Arc::new(ConfigReconciliation::default()),
            configuration_transactions,
            channels: Mutex::new(HashMap::new()),
            channel_allocations: crate::runtime::resource::ResourceBinding::new(()),

            media_runtime,
            conference_destination_tasks: Mutex::new(ConferenceTaskRegistry::default()),
            bridge_runtime,

            feature_store,
            dnd_schedules: schedule_runtime,
            background_runtime,
            control_requests: control_requests_tx.clone(),
            service_requests: service_requests_tx.clone(),
            call_signals: call_signals_tx,
            recording_trigger_wake,
            pending_recording_triggers: Mutex::new(RuntimeRecordingTriggerQueue::default()),
            ami_events,
        });
        let directory_registration = register_directory_http(
            RuntimeDirectoryProvider {
                shared: Arc::downgrade(&shared),
            },
            AsteriskHttp::new(),
        )
        .map_err(|error| format!("unable to register phone directory HTTP service: {error}"))?;
        let http_registrations = vec![directory_registration];
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
        let dialplan_registrations = vec![
            device_query_registration,
            line_query_registration,
            channel_query_registration,
            codec_preference_registration,
            called_party_registration,
            handset_message_registration,
            handset_call_indication_registration,
        ];
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
        let handle = runtime.handle().clone();
        let server_shutdown = phone.clone();
        let server_task = runtime.spawn(async move {
            let secure_ingress = ingress.clone();
            let mut handshakes = JoinSet::new();
            let mut running = Box::pin(server.run());
            let server_finished = tokio::select! {
                result = &mut running => {
                    if let Err(error) = result {
                        ast_log(LogLevel::Error, &format!("SCCP server stopped: {error}"));
                    }
                    true
                }
                result = run_clear_listener(clear_listener, ingress) => {
                    if let Err(error) = result { ast_log(LogLevel::Error, &error); }
                    false
                }
                result = run_secure_listener(secure_listener, tls_acceptor, secure_ingress, &mut handshakes) => {
                    if let Err(error) = result { ast_log(LogLevel::Error, &error); }
                    false
                }
            };
            // TLS handshakes own only streams. Cancellation is complete only
            // after every task has been joined.
            handshakes.shutdown().await;
            if !server_finished {
                let (_, result) = tokio::join!(server_shutdown.shutdown(), &mut running);
                if let Err(error) = result { ast_log(LogLevel::Error, &format!("SCCP transport stopped: {error}")); }
            }
        });
        let access = Access {
            handle,
            phone,
            shared,
        };
        let schedule_access = access.clone();
        let dnd_schedule_task = runtime.spawn_blocking(move || {
            super::dnd_schedule::DndScheduleOwner::new(
                schedule_access,
                dnd_schedules,
                dnd_schedule_store,
                schedule_mailbox,
            )
            .run();
        });
        run_dnd_schedule_tick(&access);
        let worker_task = runtime.spawn(worker_owner.run());
        let presence_access = access.clone();
        let presence_task = runtime.spawn_blocking(move || {
            super::presence_owner::PresenceOwner::new(presence_access, presence_mailbox).run();
        });
        let parking_access = access.clone();
        let parking_task = runtime.spawn_blocking(move || {
            enum Turn {
                Batch(
                    Option<
                        crate::runtime::mailbox::Admitted<
                            crate::runtime::parking_events::ParkingEventBatch,
                        >,
                    >,
                ),
                Update,
            }
            loop {
                let turn = parking_access.handle.block_on(async {
                    tokio::select! {
                        biased;
                        batch = parking_events.recv() => Turn::Batch(batch),
                        _ = parking_access.shared.parking_events.updated() => Turn::Update,
                    }
                });
                match turn {
                    Turn::Batch(None) => break,
                    Turn::Batch(Some(batch)) => {
                        let (batch, _admission) = batch.into_parts();
                        parking_access.handle.block_on(async {
                            super::handle_parking_event(&parking_access, batch.first).await;
                            if let Some(latest) = batch.latest {
                                super::handle_parking_event(&parking_access, latest).await;
                            }
                        });
                    }
                    Turn::Update => {
                        if let Some((update, _admission)) =
                            parking_access.shared.parking_events.take_latest()
                        {
                            parking_access
                                .handle
                                .block_on(super::handle_parking_event(&parking_access, update));
                        }
                    }
                }
            }
        });
        let event_access = access.clone();
        let signal_access = access.clone();
        let background_access = access.clone();
        let background_task = runtime.spawn_blocking(move || {
            super::background::BackgroundRuntime::new(
                background_access,
                background_store,
                background_mailbox,
            )
            .run();
        });
        let resolver_task = runtime.spawn(async move {
            if let Err(error) = resolver_owner.run().await {
                ast_log(LogLevel::Error, &error.to_string());
            }
        });
        let publication_task = runtime.spawn(publication_owner.run());
        let (event_shutdown, event_stop) = tokio::sync::oneshot::channel();
        let (finish_event_shutdown, finish_event_stop) = tokio::sync::oneshot::channel();
        let (ordinary_drained, event_drained) = tokio::sync::oneshot::channel();
        let signal_task = runtime.spawn(run_call_signals(signal_access, call_signals));
        let event_task = runtime.spawn(run_events(
            event_access,
            events,
            priority_events,
            control_requests,
            service_requests,
            recording_triggers,
            event_stop,
            finish_event_stop,
            ordinary_drained,
        ));
        Ok(Self {
            runtime,
            controller_task: Some(controller_task.into_thread()),
            access,
            server_task,
            event_task,
            event_shutdown: Some(event_shutdown),
            finish_event_shutdown: Some(finish_event_shutdown),
            event_drained: Some(event_drained),
            signal_task,
            background_task,
            presence_task,
            parking_task,
            worker_task,
            dnd_schedule_task,
            publication_task,
            bridge_task: bridge_task.into_task(),
            media_task: Some(media_task.into_thread()),
            resolver_task,
            configuration_task: Some(configuration_task.into_thread()),
            manager_registrations,
            dialplan_registrations,
            http_registrations,
            parking_subscription,
            sorcery_registration: None,
            #[cfg(feature = "telemetry")]
            telemetry,
        })
    }

    pub fn stop(mut self) {
        self.access.shared.channel_allocations.suspend();
        self.access.shared.control_requests.close();
        self.access.shared.service_requests.close();
        // Finish the running operation and reject unstarted external work while
        // native registrations drain. Handset transport remains available.
        if let Some(shutdown) = self.event_shutdown.take() {
            let _ = shutdown.send(());
        }
        self.manager_registrations.clear();
        self.http_registrations.clear();
        self.dialplan_registrations.clear();
        let phone = self.access.phone.clone();
        self.runtime.block_on(async {
            if let Some(drained) = self.event_drained.take() {
                let _ = drained.await;
            }
            if let Err(error) = self.access.shared.background_runtime.shutdown().await {
                ast_log(
                    LogLevel::Warning,
                    &format!("unable to stop the background runtime cleanly: {error}"),
                );
            }
            if let Err(error) = self.background_task.await {
                ast_log(
                    LogLevel::Error,
                    &format!("background owner failed during unload: {error}"),
                );
            }
            shutdown_conferences(&self.access).await;
            shutdown_one_way_microphones(&self.access).await;
            self.access.shared.workers.close();
            match self.worker_task.await {
                Ok(0) => {}
                Ok(failures) => ast_log(
                    LogLevel::Error,
                    &format!("{failures} runtime workers failed before unload"),
                ),
                Err(error) => ast_log(
                    LogLevel::Error,
                    &format!("runtime worker owner failed during unload: {error}"),
                ),
            }
            self.parking_subscription.unsubscribe();
            self.access.shared.parking_events.close();
            if let Err(error) = self.parking_task.await {
                ast_log(
                    LogLevel::Error,
                    &format!("parking event owner failed during unload: {error}"),
                );
            }
            self.access.shared.call_signals.close();
            if let Err(error) = self.signal_task.await {
                ast_log(
                    LogLevel::Error,
                    &format!("call-signal owner failed during unload: {error}"),
                );
            }
            // Signal delivery can finish a remote-hangup presentation after it
            // releases the native channel. Drain tones only after that delivery.
            shutdown_remote_hangups(&self.access).await;
            self.access.shared.dnd_schedules.close();
            if let Err(error) = self.dnd_schedule_task.await {
                ast_log(
                    LogLevel::Error,
                    &format!("DND schedule owner failed during unload: {error}"),
                );
            }
            self.access.shared.media_runtime.close();
            if let Some(task) = self.media_task.take() {
                if task.join().is_err() {
                    ast_log(LogLevel::Error, "media owner failed during unload");
                }
            }
            if let Some(finish) = self.finish_event_shutdown.take() {
                let _ = finish.send(());
            }
            // Session retirement owns reserved priority slots. Stop transport
            // while the event owner can still consume those terminal updates.
            let (_, events) = tokio::join!(phone.shutdown(), self.event_task);
            if let Err(error) = events {
                ast_log(
                    LogLevel::Error,
                    &format!("event owner failed during unload: {error}"),
                );
            }
            if let Err(error) = self.server_task.await {
                ast_log(
                    LogLevel::Error,
                    &format!("SCCP transport failed during unload: {error}"),
                );
            }
            self.access.shared.bridge_runtime.close();
            if let Err(error) = self.bridge_task.await {
                ast_log(
                    LogLevel::Error,
                    &format!("bridge owner failed during unload: {error}"),
                );
            }
            self.access.shared.presence.close();
            if let Err(error) = self.presence_task.await {
                ast_log(
                    LogLevel::Error,
                    &format!("presence owner failed during unload: {error}"),
                );
            }
            self.access.shared.external_addresses.close();
            if let Err(error) = self.resolver_task.await {
                ast_log(
                    LogLevel::Error,
                    &format!("resolver owner failed during unload: {error}"),
                );
            }
            self.access.shared.ami_events.close();
            if let Err(error) = self.publication_task.await {
                ast_log(
                    LogLevel::Error,
                    &format!("publication owner failed during unload: {error}"),
                );
            }
            self.access.shared.configuration_transactions.close();
            if let Some(task) = self.configuration_task.take() {
                if task.join().is_err() {
                    ast_log(
                        LogLevel::Error,
                        "configuration transaction owner failed during unload",
                    );
                }
            }
            self.access.shared.controller.close();
            if let Some(task) = self.controller_task.take() {
                if task.join().is_err() {
                    ast_log(LogLevel::Error, "controller owner failed during unload");
                }
            }
        });
        #[cfg(feature = "telemetry")]
        if let Some(telemetry) = &mut self.telemetry {
            self.runtime.block_on(telemetry.shutdown());
        }
        drop(self.runtime);
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
        let binding = self.shared.channels.lock_unpoisoned().get(&pbx_id).cloned();
        let Some(binding) = binding else {
            return false;
        };
        match kind {
            RuntimeCallSignalKind::Hangup { handset_call_id } => {
                binding
                    .signals
                    .retire(RuntimeCallSignalKind::Hangup { handset_call_id });
                true
            }
            kind => binding
                .signals
                .try_send(
                    kind,
                    Some(Instant::now() + super::MANAGER_CONTROL_DELIVERY_TIMEOUT),
                )
                .is_ok(),
        }
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
                .controller
                .snapshot()
                .mobility()
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
                .controller
                .snapshot()
                .mobility()
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
                .controller
                .snapshot()
                .mobility()
                .binding_for_device(device_id, line_instance)
                .cloned()
        })
}

pub fn registered_device_ids(shared: &Shared) -> Vec<DeviceId> {
    shared
        .controller
        .snapshot()
        .registered_devices()
        .map(|(device, _)| device.clone())
        .collect()
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
    let transaction = access
        .shared
        .configuration_transactions
        .begin(
            crate::runtime::configuration_transaction::ConfigurationOperation::Reload,
            Instant::now() + super::MANAGER_CONTROL_TIMEOUT,
        )
        .map_err(|error| error.to_string())?;
    let next = access
        .shared
        .config_provider
        .refresh()
        .map_err(|error| error.to_string())?;
    let staged_schedules = access
        .shared
        .dnd_schedules
        .stage(Arc::new(next.clone()), &transaction)?;
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
        .controller
        .snapshot()
        .mobility()
        .has_pending_transaction()
    {
        return Err("a mobility mutation is in progress; retry reload".into());
    }
    let feature_states = access
        .shared
        .feature_store
        .load_configuration(&next)
        .map_err(|error| format!("unable to restore reloaded feature state: {error}"))?;
    let staged_mwi = StagedMwiSubscriptions::new(access, &plan.mwi_add)?;
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
    let staged_contexts = super::StagedRegistrationContexts::new(
        access,
        Arc::new(next.clone()),
        registered_after.clone(),
        Arc::clone(&previous),
        registered_before.clone(),
        &transaction,
    )
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
            let rollback = staged_contexts.abort();
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
    let controller_commit = access.shared.controller.reload_policy(
        next.clone(),
        feature_states.clone(),
        registered_after.iter().cloned().collect(),
    );
    let (registered, previous_feature_states) = match controller_commit {
        Ok(committed) => committed,
        Err(error) => {
            let handset_rollback = access.handle.block_on(
                access.phone.reconfigure_station_policy(
                    previous.device_definitions(),
                    plan.affected_devices()
                        .chain(&plan.added)
                        .cloned()
                        .collect::<Vec<_>>(),
                    anonymous_hotline_definition(&previous)?,
                ),
            );
            let native_rollback = staged_contexts.abort();
            return Err(format!(
                "controller reload commit failed: {error}; handset rollback: {}; registration rollback: {}",
                handset_rollback.is_ok(),
                native_rollback.is_ok()
            ));
        }
    };
    let registration_failure = staged_contexts.commit(affected).err();
    access
        .phone
        .set_call_answer_order(next.general.call_answer_order.into());
    access
        .shared
        .external_addresses
        .configure(next.general.network.external.clone());
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
    match access
        .handle
        .block_on(super::background::reconcile_backgrounds_after_reload(
            access,
            previous,
            registered.clone(),
        )) {
        Ok(failures) => {
            for (device, error) in failures {
                ast_log(
                    LogLevel::Warning,
                    &format!(
                        "unable to apply the reloaded background for device {device}: {error}"
                    ),
                );
            }
        }
        Err(error) => ast_log(
            LogLevel::Warning,
            &format!("unable to reconcile reloaded backgrounds: {error}"),
        ),
    }
    for device in registered {
        if let (Some(previous), Some(current)) = (
            previous_feature_states.get(&device),
            feature_states.get(&device),
        ) {
            publish_feature_changes(access, &device, previous, current);
        }
    }
    install_reloaded_dnd_schedules(access, staged_schedules, &transaction);
    if let Err(error) = access.shared.config_provider.activated(&access.config()) {
        ast_log(
            LogLevel::Warning,
            &format!("SCCP configuration converged but activation persistence failed: {error}"),
        );
    }
    match registration_failure {
        Some(error) => Err(format!(
            "configuration applied, but registration-context owner failed during commit: {error}"
        )),
        None => Ok(()),
    }
}

pub fn reconcile_mobility_after_reload(access: &Access) {
    let config = access.config();
    let reconciliation = access
        .shared
        .controller
        .reconcile_mobility((*config).clone())
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    for (target, transaction_id) in reconciliation.cancelled_prompts {
        access
            .handle
            .block_on(crate::asterisk::phone::cancel_mobility_response(
                access,
                target,
                transaction_id,
            ));
    }
    for appearance in reconciliation.removed {
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
