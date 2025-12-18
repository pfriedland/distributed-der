//! Edge agent binary: runs the per-asset tick loop and reports telemetry upstream.
//! Comments target Rust beginners and show Python-ish equivalents inline.
//!
//! Responsibilities:
//! - Maintain an outbound gRPC stream to the headend (agent initiates; no inbound listener).
//! - Run a 4s tick loop using `sim_core::tick_asset` to advance the battery state.
//! - Push telemetry upstream; receive setpoints on the same stream.
//! - Configuration comes from env vars: ASSET_ID, SITE_ID, HEADEND_GRPC, plus asset params (and optional gateway mode vars).

use std::{fs, sync::Arc, time::Duration};

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use futures_util::StreamExt;
use opcua::client::prelude::{
    AttributeService,
    AttributeId, ClientBuilder, DataValue, EndpointDescription, IdentityToken, MessageSecurityMode,
    NodeId, SecurityPolicy, UAString, UserTokenPolicy, Variant, WriteValue,
};
use serde::{Deserialize, Serialize};
use sim_core::{Asset, BessEvent, BessState, Telemetry, tick_asset};
use tokio::{sync::RwLock, task::JoinHandle};
use tokio_stream::wrappers::ReceiverStream;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;

pub mod proto {
    tonic::include_proto!("agent");
}
use proto::{
    AgentToHeadend, AssetDescriptor, DispatchAck, Event as ProtoEvent, Heartbeat, Register,
    Setpoint, agent_link_client::AgentLinkClient, agent_to_headend, headend_to_agent,
};

#[derive(Clone)]
struct AppState {
    // Multi-asset runtime keyed by asset_id so we can support gateway-style agents.
    assets: Arc<RwLock<std::collections::HashMap<Uuid, AssetRuntime>>>,
    headend_grpc: String,
    gateway_id: String,
    opcua: Option<Arc<OpcUaClient>>,
}

