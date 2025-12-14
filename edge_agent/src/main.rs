//! Edge agent binary: runs the per-asset tick loop and reports telemetry upstream.
//! Comments target Rust beginners and show Python-ish equivalents inline.
//!
//! Responsibilities:
//! - Maintain an outbound gRPC stream to the headend (agent initiates; no inbound listener).
//! - Run a 4s tick loop using `sim_core::tick_asset` to advance the battery state.
//! - Push telemetry upstream; receive setpoints on the same stream.
//! - Configuration comes from env vars/CLI: ASSET_ID, HEADEND_URL, plus asset params.

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result};
use chrono::{SecondsFormat, Utc};
use futures_util::StreamExt;
use serde::Deserialize;
use sim_core::{tick_asset, Asset, BessState, Telemetry};
use tokio::sync::RwLock;
use tokio_stream::wrappers::ReceiverStream;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;

pub mod proto {
    tonic::include_proto!("agent");
}
use proto::{
    agent_link_client::AgentLinkClient, agent_to_headend, headend_to_agent, AgentToHeadend,
    HeadendToAgent, Register, Setpoint,
};

#[derive(Clone)]
struct AppState {
    // Shared simulation state protected by an async RwLock so handlers can read
    // while the tick loop updates it. Python-ish: `state = asyncio.Lock()`.
    asset: Arc<Asset>,
    sim: Arc<RwLock<BessState>>,
    headend_grpc: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing from RUST_LOG (e.g., RUST_LOG=info or debug).
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let cfg = AgentConfig::from_env()?;
    let asset = cfg.to_asset();
    let sim_state = BessState {
        soc_mwhr: asset.capacity_mwhr * 0.5,
        current_mw: 0.0,
        setpoint_mw: 0.0,
    };

    let state = AppState {
        asset: Arc::new(asset),
        sim: Arc::new(RwLock::new(sim_state)),
        headend_grpc: cfg.headend_grpc.clone(),
    };

    // Start the tick + gRPC stream in the background.
    run_grpc_loop(state.clone()).await?;
    Ok(())
}

async fn run_grpc_loop(state: AppState) -> Result<()> {
    loop {
        match AgentLinkClient::connect(state.headend_grpc.clone()).await {
            Ok(mut client) => {
                // Channel to send outbound messages to headend.
                let (tx, rx) = tokio::sync::mpsc::channel::<AgentToHeadend>(32);
                let outbound = ReceiverStream::new(rx);

                // Start the bidirectional stream.
                match client.stream(outbound).await {
                    Ok(response) => {
                        let mut inbound = response.into_inner();

                        // Send registration first.
                        if tx
                            .send(AgentToHeadend {
                                msg: Some(agent_to_headend::Msg::Register(Register {
                                    asset_id: state.asset.id.to_string(),
                                    site_id: state.asset.site_id.to_string(),
                                    asset_name: state.asset.name.clone(),
                                    site_name: state.asset.site_name.clone(),
                                })),
                            })
                            .await
                            .is_err()
                        {
                            tracing::warn!("failed to enqueue register msg");
                            continue;
                        }

                        // Task to handle incoming setpoints.
                        let setpoint_state = state.clone();
                        tokio::spawn(async move {
                            while let Some(Ok(msg)) = inbound.next().await {
                                if let Some(headend_to_agent::Msg::Setpoint(sp)) = msg.msg {
                                    let mut sim = setpoint_state.sim.write().await;
                                    sim.setpoint_mw = sp.mw;
                                    tracing::info!(
                                        "applied setpoint from headend: mw={} asset={}",
                                        sp.mw,
                                        setpoint_state.asset.name
                                    );
                                }
                            }
                            tracing::warn!("setpoint stream ended; will reconnect");
                        });

                        // Tick loop: every 4s send telemetry upstream.
                        let interval = Duration::from_secs(4);
                        loop {
                            tokio::time::sleep(interval).await;
                            let snap = {
                                let mut sim = state.sim.write().await;
                                tick_asset(&state.asset, &mut sim, interval.as_secs_f64())
                            };
                            if tx
                                .send(AgentToHeadend {
                                    msg: Some(agent_to_headend::Msg::Telemetry(
                                        to_proto_telemetry(&snap),
                                    )),
                                })
                                .await
                                .is_err()
                            {
                                tracing::warn!("telemetry channel closed; reconnecting");
                                break;
                            }
                        }
                    }
                    Err(err) => {
                        tracing::warn!("failed to start stream: {err}");
                    }
                }
            }
            Err(err) => {
                tracing::warn!("gRPC connect failed: {err}");
            }
        }
        // Backoff before reconnecting.
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn to_proto_telemetry(t: &Telemetry) -> proto::Telemetry {
    proto::Telemetry {
        asset_id: t.asset_id.to_string(),
        site_id: t.site_id.to_string(),
        site_name: t.site_name.clone(),
        timestamp: t
            .timestamp
            .to_rfc3339_opts(SecondsFormat::Millis, true),
        soc_mwhr: t.soc_mwhr,
        soc_pct: t.soc_pct,
        capacity_mwhr: t.capacity_mwhr,
        current_mw: t.current_mw,
        setpoint_mw: t.setpoint_mw,
        max_mw: t.max_mw,
        min_mw: t.min_mw,
        status: t.status.clone(),
    }
}

/// Configuration for the agent (env-driven to keep the binary simple).
#[derive(Clone, Debug)]
struct AgentConfig {
    asset_id: Uuid,
    site_id: Uuid,
    site_name: String,
    name: String,
    location: String,
    capacity_mwhr: f64,
    max_mw: f64,
    min_mw: f64,
    efficiency: f64,
    ramp_rate_mw_per_min: f64,
    headend_grpc: String,
}

impl AgentConfig {
    fn from_env() -> Result<Self> {
        // Beginners: env::var reads an environment variable; we parse numbers as f64.
        let asset_id = std::env::var("ASSET_ID")?.parse()?;
        let site_id = std::env::var("SITE_ID")?.parse()?;
        let site_name = std::env::var("SITE_NAME")?;
        let name = std::env::var("ASSET_NAME")?;
        let location = std::env::var("ASSET_LOCATION")?;
        let capacity_mwhr = std::env::var("CAPACITY_MWHR")?.parse()?;
        let max_mw = std::env::var("MAX_MW")?.parse()?;
        let min_mw = std::env::var("MIN_MW")?.parse()?;
        let efficiency = std::env::var("EFFICIENCY")?.parse()?;
        let ramp_rate_mw_per_min = std::env::var("RAMP_RATE_MW_PER_MIN")?.parse()?;
        // Allow HEADEND_GRPC to be just host:port; prepend http:// if missing.
        let raw = std::env::var("HEADEND_GRPC")?;
        let headend_grpc = if raw.starts_with("http://") || raw.starts_with("https://") {
            raw
        } else {
            format!("http://{}", raw)
        };

        Ok(Self {
            asset_id,
            site_id,
            site_name,
            name,
            location,
            capacity_mwhr,
            max_mw,
            min_mw,
            efficiency,
            ramp_rate_mw_per_min,
            headend_grpc,
        })
    }

    fn to_asset(&self) -> Asset {
        Asset {
            id: self.asset_id,
            site_id: self.site_id,
            site_name: self.site_name.clone(),
            name: self.name.clone(),
            location: self.location.clone(),
            capacity_mwhr: self.capacity_mwhr,
            max_mw: self.max_mw,
            min_mw: self.min_mw,
            efficiency: self.efficiency,
            ramp_rate_mw_per_min: self.ramp_rate_mw_per_min,
        }
    }
}
