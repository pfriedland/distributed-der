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
use sim_core::{tick_asset, Asset, BessState, Telemetry};
use tokio::{sync::RwLock, task::JoinHandle};
use tokio_stream::wrappers::ReceiverStream;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;

pub mod proto {
    tonic::include_proto!("agent");
}
use proto::{
    agent_link_client::AgentLinkClient, agent_to_headend, headend_to_agent, AgentToHeadend,
    Register, Setpoint, Heartbeat, AssetDescriptor,
};

#[derive(Clone)]
struct AppState {
    // Multi-asset runtime keyed by asset_id so we can support gateway-style agents.
    assets: Arc<RwLock<std::collections::HashMap<Uuid, AssetRuntime>>>,
    headend_grpc: String,
}

#[derive(Clone)]
struct AssetRuntime {
    asset: Arc<Asset>,
    sim: Arc<RwLock<BessState>>,
    setpoint_timer: Arc<RwLock<Option<JoinHandle<()>>>>,
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
    let (primary_asset_id, assets_map) = build_assets_map(&cfg);
    let state = AppState {
        headend_grpc: cfg.headend_grpc.clone(),
        assets: Arc::new(RwLock::new(assets_map)),
    };

    // Start the tick + gRPC stream in the background.
    run_grpc_loop(state.clone(), primary_asset_id).await?;
    Ok(())
}