#[derive(Clone)]
struct AssetRuntime {
    asset: Arc<Asset>,
    sim: Arc<RwLock<BessState>>,
    setpoint_timer: Arc<RwLock<Option<JoinHandle<()>>>>,
    soc_state: Arc<RwLock<SocState>>,
    last_random_event: Arc<RwLock<Option<DateTime<Utc>>>>,
    active_events: Arc<RwLock<std::collections::HashMap<String, DateTime<Utc>>>>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing from RUST_LOG (e.g., RUST_LOG=info or debug).
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let cfg = AgentConfig::from_env()?;
    let (primary_asset_id, assets_map) = build_assets_map(&cfg);
    let opcua = cfg.opcua.clone().map(OpcUaClient::new).map(Arc::new);
    let state = AppState {
        headend_grpc: cfg.headend_grpc.clone(),
        assets: Arc::new(RwLock::new(assets_map)),
        gateway_id: cfg.gateway_id.clone(),
        opcua,
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
                        let ack_tx = tx.clone();
                        tokio::spawn(async move {
                            while let Some(Ok(msg)) = inbound.next().await {
                                if let Some(headend_to_agent::Msg::Setpoint(sp)) = msg.msg {
                                    apply_setpoint(&setpoint_state, &sp, &ack_tx).await;
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
                                                    timestamp: Utc::now().to_rfc3339_opts(
                                                        SecondsFormat::Millis,
                                                        true,
                                                    ),
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
                                let events = maybe_generate_events(&state, &asset, &snap).await;
        if let Some(opc) = state.opcua.clone() {
            if let Err(err) = opc.write_telemetry(&snap).await {
                                        tracing::warn!(
                                            "opc ua telemetry write failed: asset={} site={} err={:?}",
                                            asset.name,
                                            asset.site_name,
                                            err
                                        );
                                    }
                                }
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
                                for event in events {
                                    if tx
                                        .send(AgentToHeadend {
                                            msg: Some(agent_to_headend::Msg::Event(
                                                to_proto_event(&event),
                                            )),
                                        })
                                        .await
                                        .is_err()
                                    {
                                        tracing::warn!("event channel closed; reconnecting");
                                        stream_alive = false;
                                        break;
                                    }
                                }
                                if !stream_alive {
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
        timestamp: t.timestamp.to_rfc3339_opts(SecondsFormat::Millis, true),
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

fn to_proto_event(e: &BessEvent) -> ProtoEvent {
    ProtoEvent {
        id: e.id.to_string(),
        asset_id: e.asset_id.to_string(),
        site_id: e.site_id.to_string(),
        timestamp: e.timestamp.to_rfc3339_opts(SecondsFormat::Millis, true),
        event_type: e.event_type.clone(),
        severity: e.severity.clone(),
        message: e.message.clone(),
    }
}

async fn apply_setpoint(
    state: &AppState,
    sp: &Setpoint,
    ack_tx: &tokio::sync::mpsc::Sender<AgentToHeadend>,
) {
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
    let opcua = state.opcua.clone();
    for (alloc, rt) in allocations.iter().zip(targets.iter()) {
        set_asset_setpoint(rt, *alloc, sp.duration_s, opcua.clone()).await;
        send_dispatch_ack(ack_tx, sp, rt).await;
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

async fn send_dispatch_ack(
    tx: &tokio::sync::mpsc::Sender<AgentToHeadend>,
    sp: &Setpoint,
    rt: &AssetRuntime,
) {
    let Some(dispatch_id) = sp.dispatch_id.as_ref().filter(|s| !s.trim().is_empty()) else {
        return;
    };

    let ack = DispatchAck {
        dispatch_id: dispatch_id.clone(),
        asset_id: rt.asset.id.to_string(),
        status: "applied".to_string(),
        timestamp: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        reason: String::new(),
    };

    if tx
        .send(AgentToHeadend {
            msg: Some(agent_to_headend::Msg::DispatchAck(ack)),
        })
        .await
        .is_err()
    {
        tracing::warn!(
            "failed to send dispatch ack dispatch_id={} asset_id={}",
            dispatch_id,
            rt.asset.id
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SocState {
    BelowMin,
    InRange,
    AboveMax,
}

async fn maybe_generate_events(
    state: &AppState,
    asset: &Asset,
    snap: &Telemetry,
) -> Vec<BessEvent> {
    let mut events = Vec::new();
    if let Some(e) = maybe_soc_event(state, asset, snap).await {
        events.push(e);
    }
    events.extend(maybe_clear_random_events(state, asset, snap).await);
    if let Some(e) = maybe_random_event(state, asset, snap).await {
        events.push(e);
    }
    events
}

async fn maybe_soc_event(state: &AppState, asset: &Asset, snap: &Telemetry) -> Option<BessEvent> {
    let (min_soc, max_soc) = soc_bounds(asset);
    let eps = 1e-6;
    let next = if snap.soc_mwhr <= min_soc + eps {
        SocState::BelowMin
    } else if snap.soc_mwhr >= max_soc - eps {
        SocState::AboveMax
    } else {
        SocState::InRange
    };

    let runtime = {
        let assets = state.assets.read().await;
        assets.get(&asset.id).cloned()
    };
    let Some(rt) = runtime else {
        return None;
    };
    let mut soc_state = rt.soc_state.write().await;
    let prev = *soc_state;
    if prev == next {
        return None;
    }
    *soc_state = next;

    let (event_type, severity, message) = match (next, prev) {
        (SocState::BelowMin, _) => ("MIN_SOC_REACHED", "warning", "Min SOC reached"),
        (SocState::AboveMax, _) => ("MAX_SOC_REACHED", "warning", "Max SOC reached"),
        (SocState::InRange, SocState::BelowMin) => ("MIN_SOC_REACHED", "clear", "Min SOC cleared"),
        (SocState::InRange, SocState::AboveMax) => ("MAX_SOC_REACHED", "clear", "Max SOC cleared"),
        _ => return None,
    };

    Some(BessEvent {
        id: Uuid::new_v4(),
        asset_id: asset.id,
        site_id: asset.site_id,
        timestamp: snap.timestamp,
        event_type: event_type.to_string(),
        severity: severity.to_string(),
        message: message.to_string(),
    })
}

async fn maybe_random_event(
    state: &AppState,
    asset: &Asset,
    snap: &Telemetry,
) -> Option<BessEvent> {
    const PROB: f64 = 0.005;
    const MIN_INTERVAL_SECS: i64 = 120;

    let roll: f64 = rand::random();
    if roll > PROB {
        return None;
    }

    let runtime = {
        let assets = state.assets.read().await;
        assets.get(&asset.id).cloned()
    };
    let Some(rt) = runtime else {
        return None;
    };
    let mut last = rt.last_random_event.write().await;
    if let Some(prev) = *last {
        if (snap.timestamp - prev).num_seconds() < MIN_INTERVAL_SECS {
            return None;
        }
    }
    *last = Some(snap.timestamp);

    let templates = [
        ("RACK_FAULT", "warning", "Rack 2/4 Trouble"),
        ("HVAC_FAILED", "alarm", "HVAC Failed"),
        ("INVERTER_FAULT", "alarm", "Inverter Fault Detected"),
        ("COMMS_DEGRADED", "warning", "Site comms degraded"),
    ];
    let runtime = {
        let assets = state.assets.read().await;
        assets.get(&asset.id).cloned()
    };
    let Some(rt) = runtime else {
        return None;
    };
    let mut active = rt.active_events.write().await;
    let available: Vec<_> = templates
        .iter()
        .filter(|(t, _, _)| !active.contains_key(*t))
        .collect();
    if available.is_empty() {
        return None;
    }
    let idx = rand::random::<usize>() % available.len();
    let (event_type, severity, message) = *available[idx];
    active.insert(event_type.to_string(), snap.timestamp);

    Some(BessEvent {
        id: Uuid::new_v4(),
        asset_id: asset.id,
        site_id: asset.site_id,
        timestamp: snap.timestamp,
        event_type: event_type.to_string(),
        severity: severity.to_string(),
        message: message.to_string(),
    })
}

async fn maybe_clear_random_events(
    state: &AppState,
    asset: &Asset,
    snap: &Telemetry,
) -> Vec<BessEvent> {
    const CLEAR_AFTER_SECS: i64 = 300;
    let runtime = {
        let assets = state.assets.read().await;
        assets.get(&asset.id).cloned()
    };
    let Some(rt) = runtime else {
        return Vec::new();
    };
    let mut active = rt.active_events.write().await;
    let mut cleared = Vec::new();
    let mut to_remove = Vec::new();
    for (event_type, started) in active.iter() {
        if (snap.timestamp - *started).num_seconds() >= CLEAR_AFTER_SECS {
            to_remove.push(event_type.clone());
        }
    }
    for event_type in to_remove {
        active.remove(&event_type);
        cleared.push(BessEvent {
            id: Uuid::new_v4(),
            asset_id: asset.id,
            site_id: asset.site_id,
            timestamp: snap.timestamp,
            event_type,
            severity: "clear".to_string(),
            message: "Cleared".to_string(),
        });
    }
    cleared
}

fn soc_bounds(asset: &Asset) -> (f64, f64) {
    let cap = asset.capacity_mwhr.max(0.0);
    let min_pct = asset.min_soc_pct.clamp(0.0, 100.0);
    let max_pct = asset.max_soc_pct.clamp(0.0, 100.0);
    let min_mwhr = cap * min_pct / 100.0;
    let max_mwhr = cap * max_pct / 100.0;
    if min_mwhr <= max_mwhr {
        (min_mwhr, max_mwhr)
    } else {
        (0.0, cap)
    }
}

fn build_assets_map(cfg: &AgentConfig) -> (Uuid, std::collections::HashMap<Uuid, AssetRuntime>) {
    // Gateway Mode: load all assets for a site from YAML and serve them from one process.
    if let (Some(site_id), Some(path)) = (cfg.gateway_site_id, cfg.assets_path.as_deref()) {
        let assets = load_assets_for_site(path, site_id)
            .expect("failed to load assets for gateway site (ASSETS_PATH/GATEWAY_SITE_ID)");
        if assets.is_empty() {
            panic!(
                "no assets found for gateway site_id={} in {}",
                site_id, path
            );
        }

        tracing::info!(
            "gateway mode enabled: site_id={} assets_path={} num_assets={} asset_ids={:?}",
            site_id,
            path,
            assets.len(),
            assets.iter().map(|a| a.id).collect::<Vec<_>>()
        );

        let mut map = std::collections::HashMap::new();
        for asset in assets {
            let asset_id = asset.id;
            let sim_state = BessState {
                soc_mwhr: initial_soc_mwhr(&asset),
                current_mw: 0.0,
                setpoint_mw: 0.0,
            };
            let runtime = AssetRuntime {
                asset: Arc::new(asset),
                sim: Arc::new(RwLock::new(sim_state)),
                setpoint_timer: Arc::new(RwLock::new(None)),
                soc_state: Arc::new(RwLock::new(SocState::InRange)),
                last_random_event: Arc::new(RwLock::new(None)),
                active_events: Arc::new(RwLock::new(std::collections::HashMap::new())),
            };
            map.insert(asset_id, runtime);
        }

        // Deterministic primary asset: smallest UUID.
        let mut ids: Vec<Uuid> = map.keys().copied().collect();
        ids.sort();
        return (*ids.first().expect("empty assets map"), map);
    }

    // Legacy single-asset mode (env-driven).
    let asset = cfg.to_asset();
    let asset_id = asset.id;
    let sim_state = BessState {
        soc_mwhr: initial_soc_mwhr(&asset),
        current_mw: 0.0,
        setpoint_mw: 0.0,
    };
    let runtime = AssetRuntime {
        asset: Arc::new(asset),
        sim: Arc::new(RwLock::new(sim_state)),
        setpoint_timer: Arc::new(RwLock::new(None)),
        soc_state: Arc::new(RwLock::new(SocState::InRange)),
        last_random_event: Arc::new(RwLock::new(None)),
        active_events: Arc::new(RwLock::new(std::collections::HashMap::new())),
    };
    let mut map = std::collections::HashMap::new();
    map.insert(asset_id, runtime);
    (asset_id, map)
}

async fn snapshot_assets(state: &AppState) -> Vec<(Uuid, Arc<Asset>, Arc<RwLock<BessState>>)> {
    let assets = state.assets.read().await;
    let mut rows: Vec<(Uuid, Arc<Asset>, Arc<RwLock<BessState>>)> = assets
        .iter()
        .map(|(id, rt)| (*id, rt.asset.clone(), rt.sim.clone()))
        .collect();
    rows.sort_by_key(|(id, _, _)| *id);
    rows
}

async fn resolve_targets(state: &AppState, sp: &Setpoint) -> Vec<AssetRuntime> {
    let assets = state.assets.read().await;

    // group_id semantics are not defined in the MVP. Ignore for now to avoid surprising behavior.
    if sp.group_id.is_some() {
        tracing::warn!(
            "received setpoint with group_id but group routing is not implemented; ignoring group_id"
        );
    }

    // IMPORTANT: If asset_id is present and matches a known asset, treat this as a per-asset command,
    // even if the headend also includes site_id for context. This prevents accidental fanout in
    // Gateway Mode.
    let asset_id_str = sp.asset_id.trim();
    if !asset_id_str.is_empty() {
        if let Ok(id) = Uuid::parse_str(asset_id_str) {
            if let Some(rt) = assets.get(&id) {
                return vec![rt.clone()];
            }
        }
    }

    // If site_id is provided (and asset_id was empty/unknown), target all assets in that site.
    if let Some(site_id) = sp.site_id.as_ref().and_then(|s| Uuid::parse_str(s).ok()) {
        let mut site_assets: Vec<_> = assets
            .values()
            .filter(|rt| rt.asset.site_id == site_id)
            .cloned()
            .collect();
        site_assets.sort_by_key(|rt| rt.asset.id);
        if !site_assets.is_empty() {
            return site_assets;
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
        // Fallback: equal split if capacities are missing/zero (still clamp per asset).
        let per = mw_total / (targets.len() as f64);
        return targets
            .iter()
            .map(|rt| per.clamp(rt.asset.min_mw, rt.asset.max_mw))
            .collect();
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

        for ((val, headroom), rt) in clamped.iter_mut().zip(headrooms.iter()).zip(targets.iter()) {
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
        "setpoint allocation: mw_total={} raw_sum={} clamped_sum={} residual={} num_assets={}",
        mw_total,
        raw_sum,
        clamped_sum,
        residual,
        targets.len()
    );

    let rows: Vec<(Uuid, f64, f64, f64, f64, f64, f64)> = targets
        .iter()
        .enumerate()
        .map(|(i, rt)| {
            (
                rt.asset.id,
                rt.asset.capacity_mwhr,
                weights.get(i).copied().unwrap_or(0.0),
                raw.get(i).copied().unwrap_or(0.0),
                clamped.get(i).copied().unwrap_or(0.0),
                rt.asset.min_mw,
                rt.asset.max_mw,
            )
        })
        .collect();
    tracing::info!(
        "allocation_rows=(asset_id,cap_mwhr,weight,raw,clamped,min,max) {:?}",
        rows
    );
    clamped
}

fn initial_soc_mwhr(asset: &Asset) -> f64 {
    let cap = asset.capacity_mwhr.max(0.0);
    let min_pct = asset.min_soc_pct.clamp(0.0, 100.0);
    let max_pct = asset.max_soc_pct.clamp(0.0, 100.0);
    if min_pct <= max_pct {
        cap * (min_pct + max_pct) / 200.0
    } else {
        cap * 0.5
    }
}

#[derive(Clone, Debug)]
struct OpcUaConfig {
    endpoint: String,
    username: Option<String>,
    password: Option<String>,
    // Per-asset mapping; if missing, falls back to default_* nodes.
    setpoints: std::collections::HashMap<Uuid, NodeId>,
    telemetry_nodes: TelemetryNodes,
    // Defaults used when an asset-specific mapping is absent.
    default_setpoint: Option<NodeId>,
    default_telemetry: TelemetryNodes,
}

#[derive(Clone, Debug, Default)]
struct TelemetryNodes {
    current_mw: Option<NodeId>,
    soc_pct: Option<NodeId>,
    soc_mwhr: Option<NodeId>,
    status: Option<NodeId>,
}

impl TelemetryNodes {
    fn clone_with_defaults(&self, defaults: &TelemetryNodes) -> TelemetryNodes {
        TelemetryNodes {
            current_mw: self.current_mw.clone().or_else(|| defaults.current_mw.clone()),
            soc_pct: self.soc_pct.clone().or_else(|| defaults.soc_pct.clone()),
            soc_mwhr: self
                .soc_mwhr
                .clone()
                .or_else(|| defaults.soc_mwhr.clone()),
            status: self.status.clone().or_else(|| defaults.status.clone()),
        }
    }
}

#[derive(Deserialize, Serialize, Debug)]
struct OpcUaConfigFile {
    endpoint: String,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    setpoints: std::collections::HashMap<String, String>,
    #[serde(default)]
    default_setpoint: Option<String>,
    #[serde(default)]
    telemetry: Option<OpcUaTelemetryMap>,
    #[serde(default)]
    default_telemetry: Option<OpcUaTelemetryMap>,
}

#[derive(Deserialize, Serialize, Debug, Default)]
struct OpcUaTelemetryMap {
    #[serde(default)]
    current_mw: Option<String>,
    #[serde(default)]
    soc_pct: Option<String>,
    #[serde(default)]
    soc_mwhr: Option<String>,
    #[serde(default)]
    status: Option<String>,
}

impl OpcUaConfig {
    fn from_env() -> Result<Option<Self>> {
        // If config file is specified, load it first.
        if let Ok(path) = std::env::var("OPCUA_CONFIG_PATH") {
            if !path.trim().is_empty() {
                let contents = std::fs::read_to_string(&path)
                    .with_context(|| format!("reading OPCUA_CONFIG_PATH {}", path))?;
                let parsed: OpcUaConfigFile =
                    serde_yaml::from_str(&contents).context("parsing OPC UA config yaml")?;
                return Self::from_file(parsed);
            }
        }

        let endpoint = match std::env::var("OPCUA_ENDPOINT") {
            Ok(v) if !v.trim().is_empty() => v,
            _ => return Ok(None),
        };

        let default_setpoint = std::env::var("OPCUA_SETPOINT_NODE")
            .ok()
            .and_then(|s| s.parse().ok());

        let username = std::env::var("OPCUA_USERNAME").ok();
        let password = std::env::var("OPCUA_PASSWORD").ok();
        let default_telemetry = TelemetryNodes {
            current_mw: std::env::var("OPCUA_NODE_CURRENT_MW")
                .ok()
                .and_then(|s| s.parse().ok()),
            soc_pct: std::env::var("OPCUA_NODE_SOC_PCT")
                .ok()
                .and_then(|s| s.parse().ok()),
            soc_mwhr: std::env::var("OPCUA_NODE_SOC_MWHR")
                .ok()
                .and_then(|s| s.parse().ok()),
            status: std::env::var("OPCUA_NODE_STATUS")
                .ok()
                .and_then(|s| s.parse().ok()),
        };

        Ok(Some(Self {
            endpoint,
            username,
            password,
            setpoints: std::collections::HashMap::new(),
            telemetry_nodes: TelemetryNodes::default(),
            default_setpoint,
            default_telemetry,
        }))
    }

    fn from_file(file: OpcUaConfigFile) -> Result<Option<Self>> {
        let endpoint = file.endpoint;
        let username = file.username;
        let password = file.password;

        let mut setpoints = std::collections::HashMap::new();
        for (k, v) in file.setpoints {
            let id: Uuid = k.parse().map_err(|e| anyhow!("bad asset id {}: {e:?}", k))?;
            let node: NodeId = v
                .parse()
                .map_err(|e| anyhow!("bad setpoint node {}: {e:?}", v))?;
            setpoints.insert(id, node);
        }
        let default_setpoint = match file.default_setpoint {
            Some(s) => Some(
                s.parse()
                    .map_err(|e| anyhow!("bad default setpoint node {}: {e:?}", s))?,
            ),
            None => None,
        };

        let telemetry_nodes = Self::telemetry_map_to_nodes(&file.telemetry)?;
        let default_telemetry = Self::telemetry_map_to_nodes(&file.default_telemetry)?;

        Ok(Some(Self {
            endpoint,
            username,
            password,
            setpoints,
            telemetry_nodes,
            default_setpoint,
            default_telemetry,
        }))
    }

    fn telemetry_map_to_nodes(
        map: &Option<OpcUaTelemetryMap>,
    ) -> Result<TelemetryNodes> {
        let Some(map) = map else { return Ok(TelemetryNodes::default()); };
        let current_mw = match &map.current_mw {
            Some(s) => Some(
                s.parse()
                    .map_err(|e| anyhow!("bad telemetry current_mw node {}: {e:?}", s))?,
            ),
            None => None,
        };
        let soc_pct = match &map.soc_pct {
            Some(s) => Some(
                s.parse()
                    .map_err(|e| anyhow!("bad telemetry soc_pct node {}: {e:?}", s))?,
            ),
            None => None,
        };
        let soc_mwhr = match &map.soc_mwhr {
            Some(s) => Some(
                s.parse()
                    .map_err(|e| anyhow!("bad telemetry soc_mwhr node {}: {e:?}", s))?,
            ),
            None => None,
        };
        let status = match &map.status {
            Some(s) => Some(
                s.parse()
                    .map_err(|e| anyhow!("bad telemetry status node {}: {e:?}", s))?,
            ),
            None => None,
        };

        Ok(TelemetryNodes {
            current_mw,
            soc_pct,
            soc_mwhr,
            status,
        })
    }
}

async fn set_asset_setpoint(
    rt: &AssetRuntime,
    mw: f64,
    duration_s: Option<u64>,
    opcua: Option<Arc<OpcUaClient>>,
) {
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

    if let Some(client) = opcua {
        if let Err(err) = client.write_setpoint(rt.asset.id, mw).await {
            tracing::warn!(
                "opc ua setpoint write failed: asset={} site={} err={:?}",
                rt.asset.name,
                rt.asset.site_name,
                err
            );
        }
    }
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
    // NOTE: assets is a HashMap, so iteration order is not stable; sort by asset_id for determinism.
    let mut rts: Vec<AssetRuntime> = assets.values().cloned().collect();
    rts.sort_by_key(|rt| rt.asset.id);

    let descriptors: Vec<AssetDescriptor> = rts
        .iter()
        .map(|rt| AssetDescriptor {
            asset_id: rt.asset.id.to_string(),
            site_id: rt.asset.site_id.to_string(),
            asset_name: rt.asset.name.clone(),
            site_name: rt.asset.site_name.clone(),
            location: rt.asset.location.clone(),
        })
        .collect();

    tracing::info!(
        "sending Register: primary_asset={} site_id={} gateway_id={} num_assets={} asset_ids={:?}",
        primary.asset.id,
        primary.asset.site_id,
        state.gateway_id,
        descriptors.len(),
        rts.iter().map(|rt| rt.asset.id).collect::<Vec<_>>()
    );

    tx.send(AgentToHeadend {
        msg: Some(agent_to_headend::Msg::Register(Register {
            asset_id: primary.asset.id.to_string(),
            site_id: primary.asset.site_id.to_string(),
            asset_name: primary.asset.name.clone(),
            site_name: primary.asset.site_name.clone(),
            gateway_id: state.gateway_id.clone(),
            assets: descriptors,
        })),
    })
    .await
    .context("sending register")
}

// --- assets_test.yaml parsing (shared schema with der_headend) ---
#[derive(Debug, Deserialize)]
struct AssetsFile {
    sites: Vec<YamlSite>,
    assets: Vec<YamlAsset>,
}

#[derive(Debug, Deserialize)]
struct YamlSite {
    id: String,
    name: String,
    location: String,
}

#[derive(Debug, Deserialize)]
struct YamlAsset {
    id: String,
    name: String,
    site_id: String,
    capacity_mwhr: f64,
    max_mw: f64,
    min_mw: f64,
    #[serde(default = "default_min_soc_pct")]
    min_soc_pct: f64,
    #[serde(default = "default_max_soc_pct")]
    max_soc_pct: f64,
    efficiency: f64,
    ramp_rate_mw_per_min: f64,
}

fn default_min_soc_pct() -> f64 {
    0.0
}

fn default_max_soc_pct() -> f64 {
    100.0
}

fn load_assets_for_site(path: &str, site_id: Uuid) -> Result<Vec<Asset>> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read assets yaml at {}", path))?;
    let parsed: AssetsFile = serde_yaml::from_str(&text)
        .with_context(|| format!("failed to parse assets yaml at {}", path))?;

    let site_id_str = site_id.to_string();
    // Best-effort: use the site name/location from the sites table if present.
    let (site_name, location) = parsed
        .sites
        .iter()
        .find(|s| s.id == site_id_str)
        .map(|s| (s.name.clone(), s.location.clone()))
        .unwrap_or_else(|| ("".to_string(), "".to_string()));

    let mut out = Vec::new();
    for a in parsed
        .assets
        .into_iter()
        .filter(|a| a.site_id == site_id_str)
    {
        let asset_id: Uuid =
            a.id.parse()
                .with_context(|| format!("bad asset id {}", a.id))?;
        let site_uuid: Uuid = a
            .site_id
            .parse()
            .with_context(|| format!("bad site id {}", a.site_id))?;

        out.push(Asset {
            id: asset_id,
            site_id: site_uuid,
            site_name: site_name.clone(),
            name: a.name,
            location: location.clone(),
            capacity_mwhr: a.capacity_mwhr,
            max_mw: a.max_mw,
            min_mw: a.min_mw,
            min_soc_pct: a.min_soc_pct,
            max_soc_pct: a.max_soc_pct,
            efficiency: a.efficiency,
            ramp_rate_mw_per_min: a.ramp_rate_mw_per_min,
        });
    }

    // Stable ordering.
    out.sort_by_key(|a| a.id);
    Ok(out)
}

/// Configuration for the agent (env-driven to keep the binary simple).
#[derive(Clone, Debug)]
struct AgentConfig {
    // Legacy single-asset mode fields.
    asset_id: Uuid,
    site_id: Uuid,
    site_name: String,
    name: String,
    location: String,
    capacity_mwhr: f64,
    max_mw: f64,
    min_mw: f64,
    min_soc_pct: f64,
    max_soc_pct: f64,
    efficiency: f64,
    ramp_rate_mw_per_min: f64,

    // Connectivity.
    headend_grpc: String,

    // Gateway mode (optional).
    assets_path: Option<String>,
    gateway_site_id: Option<Uuid>,
    gateway_id: String,
    opcua: Option<OpcUaConfig>,
}

impl AgentConfig {
    fn from_env() -> Result<Self> {
        // Allow HEADEND_GRPC to be just host:port; prepend http:// if missing.
        let raw = std::env::var("HEADEND_GRPC")?;
        let headend_grpc = if raw.starts_with("http://") || raw.starts_with("https://") {
            raw
        } else {
            format!("http://{}", raw)
        };
        let opcua = OpcUaConfig::from_env()?;

        // Optional gateway mode: if GATEWAY_SITE_ID is set, load all assets for that site from ASSETS_PATH.
        let gateway_site_id: Option<Uuid> = match std::env::var("GATEWAY_SITE_ID") {
            Ok(v) if !v.trim().is_empty() => Some(v.parse()?),
            _ => None,
        };
        let assets_path: Option<String> = match std::env::var("ASSETS_PATH") {
            Ok(v) if !v.trim().is_empty() => Some(v),
            _ => None,
        };

        // If gateway mode is enabled, do NOT require legacy per-asset env vars.
        if let Some(site_uuid) = gateway_site_id {
            if assets_path.is_none() {
                anyhow::bail!("GATEWAY_SITE_ID is set but ASSETS_PATH is not set");
            }

            // gateway_id is optional; default to SITE_ID (here: the gateway site id) so it is stable.
            let gateway_id = std::env::var("GATEWAY_ID").unwrap_or_else(|_| site_uuid.to_string());

            // Legacy fields are placeholders in gateway mode; assets come from YAML.
            return Ok(Self {
                asset_id: Uuid::nil(),
                site_id: site_uuid,
                site_name: String::new(),
                name: String::new(),
                location: String::new(),
                capacity_mwhr: 0.0,
                max_mw: 0.0,
                min_mw: 0.0,
                min_soc_pct: 0.0,
                max_soc_pct: 100.0,
                efficiency: 1.0,
                ramp_rate_mw_per_min: 0.0,
                headend_grpc,
                assets_path,
                gateway_site_id: Some(site_uuid),
                gateway_id,
                opcua,
            });
        }

        // Legacy single-asset mode (env-driven): require per-asset vars.
        // Beginners: env::var reads an environment variable; we parse numbers as f64.
        let asset_id: Uuid = std::env::var("ASSET_ID")?.parse()?;
        let site_id: Uuid = std::env::var("SITE_ID")?.parse()?;
        let site_name = std::env::var("SITE_NAME")?;
        let name = std::env::var("ASSET_NAME")?;
        let location = std::env::var("ASSET_LOCATION")?;
        let capacity_mwhr: f64 = std::env::var("CAPACITY_MWHR")?.parse()?;
        let max_mw: f64 = std::env::var("MAX_MW")?.parse()?;
        let min_mw: f64 = std::env::var("MIN_MW")?.parse()?;
        let min_soc_pct: f64 = std::env::var("MIN_SOC_PCT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.0);
        let max_soc_pct: f64 = std::env::var("MAX_SOC_PCT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(100.0);
        let efficiency: f64 = std::env::var("EFFICIENCY")?.parse()?;
        let ramp_rate_mw_per_min: f64 = std::env::var("RAMP_RATE_MW_PER_MIN")?.parse()?;

        // gateway_id is optional; default to SITE_ID so it is stable.
        let gateway_id = std::env::var("GATEWAY_ID").unwrap_or_else(|_| site_id.to_string());

        Ok(Self {
            asset_id,
            site_id,
            site_name,
            name,
            location,
            capacity_mwhr,
            max_mw,
            min_mw,
            min_soc_pct,
            max_soc_pct,
            efficiency,
            ramp_rate_mw_per_min,
            headend_grpc,
            assets_path,
            gateway_site_id,
            gateway_id,
            opcua,
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
            min_soc_pct: self.min_soc_pct,
            max_soc_pct: self.max_soc_pct,
            efficiency: self.efficiency,
            ramp_rate_mw_per_min: self.ramp_rate_mw_per_min,
        }
    }
}

#[derive(Clone)]
struct OpcUaClient {
    cfg: OpcUaConfig,
}

impl OpcUaClient {
    fn new(cfg: OpcUaConfig) -> Self {
        Self { cfg }
    }

    fn telemetry_nodes_for_asset(&self, _asset_id: Uuid) -> TelemetryNodes {
        // Currently uses shared defaults only; placeholder for per-asset telemetry maps if needed.
        self.cfg
            .telemetry_nodes
            .clone_with_defaults(&self.cfg.default_telemetry)
    }

    async fn write_setpoint(&self, asset_id: Uuid, mw: f64) -> Result<()> {
        let node = self
            .cfg
            .setpoints
            .get(&asset_id)
            .cloned()
            .or_else(|| self.cfg.default_setpoint.clone());
        let Some(node_id) = node else {
            return Ok(()); // No mapping; skip silently.
        };

        let write = WriteValue {
            node_id,
            attribute_id: AttributeId::Value as u32,
            index_range: UAString::null(),
            value: DataValue {
                value: Some(Variant::Double(mw)),
                status: None,
                source_timestamp: None,
                source_picoseconds: None,
                server_timestamp: None,
                server_picoseconds: None,
            },
        };
        self.write_values(vec![write]).await
    }

    async fn write_telemetry(&self, snap: &Telemetry) -> Result<()> {
        let mut writes = Vec::new();
        let nodes = self.telemetry_nodes_for_asset(snap.asset_id);

        if let Some(node) = nodes.current_mw {
            writes.push(WriteValue {
                node_id: node,
                attribute_id: AttributeId::Value as u32,
                index_range: UAString::null(),
                value: DataValue {
                    value: Some(Variant::Double(snap.current_mw)),
                    status: None,
                    source_timestamp: None,
                    source_picoseconds: None,
                    server_timestamp: None,
                    server_picoseconds: None,
                },
            });
        }
        if let Some(node) = nodes.soc_pct {
            writes.push(WriteValue {
                node_id: node,
                attribute_id: AttributeId::Value as u32,
                index_range: UAString::null(),
                value: DataValue {
                    value: Some(Variant::Double(snap.soc_pct)),
                    status: None,
                    source_timestamp: None,
                    source_picoseconds: None,
                    server_timestamp: None,
                    server_picoseconds: None,
                },
            });
        }
        if let Some(node) = nodes.soc_mwhr {
            writes.push(WriteValue {
                node_id: node,
                attribute_id: AttributeId::Value as u32,
                index_range: UAString::null(),
                value: DataValue {
                    value: Some(Variant::Double(snap.soc_mwhr)),
                    status: None,
                    source_timestamp: None,
                    source_picoseconds: None,
                    server_timestamp: None,
                    server_picoseconds: None,
                },
            });
        }
        if let Some(node) = nodes.status {
            writes.push(WriteValue {
                node_id: node,
                attribute_id: AttributeId::Value as u32,
                index_range: UAString::null(),
                value: DataValue {
                    value: Some(Variant::String(snap.status.clone().into())),
                    status: None,
                    source_timestamp: None,
                    source_picoseconds: None,
                    server_timestamp: None,
                    server_picoseconds: None,
                },
            });
        }

        if writes.is_empty() {
            return Ok(());
        }

        self.write_values(writes).await
    }

    async fn write_values(&self, writes: Vec<WriteValue>) -> Result<()> {
        let cfg = self.cfg.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut client = ClientBuilder::new()
                .application_name("edge_agent_opcua")
                .application_uri("urn:edge_agent_opcua")
                .trust_server_certs(true)
                .create_sample_keypair(true)
                .session_retry_limit(1)
                .client()
                .ok_or_else(|| anyhow!("opc ua client build failed"))?;

            let endpoint: EndpointDescription = (
                cfg.endpoint.as_str(),
                SecurityPolicy::None.to_str(),
                MessageSecurityMode::None,
                UserTokenPolicy::anonymous(),
            )
                .into();

            let identity = match (cfg.username.clone(), cfg.password.clone()) {
                (Some(u), Some(p)) => IdentityToken::UserName(u, p),
                _ => IdentityToken::Anonymous,
            };

            let session = client
                .connect_to_endpoint(endpoint, identity)
                .map_err(|e| anyhow!("opc ua connect failed: {e:?}"))?;

            let result = {
                let session = session.write();
                session.write(&writes)
            };

            match result {
                Ok(statuses) => {
                    if !statuses.iter().all(|s| s.is_good()) {
                        return Err(anyhow!("opc ua write bad status: {:?}", statuses));
                    }
                }
                Err(status) => return Err(anyhow!("opc ua write failed: {:?}", status)),
            }

            {
                let session = session.write();
                let _ = session.disconnect();
            }

            Ok(())
        })
        .await
        .map_err(|e| anyhow!("opc ua task join error: {e}"))?
    }
}
