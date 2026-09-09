use std::error::Error;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use bridge::config::AppConfig;
use bridge::coordinator::Coordinator;
use bridge::sip::{self, StackConfig};
use sccp_protocol::{Server, ServerConfig};
use tokio::time::timeout;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "bridge.toml".into());
    let config = Arc::new(AppConfig::load(&path)?);
    let definitions = config.sccp_definitions()?;
    let sccp_advertised = match config.sccp.bind.ip() {
        IpAddr::V4(address) if !address.is_unspecified() => address,
        _ => config.media.advertised_address,
    };
    let sccp_config = ServerConfig {
        bind: config.sccp.bind,
        signaling_qos: Default::default(),
        advertised_address: sccp_advertised,
        advertised_ipv6_address: None,
        server_name: config.sccp.server_name.clone(),
        keepalive_seconds: config.sccp.keepalive_seconds,
        secondary_keepalive_seconds: config.sccp.keepalive_seconds,
        signaling_servers: Vec::new(),
        registration_tokens: Default::default(),
        firmware_version: config.sccp.firmware_version.clone(),
        dial_terminator: sccp_protocol::Digit::Pound,
        record_dial_terminator: false,
        call_answer_order: sccp_protocol::CallSelectionOrder::OldestFirst,
        timezone_offset_minutes: 0,
        timezone: None,
        date_template: Default::default(),
        anonymous_hotline: None,
    };
    let (server, sccp, sccp_events) = Server::bind(sccp_config, definitions).await?;

    let max_calls = config
        .phones
        .iter()
        .map(|phone| phone.lines.len() as u32)
        .sum::<u32>()
        .saturating_mul(4)
        .max(8);
    let (sip, sip_events) = sip::start(StackConfig {
        bind: config.sip.bind,
        advertised_address: config.sip.advertised_address,
        max_calls,
    })?;
    let coordinator = Coordinator::new(config, sccp.clone(), sccp_events, sip.clone(), sip_events)?;

    let mut server_task = tokio::spawn(server.run());
    let mut coordinator_task = tokio::spawn(coordinator.run());
    info!(config = %path, "SCCP/SIP bridge started");

    tokio::select! {
        signal = tokio::signal::ctrl_c() => signal?,
        result = &mut server_task => {
            match result {
                Ok(Ok(())) => info!("SCCP server stopped"),
                Ok(Err(error)) => error!(%error, "SCCP server failed"),
                Err(error) => error!(%error, "SCCP server task failed"),
            }
        }
        result = &mut coordinator_task => {
            match result {
                Ok(Ok(())) => info!("bridge coordinator stopped"),
                Ok(Err(error)) => error!(%error, "bridge coordinator failed"),
                Err(error) => error!(%error, "bridge coordinator task failed"),
            }
        }
    }

    info!("shutting down");
    let _ = sccp.shutdown().await;
    let _ = sip.shutdown().await;
    if !server_task.is_finished() {
        let _ = timeout(Duration::from_secs(2), &mut server_task).await;
    }
    if !coordinator_task.is_finished() {
        let _ = timeout(Duration::from_secs(2), &mut coordinator_task).await;
    }
    Ok(())
}