async fn run_grpc_loop(state: AppState, primary_asset: Uuid) -> Result<()> {
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
                        if send_registration(&state, primary_asset, &tx).await.is_err() {
                            tracing::warn!("failed to enqueue register msg");
                            continue;
                        }

                        // Task to handle incoming setpoints.
                        let setpoint_state = state.clone();
                        tokio::spawn(async move {
                            while let Some(Ok(msg)) = inbound.next().await {
                                if let Some(headend_to_agent::Msg::Setpoint(sp)) = msg.msg {
                                    apply_setpoint(&setpoint_state, &sp).await;
                                }
                            }
                            tracing::warn!("setpoint stream ended; will reconnect");
                        });

                        // Heartbeat task: every 30s send a heartbeat to the headend.
                        let hb_tx = tx.clone();
                        let hb_state = state.clone();
                        tokio::spawn(async move {
                            let mut alive = true;
                            while alive {
                                tokio::time::sleep(Duration::from_secs(30)).await;
                                let assets_snapshot = snapshot_assets(&hb_state).await;
                                for (asset_id, _, _) in assets_snapshot {
                                    if hb_tx
                                        .send(AgentToHeadend {
                                            msg: Some(agent_to_headend::Msg::Heartbeat(
                                                Heartbeat {
                                                    asset_id: asset_id.to_string(),
                                                    timestamp: Utc::now()
                                                        .to_rfc3339_opts(SecondsFormat::Millis, true),
                                                },
                                            )),
                                        })
                                        .await
                                        .is_err()
                                    {
                                        tracing::warn!("heartbeat channel closed; reconnecting");
                                        alive = false;
                                        break;
                                    }
                                }
                            }
                        });

                        // Tick loop: every 4s send telemetry upstream.
                        let interval = Duration::from_secs(4);
                        loop {
                            tokio::time::sleep(interval).await;
                            let assets_snapshot = snapshot_assets(&state).await;
                            let mut stream_alive = true;
                            for (_asset_id, asset, sim) in assets_snapshot {
                                let snap = {
                                    let mut sim_guard = sim.write().await;
                                    tick_asset(&asset, &mut sim_guard, interval.as_secs_f64())
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
                                    stream_alive = false;
                                    break;
                                }
                            }
                            if !stream_alive {
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

async fn apply_setpoint(state: &AppState, sp: &Setpoint) {
    // Resolve targets: site/group if provided; otherwise single asset_id.
    let targets = resolve_targets(state, sp).await;
    if targets.is_empty() {
        tracing::warn!(
            "received setpoint but found no matching assets; asset_id={} site_id={:?} group_id={:?}",
            sp.asset_id,
            sp.site_id,
            sp.group_id
        );
        return;
    }

    let mw_total = sp.mw;
    let allocations = compute_allocations(&targets, mw_total);

    // Apply allocations per asset and manage timers.
    for (alloc, rt) in allocations.iter().zip(targets.iter()) {
        set_asset_setpoint(rt, *alloc, sp.duration_s).await;
    }

    let sum: f64 = allocations.iter().sum();
    tracing::info!(
        "applied setpoint fanout: mw_total={} duration_s={:?} num_assets={} sum_applied={}",
        mw_total,
        sp.duration_s,
        targets.len(),
        sum
    );
}

fn build_assets_map(cfg: &AgentConfig) -> (Uuid, std::collections::HashMap<Uuid, AssetRuntime>) {
    let asset = cfg.to_asset();
    let asset_id = asset.id;
    let sim_state = BessState {
        soc_mwhr: asset.capacity_mwhr * 0.5,
        current_mw: 0.0,
        setpoint_mw: 0.0,
    };
    let runtime = AssetRuntime {
        asset: Arc::new(asset),
        sim: Arc::new(RwLock::new(sim_state)),
        setpoint_timer: Arc::new(RwLock::new(None)),
    };
    let mut map = std::collections::HashMap::new();
    map.insert(asset_id, runtime);
    (asset_id, map)
}

async fn snapshot_assets(
    state: &AppState,
) -> Vec<(Uuid, Arc<Asset>, Arc<RwLock<BessState>>)> {
    let assets = state.assets.read().await;
    assets
        .iter()
        .map(|(id, rt)| (*id, rt.asset.clone(), rt.sim.clone()))
        .collect()
}

async fn resolve_targets(state: &AppState, sp: &Setpoint) -> Vec<AssetRuntime> {
    let assets = state.assets.read().await;

    // If group_id is provided, include all assets (best-effort until groups are defined).
    if sp.group_id.is_some() {
        return assets.values().cloned().collect();
    }

    // If site_id is provided, target all assets in that site.
    if let Some(site_id) = sp
        .site_id
        .as_ref()
        .and_then(|s| Uuid::parse_str(s).ok())
    {
        let site_assets: Vec<_> = assets
            .values()
            .filter(|rt| rt.asset.site_id == site_id)
            .cloned()
            .collect();
        if !site_assets.is_empty() {
            return site_assets;
        }
    }

    // Fallback: single asset_id.
    if let Ok(id) = Uuid::parse_str(&sp.asset_id) {
        if let Some(rt) = assets.get(&id) {
            return vec![rt.clone()];
        }
    }
    Vec::new()
}

fn compute_allocations(targets: &[AssetRuntime], mw_total: f64) -> Vec<f64> {
    if targets.is_empty() {
        return Vec::new();
    }
    let cap_sum: f64 = targets.iter().map(|rt| rt.asset.capacity_mwhr).sum();
    if cap_sum <= f64::EPSILON {
        return vec![0.0; targets.len()];
    }

    let raw: Vec<f64> = targets
        .iter()
        .map(|rt| mw_total * rt.asset.capacity_mwhr / cap_sum)
        .collect();
    let mut clamped: Vec<f64> = raw
        .iter()
        .zip(targets.iter())
        .map(|(mw, rt)| mw.clamp(rt.asset.min_mw, rt.asset.max_mw))
        .collect();

    let weights: Vec<f64> = targets
        .iter()
        .map(|rt| rt.asset.capacity_mwhr / cap_sum)
        .collect();

    // Adjust residual if clamping changed the sum.
    let mut residual = mw_total - clamped.iter().sum::<f64>();
    let tolerance = 1e-6;
    let mut attempts = 0;
    while residual.abs() > tolerance && attempts < 3 {
        attempts += 1;
        let direction_positive = residual > 0.0;
        let mut headrooms = Vec::with_capacity(targets.len());
        for (val, rt) in clamped.iter().zip(targets.iter()) {
            let headroom = if direction_positive {
                rt.asset.max_mw - *val
            } else {
                rt.asset.min_mw - *val
            };
            headrooms.push(headroom);
        }
        let total_headroom: f64 = headrooms
            .iter()
            .filter(|h| direction_positive.then_some(**h > 0.0).unwrap_or(**h < 0.0))
            .map(|h| h.abs())
            .sum();

        if total_headroom <= f64::EPSILON {
            break;
        }

        for ((val, headroom), rt) in clamped
            .iter_mut()
            .zip(headrooms.iter())
            .zip(targets.iter())
        {
            let share = headroom.abs() / total_headroom;
            let delta = residual * share;
            let candidate = *val + delta;
            *val = candidate.clamp(rt.asset.min_mw, rt.asset.max_mw);
        }
        residual = mw_total - clamped.iter().sum::<f64>();
    }

    let clamped_sum: f64 = clamped.iter().sum();
    let raw_sum: f64 = raw.iter().sum();
    tracing::info!(
        "setpoint allocation: mw_total={} raw_sum={} clamped_sum={} residual={}",
        mw_total,
        raw_sum,
        clamped_sum,
        residual
    );
    tracing::info!("weights={:?} raw_allocations={:?}", weights, raw);
    tracing::info!("clamped_allocations={:?}", clamped);
    clamped
}

async fn set_asset_setpoint(rt: &AssetRuntime, mw: f64, duration_s: Option<u64>) {
    {
        let mut sim = rt.sim.write().await;
        sim.setpoint_mw = mw;
    }

    if let Some(handle) = rt.setpoint_timer.write().await.take() {
        handle.abort();
    }

    if let Some(dur) = duration_s {
        let duration = Duration::from_secs(dur);
        let timer_rt = rt.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(duration).await;
            let mut sim = timer_rt.sim.write().await;
            sim.setpoint_mw = 0.0;
            tracing::info!(
                "setpoint duration elapsed; reset to 0 MW for asset={} site={}",
                timer_rt.asset.name,
                timer_rt.asset.site_name
            );
        });
        *rt.setpoint_timer.write().await = Some(handle);
    }

    tracing::info!(
        "applied setpoint: asset={} site={} mw={} duration_s={:?}",
        rt.asset.name,
        rt.asset.site_name,
        mw,
        duration_s
    );
}

async fn send_registration(
    state: &AppState,
    primary_asset: Uuid,
    tx: &tokio::sync::mpsc::Sender<AgentToHeadend>,
) -> Result<()> {
    let assets = state.assets.read().await;
    let Some(primary) = assets.get(&primary_asset) else {
        anyhow::bail!("primary asset not found in map");
    };

    // Populate the repeated assets list; legacy fields use the primary asset.
    let mut descriptors = Vec::new();
    for rt in assets.values() {
        descriptors.push(AssetDescriptor {
            asset_id: rt.asset.id.to_string(),
            site_id: rt.asset.site_id.to_string(),
            asset_name: rt.asset.name.clone(),
            site_name: rt.asset.site_name.clone(),
            location: rt.asset.location.clone(),
        });
    }

    tx.send(AgentToHeadend {
        msg: Some(agent_to_headend::Msg::Register(Register {
            asset_id: primary.asset.id.to_string(),
            site_id: primary.asset.site_id.to_string(),
            asset_name: primary.asset.name.clone(),
            site_name: primary.asset.site_name.clone(),
            gateway_id: String::new(),
            assets: descriptors,
        })),
    })
    .await
    .context("sending register")
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
