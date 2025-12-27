//! Headend service for the multi-crate workspace.
//! - Loads BESS assets from YAML (with sites) into a simulator.
//! - Optionally connects to Postgres/Timescale to persist assets and telemetry.
//! - Exposes a REST-ish API for assets, live telemetry, history, and dispatch.
//! - Uses `sim_core` for shared models and the tick step so agents/headend stay aligned.
//! - Env vars: ASSETS_PATH, HEADEND_GRPC_ADDR, DATABASE_URL (optional), RESET_DB (optional).
//!
//! Comments are written for Rust beginners and include small Python-ish snippets
//! to show the same idea in another language.

use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use axum::{Router, routing::{get, post}};
use tonic::codec::CompressionEncoding;
use chrono::{DateTime, SecondsFormat, Utc};
use sim_core::{Asset, BessEvent, BessState, Dispatch, DispatchRequest, Telemetry, tick_asset};
use sqlx::PgPool;
use tokio::sync::{RwLock, mpsc};
use tokio_stream::{StreamExt as TokioStreamExt, wrappers::ReceiverStream};
use tonic::{Request, Response as GrpcResponse, Status, transport::Server};
use tower_http::trace::TraceLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;

pub mod proto {
    tonic::include_proto!("agent");
}
mod db;
mod models;
mod http;
use proto::{
    AgentToHeadend, AssetBootstrap, BootstrapRequest, BootstrapResponse, DispatchAck,
    Event as ProtoEvent, HeadendToAgent, Register, Setpoint,
    agent_link_server::{AgentLink, AgentLinkServer},
    agent_to_headend, headend_to_agent,
};

use crate::db::{
    maybe_connect_db, persist_assets, persist_event, persist_heartbeat, persist_telemetry,
    record_agent_connect, record_agent_disconnect,
};
use crate::models::{AssetsFile, DispatchSnapshotRow, SiteCfg, TelemetrySeed, TelemetrySnapshotRow};
use crate::http::{
    create_dispatch, create_event, get_asset, health, history_heartbeats, history_telemetry,
    ingest_telemetry, latest_heartbeat, latest_telemetry, list_agents, list_assets,
    list_dispatch_history, list_event_history, list_sites, ui_home,
};

#[derive(Clone)]
struct AppState {
    // Shared simulator behind an async RwLock so many readers / single writer.
    // Python-ish: `sim = Simulator()` protected by an async lock.
    sim: Arc<RwLock<Simulator>>,
    // Optional Postgres pool; None means run in-memory only.
    db: Option<PgPool>,
    // Latest telemetry snapshots pushed by agents (agent → headend).
    latest: Arc<RwLock<HashMap<Uuid, Telemetry>>>,
    // Registry of agent gRPC senders keyed by asset id so we can push setpoints.
    agent_streams: Arc<RwLock<HashMap<Uuid, AgentStream>>>,
    // Pending setpoints if an agent is offline; delivered on next connect.
    pending_setpoints: Arc<RwLock<HashMap<Uuid, Dispatch>>>,
    // Tracks SOC state to avoid spamming events.
    soc_state: Arc<RwLock<HashMap<Uuid, SocState>>>,
}

/// Connection info for a live agent stream.
#[derive(Clone)]
struct AgentStream {
    tx: mpsc::Sender<HeadendToAgent>,
    peer: String,
    asset_name: String,
    site_name: String,
    #[allow(dead_code)]
    site_id: Uuid,
    connected_at: DateTime<Utc>,
}

#[derive(Clone)]
struct Simulator {
    assets: HashMap<Uuid, Asset>,
    state: HashMap<Uuid, BessState>,
    dispatches: HashMap<Uuid, Dispatch>,
}

impl Simulator {
    fn from_assets(assets: Vec<Asset>) -> Self {
        let mut state = HashMap::new();
        let mut dispatches = HashMap::new();
        for asset in &assets {
            // Start each asset at mid-range SOC and zero MW.
            let soc = initial_soc_mwhr(asset);
            state.insert(
                asset.id,
                BessState {
                    soc_mwhr: soc,
                    current_mw: 0.0,
                    setpoint_mw: 0.0,
                },
            );
            dispatches.insert(
                asset.id,
                Dispatch {
                    id: Uuid::new_v4(),
                    asset_id: asset.id,
                    mw: 0.0,
                    duration_s: None,
                    status: "accepted".to_string(),
                    reason: None,
                    submitted_at: Utc::now(),
                    clamped: false,
                },
            );
        }
        Self {
            assets: assets.into_iter().map(|a| (a.id, a)).collect(),
            state,
            dispatches,
        }
    }

    fn assets(&self) -> Vec<Asset> {
        self.assets.values().cloned().collect()
    }

    fn set_dispatch(
        &mut self,
        req: DispatchRequest,
        soc_mwhr_override: Option<f64>,
    ) -> Result<Dispatch, anyhow::Error> {
        // Validate the asset exists.
        let asset = self
            .assets
            .get(&req.asset_id)
            .context("asset not found for dispatch")?;
        // Reject dispatches that push beyond SOC bounds.
        if let Some(state) = self.state.get(&req.asset_id) {
            let (min_soc, max_soc) = soc_bounds(asset);
            let eps = 1e-6;
            let soc_mwhr = soc_mwhr_override.unwrap_or(state.soc_mwhr);
            if req.mw > 0.0 && soc_mwhr <= min_soc + eps {
                anyhow::bail!("dispatch rejected: at min SOC");
            }
            if req.mw < 0.0 && soc_mwhr >= max_soc - eps {
                anyhow::bail!("dispatch rejected: at max SOC");
            }
        }
        // Basic limit check: clamp MW to min/max.
        if req.mw > asset.max_mw || req.mw < asset.min_mw {
            anyhow::bail!("mw out of bounds for asset");
        }
        // Update the setpoint in the per-asset state.
        if let Some(state) = self.state.get_mut(&req.asset_id) {
            state.setpoint_mw = req.mw;
        }
        // Record the dispatch in memory.
        let dispatch = Dispatch {
            id: Uuid::new_v4(),
            asset_id: req.asset_id,
            mw: req.mw,
            duration_s: req.duration_s,
            status: "accepted".to_string(),
            reason: None,
            submitted_at: Utc::now(),
            clamped: req.clamped,
        };
        self.dispatches.insert(req.asset_id, dispatch.clone());
        Ok(dispatch)
    }

    #[allow(dead_code)]
    fn tick(&mut self, dt_secs: f64) -> Vec<Telemetry> {
        // Advance each asset and collect telemetry snapshots.
        let mut snaps = Vec::new();
        for (id, state) in self.state.iter_mut() {
            if let Some(asset) = self.assets.get(id) {
                snaps.push(tick_asset(asset, state, dt_secs));
            }
        }
        snaps
    }
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


async fn load_assets_from_yaml() -> Result<Vec<Asset>> {
    // Read the YAML file and map site/asset records into `sim_core::Asset`.
    // Python-ish: `data = yaml.safe_load(open(path))`
    let (_path, raw) = read_assets_file().await?;
    let parsed: AssetsFile = serde_yaml::from_str(&raw).context("parsing assets.yaml")?;
    let sites: HashMap<Uuid, SiteCfg> = parsed.sites.into_iter().map(|s| (s.id, s)).collect();

    let mut assets = Vec::new();
    for cfg in parsed.assets {
        let site = sites
            .get(&cfg.site_id)
            .with_context(|| format!("site not found for asset {}", cfg.name))?;
        assets.push(Asset {
            id: cfg.id,
            site_id: cfg.site_id,
            site_name: site.name.clone(),
            name: cfg.name,
            location: site.location.clone(),
            capacity_mwhr: cfg.capacity_mwhr,
            max_mw: cfg.max_mw,
            min_mw: cfg.min_mw,
            min_soc_pct: cfg.min_soc_pct,
            max_soc_pct: cfg.max_soc_pct,
            efficiency: cfg.efficiency,
            ramp_rate_mw_per_min: cfg.ramp_rate_mw_per_min,
        });
    }
    Ok(assets)
}

async fn hydrate_sim_state(sim: &mut Simulator, db: &PgPool) -> Result<()> {
    let rows = sqlx::query_as::<_, TelemetrySeed>(
        r#"
        SELECT DISTINCT ON (asset_id)
            asset_id,
            soc_mwhr,
            current_mw,
            setpoint_mw
        FROM telemetry
        ORDER BY asset_id, ts DESC
        "#,
    )
    .fetch_all(db)
    .await
    .context("querying latest telemetry for state hydration")?;

    let mut updated = 0usize;
    for row in rows {
        let Some(state) = sim.state.get_mut(&row.asset_id) else {
            continue;
        };
        let Some(asset) = sim.assets.get(&row.asset_id) else {
            continue;
        };
        state.soc_mwhr = clamp_soc(asset, row.soc_mwhr);
        state.current_mw = row.current_mw.clamp(asset.min_mw, asset.max_mw);
        state.setpoint_mw = row.setpoint_mw.clamp(asset.min_mw, asset.max_mw);
        updated += 1;
    }
    if updated > 0 {
        tracing::info!("hydrated simulator state from telemetry rows={}", updated);
    }
    Ok(())
}

fn clamp_soc(asset: &Asset, soc_mwhr: f64) -> f64 {
    let cap = asset.capacity_mwhr.max(0.0);
    let min_pct = asset.min_soc_pct.clamp(0.0, 100.0);
    let max_pct = asset.max_soc_pct.clamp(0.0, 100.0);
    let min_mwhr = cap * min_pct / 100.0;
    let max_mwhr = cap * max_pct / 100.0;
    if min_mwhr <= max_mwhr {
        soc_mwhr.clamp(min_mwhr, max_mwhr)
    } else {
        soc_mwhr.clamp(0.0, cap)
    }
}

async fn read_assets_file() -> Result<(PathBuf, String)> {
    // Allow an override via ASSETS_PATH, otherwise try common relative locations.
    let candidates = if let Ok(p) = std::env::var("ASSETS_PATH") {
        vec![PathBuf::from(p)]
    } else {
        vec![
            PathBuf::from("assets.yaml"),
            PathBuf::from("../assets.yaml"),
        ]
    };

    for path in candidates {
        if let Ok(raw) = tokio::fs::read_to_string(&path).await {
            tracing::info!("loaded assets from {}", path.display());
            return Ok((path, raw));
        }
    }

    anyhow::bail!(
        "assets.yaml not found. Set ASSETS_PATH or place assets.yaml in the headend cwd or workspace root."
    )
}

#[tokio::main]
async fn main() -> Result<()> {
    // Set up tracing so we get logs on stdout. Use `RUST_LOG=debug` for more detail.
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Load assets from YAML (with beginner-friendly error messages).
    let assets = load_assets_from_yaml().await?;
    let db = maybe_connect_db().await?;
    if let Some(db) = db.as_ref() {
        // Persist asset metadata so history queries can include names/locations.
        persist_assets(db, &assets).await?;
    }
    let mut simulator = Simulator::from_assets(assets.clone());
    if let Some(db) = db.as_ref() {
        hydrate_sim_state(&mut simulator, db).await?;
    }

    // Wrap shared state in Arc<RwLock> for Axum handlers and the tick loop.
    let state = AppState {
        sim: Arc::new(RwLock::new(simulator)),
        db,
        latest: Arc::new(RwLock::new(HashMap::new())),
        agent_streams: Arc::new(RwLock::new(HashMap::new())),
        pending_setpoints: Arc::new(RwLock::new(HashMap::new())),
        soc_state: Arc::new(RwLock::new(HashMap::new())),
    };

    // Spawn the tick loop on a background task.
    //spawn_tick_loop(state.clone());

    // Build the Axum router.
    let app = Router::new()
        .route("/", get(ui_home))
        .route("/health", get(health))
        .route("/assets", get(list_assets))
        .route("/assets/{id}", get(get_asset))
        .route("/sites", get(list_sites))
        .route("/agents", get(list_agents))
        .route("/telemetry/{id}", get(latest_telemetry))
        .route("/telemetry/{id}/history", get(history_telemetry))
        .route("/telemetry", post(ingest_telemetry))
        .route("/dispatch", post(create_dispatch))
        .route("/dispatch/history", get(list_dispatch_history))
        .route("/events", post(create_event))
        .route("/events/{id}/history", get(list_event_history))
        .route("/heartbeat/{id}", get(latest_heartbeat))
        .route("/heartbeat/{id}/history", get(history_heartbeats))
        .with_state(state.clone())
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(|req: &axum::http::Request<_>| {
                    let headers = req.headers();
                    tracing::info_span!(
                        "http_request",
                        method = %req.method(),
                        path = %req.uri().path(),
                        user_agent = ?headers.get(axum::http::header::USER_AGENT),
                        x_request_id = ?headers.get("x-request-id"),
                    )
                })
                .on_request(|req: &axum::http::Request<_>, _span: &tracing::Span| {
                    tracing::info!(
                        "incoming request method={} path={}",
                        req.method(),
                        req.uri().path()
                    );
                })
                .on_response(
                    |res: &axum::http::Response<_>,
                     latency: std::time::Duration,
                     span: &tracing::Span| {
                        span.record("status", &tracing::field::display(res.status()));
                        tracing::info!(
                            parent: span,
                            status = %res.status(),
                            latency_ms = %latency.as_millis(),
                            "response sent"
                        );
                    },
                )
                .on_failure(
                    |error: tower_http::classify::ServerErrorsFailureClass,
                     latency: std::time::Duration,
                     span: &tracing::Span| {
                        tracing::error!(
                            parent: span,
                            error = %error,
                            latency_ms = %latency.as_millis(),
                            "request failed"
                        );
                    },
                ),
        );

    // Start HTTP and gRPC servers concurrently.
    // Default HTTP bind can be overridden by HEADEND_HTTP_ADDR (e.g., 0.0.0.0:3001)
    let http_addr: SocketAddr = std::env::var("HEADEND_HTTP_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:3001".to_string())
        .parse()
        .expect("invalid HEADEND_HTTP_ADDR");
    // Default gRPC port can be overridden by HEADEND_GRPC_ADDR (e.g., 0.0.0.0:50070)
    let grpc_addr: SocketAddr = std::env::var("HEADEND_GRPC_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:50051".to_string())
        .parse()
        .expect("invalid HEADEND_GRPC_ADDR");

    let grpc_state = state.clone();
    let http = axum::serve(tokio::net::TcpListener::bind(http_addr).await?, app);
    let grpc = Server::builder()
        .add_service(
            AgentLinkServer::new(GrpcApi { state: grpc_state })
                .accept_compressed(CompressionEncoding::Gzip)
                .send_compressed(CompressionEncoding::Gzip),
        )
        .serve(grpc_addr);

    tracing::info!(
        "Headend HTTP on http://{}, gRPC on {}",
        http_addr,
        grpc_addr
    );

    // Normalize error types so try_join can return anyhow::Error.
    tokio::try_join!(async { http.await.map_err(anyhow::Error::from) }, async {
        grpc.await.map_err(anyhow::Error::from)
    })?;
    Ok(())
}

#[allow(dead_code)]
fn spawn_tick_loop(state: AppState) {
    // Basic loop: every 4 seconds run the tick and optionally persist telemetry.
    tokio::spawn(async move {
        let mut last = Instant::now();
        loop {
            let now = Instant::now();
            let dt = now.duration_since(last);
            last = now;
            let dt_secs = dt.as_secs_f64().max(1.0); // avoid zero

            // Acquire a write lock on the simulator to advance all assets.
            let mut sim = state.sim.write().await;
            let snaps = sim.tick(dt_secs);
            drop(sim);

            // Persist telemetry if DB is configured; ignore errors for now.
            if let Some(db) = state.db.as_ref() {
                if let Err(err) = persist_telemetry(db, &snaps).await {
                    tracing::warn!("failed to persist telemetry: {err}");
                }
            }
            tokio::time::sleep(Duration::from_secs(4)).await;
        }
    });
}

#[derive(Clone)]
struct GrpcApi {
    state: AppState,
}

#[tonic::async_trait]
impl AgentLink for GrpcApi {
    type StreamStream =
        Pin<Box<dyn futures_util::Stream<Item = Result<HeadendToAgent, Status>> + Send>>;

    async fn bootstrap(
        &self,
        request: Request<BootstrapRequest>,
    ) -> Result<GrpcResponse<BootstrapResponse>, Status> {
        let req = request.into_inner();
        let mut asset_ids = Vec::new();
        for id in req.asset_ids {
            match Uuid::parse_str(&id) {
                Ok(uuid) => asset_ids.push(uuid),
                Err(_) => {
                    return Err(Status::invalid_argument(format!(
                        "invalid asset_id {}",
                        id
                    )));
                }
            }
        }
        let resp = build_bootstrap_response(&self.state, &asset_ids)
            .await
            .map_err(|e| Status::internal(format!("bootstrap failed: {e}")))?;
        Ok(GrpcResponse::new(resp))
    }

    async fn stream(
        &self,
        request: Request<tonic::Streaming<AgentToHeadend>>,
    ) -> Result<GrpcResponse<Self::StreamStream>, Status> {
        let peer_ip = request
            .remote_addr()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "<unknown>".into());

        let mut inbound = request.into_inner();

        // Each connection gets its own channel for outbound setpoints.
        let (tx, rx) = mpsc::channel::<HeadendToAgent>(32);

        let state = self.state.clone();
        tokio::spawn(async move {
            let mut asset_ids: Vec<(Uuid, String, String)> = Vec::new(); // (asset_id, asset_name, site_name)
            while let Some(msg) = tokio_stream::StreamExt::next(&mut inbound).await {
                match msg {
                    Ok(AgentToHeadend {
                        msg:
                            Some(agent_to_headend::Msg::Register(Register {
                                asset_id: id,
                                site_id,
                                asset_name,
                                site_name,
                                assets,
                                ..
                            })),
                    }) => {
                        asset_ids.clear();

                        // Prefer multi-asset registrations; fall back to legacy single-asset fields.
                        let mut registered_any = false;
                        for desc in assets {
                            if let Ok(uuid) = Uuid::parse_str(&desc.asset_id) {
                                let asset_name = if desc.asset_name.is_empty() {
                                    asset_name.clone()
                                } else {
                                    desc.asset_name.clone()
                                };
                                let site_name = if desc.site_name.is_empty() {
                                    site_name.clone()
                                } else {
                                    desc.site_name.clone()
                                };
                                let site_uuid = if !desc.site_id.is_empty() {
                                    Uuid::parse_str(&desc.site_id).unwrap_or_else(|_| Uuid::nil())
                                } else {
                                    Uuid::parse_str(&site_id).unwrap_or_else(|_| Uuid::nil())
                                };

                                let connected_at = Utc::now();
                                state.agent_streams.write().await.insert(
                                    uuid,
                                    AgentStream {
                                        tx: tx.clone(),
                                        peer: peer_ip.clone(),
                                        asset_name: asset_name.clone(),
                                        site_name: site_name.clone(),
                                        site_id: site_uuid,
                                        connected_at,
                                    },
                                );
                                asset_ids.push((uuid, asset_name.clone(), site_name.clone()));
                                registered_any = true;

                                tracing::info!(
                                    "agent connected asset_id={} asset_name={} site_name={} peer={}",
                                    uuid,
                                    asset_name,
                                    site_name,
                                    peer_ip,
                                );

                                if let Some(db) = state.db.as_ref() {
                                    if let Err(err) = record_agent_connect(
                                        db,
                                        uuid,
                                        &peer_ip,
                                        &asset_name,
                                        &site_name,
                                        connected_at,
                                    )
                                    .await
                                    {
                                        tracing::warn!("failed to record agent connect: {err}");
                                    }
                                }

                                if let Some(pending) =
                                    state.pending_setpoints.write().await.remove(&uuid)
                                {
                                    let _ = tx
                                        .send(HeadendToAgent {
                                            msg: Some(headend_to_agent::Msg::Setpoint(Setpoint {
                                                asset_id: pending.asset_id.to_string(),
                                                mw: pending.mw,
                                                duration_s: pending.duration_s.map(|d| d as u64),
                                                site_id: Some(site_uuid.to_string()),
                                                group_id: None,
                                                dispatch_id: Some(pending.id.to_string()),
                                            })),
                                        })
                                        .await;
                                }
                            }
                        }

                        // Legacy single-asset registration.
                        if !registered_any {
                            if let Ok(uuid) = Uuid::parse_str(&id) {
                                let site_uuid =
                                    Uuid::parse_str(&site_id).unwrap_or_else(|_| Uuid::nil());
                                let connected_at = Utc::now();
                                state.agent_streams.write().await.insert(
                                    uuid,
                                    AgentStream {
                                        tx: tx.clone(),
                                        peer: peer_ip.clone(),
                                        asset_name: asset_name.clone(),
                                        site_name: site_name.clone(),
                                        site_id: site_uuid,
                                        connected_at,
                                    },
                                );
                                asset_ids.push((uuid, asset_name.clone(), site_name.clone()));

                                tracing::info!(
                                    "agent connected asset_id={} asset_name={} site_name={} peer={}",
                                    uuid,
                                    asset_name,
                                    site_name,
                                    peer_ip,
                                );

                                if let Some(db) = state.db.as_ref() {
                                    if let Err(err) = record_agent_connect(
                                        db,
                                        uuid,
                                        &peer_ip,
                                        &asset_name,
                                        &site_name,
                                        connected_at,
                                    )
                                    .await
                                    {
                                        tracing::warn!("failed to record agent connect: {err}");
                                    }
                                }

                                if let Some(pending) =
                                    state.pending_setpoints.write().await.remove(&uuid)
                                {
                                    let _ = tx
                                        .send(HeadendToAgent {
                                            msg: Some(headend_to_agent::Msg::Setpoint(Setpoint {
                                                asset_id: pending.asset_id.to_string(),
                                                mw: pending.mw,
                                                duration_s: pending.duration_s.map(|d| d as u64),
                                                site_id: Some(site_uuid.to_string()),
                                                group_id: None,
                                                dispatch_id: Some(pending.id.to_string()),
                                            })),
                                        })
                                        .await;
                                }
                            }
                        }
                    }
                    Ok(AgentToHeadend {
                        msg: Some(agent_to_headend::Msg::Telemetry(t)),
                    }) => {
                        if let Err(err) = handle_agent_telemetry(&state, t).await {
                            tracing::warn!("telemetry ingest failed: {err}");
                        }
                    }
                    Ok(AgentToHeadend {
                        msg: Some(agent_to_headend::Msg::Heartbeat(hb)),
                    }) => {
                        if let Err(err) = handle_agent_heartbeat(&state, hb).await {
                            tracing::warn!("heartbeat handling failed: {err}");
                        }
                    }
                    Ok(AgentToHeadend {
                        msg: Some(agent_to_headend::Msg::DispatchAck(ack)),
                    }) => {
                        if let Err(err) = handle_dispatch_ack(&state, ack).await {
                            tracing::warn!("dispatch ack handling failed: {err}");
                        }
                    }
                    Ok(AgentToHeadend {
                        msg: Some(agent_to_headend::Msg::Event(evt)),
                    }) => {
                        if let Err(err) = handle_agent_event(&state, evt).await {
                            tracing::warn!("event handling failed: {err}");
                        }
                    }
                    Ok(_) => {}
                    Err(err) => {
                        let primary = asset_ids.get(0).map(|(id, _, _)| *id);
                        tracing::info!(
                            "agent stream closed/errored (asset={:?}, peer={}): {err}",
                            primary,
                            peer_ip
                        );
                        break;
                    }
                }
            }

            // Clean up on disconnect.
            if !asset_ids.is_empty() {
                for (id, _asset_name, _site_name) in asset_ids {
                    if let Some(agent) = state.agent_streams.write().await.remove(&id) {
                        tracing::info!(
                            "agent disconnected asset_id={} asset_name={} site_name={} peer={}",
                            id,
                            agent.asset_name,
                            agent.site_name,
                            peer_ip
                        );
                    } else {
                        tracing::info!("agent disconnected asset_id={} peer={}", id, peer_ip);
                    }
                    if let Some(db) = state.db.as_ref() {
                        if let Err(err) = record_agent_disconnect(db, id).await {
                            tracing::warn!("failed to record agent disconnect: {err}");
                        }
                    }
                }
            }
        });

        let outbound = ReceiverStream::new(rx).map(Ok);
        Ok(GrpcResponse::new(Box::pin(outbound)))
    }
}

async fn handle_agent_telemetry(state: &AppState, t: proto::Telemetry) -> Result<()> {
    let asset_id = Uuid::parse_str(&t.asset_id)?;
    let site_id = Uuid::parse_str(&t.site_id)?;
    let ts = DateTime::parse_from_rfc3339(&t.timestamp)
        .map(|dt| dt.with_timezone(&Utc))
        .context("parsing telemetry timestamp")?;

    let snap = Telemetry {
        asset_id,
        site_id,
        site_name: t.site_name.clone(),
        timestamp: ts,
        soc_mwhr: t.soc_mwhr,
        soc_pct: t.soc_pct,
        capacity_mwhr: t.capacity_mwhr,
        current_mw: t.current_mw,
        setpoint_mw: t.setpoint_mw,
        max_mw: t.max_mw,
        min_mw: t.min_mw,
        status: t.status.clone(),
        voltage_v: t.voltage_v,
        current_a: t.current_a,
        dc_bus_v: t.dc_bus_v,
        dc_bus_a: t.dc_bus_a,
        temperature_cell_f: t.temperature_cell_f,
        temperature_module_f: t.temperature_module_f,
        temperature_ambient_f: t.temperature_ambient_f,
        soh_pct: t.soh_pct,
        cycle_count: t.cycle_count,
        energy_in_mwh: t.energy_in_mwh,
        energy_out_mwh: t.energy_out_mwh,
        available_charge_kw: t.available_charge_kw,
        available_discharge_kw: t.available_discharge_kw,
    };

    {
        let mut latest = state.latest.write().await;
        latest.insert(asset_id, snap.clone());
    }

    if let Some(db) = state.db.as_ref() {
        persist_telemetry(db, &[snap.clone()]).await?;
        maybe_emit_soc_event(state, db, &snap).await?;
    }

    Ok(())
}

fn telemetry_to_proto(snap: &Telemetry) -> proto::Telemetry {
    proto::Telemetry {
        asset_id: snap.asset_id.to_string(),
        site_id: snap.site_id.to_string(),
        site_name: snap.site_name.clone(),
        timestamp: snap
            .timestamp
            .to_rfc3339_opts(SecondsFormat::Millis, true),
        soc_mwhr: snap.soc_mwhr,
        soc_pct: snap.soc_pct,
        capacity_mwhr: snap.capacity_mwhr,
        current_mw: snap.current_mw,
        setpoint_mw: snap.setpoint_mw,
        max_mw: snap.max_mw,
        min_mw: snap.min_mw,
        status: snap.status.clone(),
        voltage_v: snap.voltage_v,
        current_a: snap.current_a,
        dc_bus_v: snap.dc_bus_v,
        dc_bus_a: snap.dc_bus_a,
        temperature_cell_f: snap.temperature_cell_f,
        temperature_module_f: snap.temperature_module_f,
        temperature_ambient_f: snap.temperature_ambient_f,
        soh_pct: snap.soh_pct,
        cycle_count: snap.cycle_count,
        energy_in_mwh: snap.energy_in_mwh,
        energy_out_mwh: snap.energy_out_mwh,
        available_charge_kw: snap.available_charge_kw,
        available_discharge_kw: snap.available_discharge_kw,
    }
}

fn setpoint_from_dispatch(dispatch: &Dispatch, asset: &Asset) -> Setpoint {
    Setpoint {
        asset_id: dispatch.asset_id.to_string(),
        mw: dispatch.mw,
        duration_s: dispatch.duration_s.map(|v| v as u64),
        site_id: Some(asset.site_id.to_string()),
        group_id: None,
        dispatch_id: Some(dispatch.id.to_string()),
    }
}

fn setpoint_from_state(asset: &Asset, state: &BessState) -> Option<Setpoint> {
    if state.setpoint_mw.abs() <= f64::EPSILON {
        return None;
    }
    Some(Setpoint {
        asset_id: asset.id.to_string(),
        mw: state.setpoint_mw,
        duration_s: None,
        site_id: Some(asset.site_id.to_string()),
        group_id: None,
        dispatch_id: None,
    })
}

async fn build_bootstrap_response(
    state: &AppState,
    asset_ids: &[Uuid],
) -> Result<BootstrapResponse> {
    let mut telemetry_map: HashMap<Uuid, Telemetry> = HashMap::new();
    {
        let latest = state.latest.read().await;
        for id in asset_ids {
            if let Some(snap) = latest.get(id) {
                telemetry_map.insert(*id, snap.clone());
            }
        }
    }

    if let Some(db) = state.db.as_ref() {
        let missing: Vec<Uuid> = asset_ids
            .iter()
            .filter(|id| !telemetry_map.contains_key(id))
            .cloned()
            .collect();
        if !missing.is_empty() {
            let rows = sqlx::query_as::<_, TelemetrySnapshotRow>(
                r#"
                SELECT DISTINCT ON (asset_id)
                    asset_id,
                    site_id,
                    site_name,
                    ts,
                    soc_mwhr,
                    soc_pct,
                    capacity_mwhr,
                    current_mw,
                    setpoint_mw,
                    max_mw,
                    min_mw,
                    status,
                    voltage_v,
                    current_a,
                    dc_bus_v,
                    dc_bus_a,
                    temperature_cell_f,
                    temperature_module_f,
                    temperature_ambient_f,
                    soh_pct,
                    cycle_count,
                    energy_in_mwh,
                    energy_out_mwh,
                    available_charge_kw,
                    available_discharge_kw
                FROM telemetry
                WHERE asset_id = ANY($1)
                ORDER BY asset_id, ts DESC
                "#,
            )
            .bind(&missing)
            .fetch_all(db)
            .await
            .context("querying latest telemetry for bootstrap")?;
            for row in rows {
                telemetry_map.insert(
                    row.asset_id,
                    Telemetry {
                        asset_id: row.asset_id,
                        site_id: row.site_id,
                        site_name: row.site_name,
                        timestamp: row.ts,
                        soc_mwhr: row.soc_mwhr,
                        soc_pct: row.soc_pct,
                        capacity_mwhr: row.capacity_mwhr,
                        current_mw: row.current_mw,
                        setpoint_mw: row.setpoint_mw,
                        max_mw: row.max_mw,
                        min_mw: row.min_mw,
                        status: row.status,
                        voltage_v: row.voltage_v,
                        current_a: row.current_a,
                        dc_bus_v: row.dc_bus_v,
                        dc_bus_a: row.dc_bus_a,
                        temperature_cell_f: row.temperature_cell_f,
                        temperature_module_f: row.temperature_module_f,
                        temperature_ambient_f: row.temperature_ambient_f,
                        soh_pct: row.soh_pct,
                        cycle_count: row.cycle_count as u64,
                        energy_in_mwh: row.energy_in_mwh,
                        energy_out_mwh: row.energy_out_mwh,
                        available_charge_kw: row.available_charge_kw,
                        available_discharge_kw: row.available_discharge_kw,
                    },
                );
            }
        }
    }

    let mut setpoint_map: HashMap<Uuid, Setpoint> = HashMap::new();
    let sim = state.sim.read().await;
    for id in asset_ids {
        if let Some(dispatch) = sim.dispatches.get(id) {
            if let Some(asset) = sim.assets.get(id) {
                setpoint_map.insert(*id, setpoint_from_dispatch(dispatch, asset));
            }
        }
    }

    if let Some(db) = state.db.as_ref() {
        let missing: Vec<Uuid> = asset_ids
            .iter()
            .filter(|id| !setpoint_map.contains_key(id))
            .cloned()
            .collect();
        if !missing.is_empty() {
            let rows = sqlx::query_as::<_, DispatchSnapshotRow>(
                r#"
                SELECT DISTINCT ON (asset_id)
                    id,
                    asset_id,
                    mw,
                    duration_s
                FROM dispatches
                WHERE asset_id = ANY($1)
                ORDER BY asset_id, submitted_at DESC
                "#,
            )
            .bind(&missing)
            .fetch_all(db)
            .await
            .context("querying latest dispatches for bootstrap")?;
            for row in rows {
                if let Some(asset) = sim.assets.get(&row.asset_id) {
                    let duration_s = row.duration_s.and_then(|v| u64::try_from(v).ok());
                    setpoint_map.insert(
                        row.asset_id,
                        Setpoint {
                            asset_id: row.asset_id.to_string(),
                            mw: row.mw,
                            duration_s,
                            site_id: Some(asset.site_id.to_string()),
                            group_id: None,
                            dispatch_id: Some(row.id.to_string()),
                        },
                    );
                }
            }
        }
    }

    for id in asset_ids {
        if setpoint_map.contains_key(id) {
            continue;
        }
        if let (Some(asset), Some(state)) = (sim.assets.get(id), sim.state.get(id)) {
            if let Some(sp) = setpoint_from_state(asset, state) {
                setpoint_map.insert(*id, sp);
            }
        }
    }

    for id in asset_ids {
        if telemetry_map.contains_key(id) {
            continue;
        }
        if let (Some(asset), Some(st)) = (sim.assets.get(id), sim.state.get(id)) {
            let mut tmp_state = st.clone();
            let snap = tick_asset(asset, &mut tmp_state, 0.0);
            telemetry_map.insert(*id, snap);
        }
    }

    let mut assets = Vec::new();
    for id in asset_ids {
        assets.push(AssetBootstrap {
            asset_id: id.to_string(),
            telemetry: telemetry_map.get(id).map(telemetry_to_proto),
            setpoint: setpoint_map.get(id).cloned(),
        });
    }

    Ok(BootstrapResponse { assets })
}

async fn handle_agent_heartbeat(state: &AppState, hb: proto::Heartbeat) -> Result<()> {
    let asset_id = Uuid::parse_str(&hb.asset_id)?;
    let ts = DateTime::parse_from_rfc3339(&hb.timestamp)
        .map(|dt| dt.with_timezone(&Utc))
        .context("parsing heartbeat timestamp")?;

    if let Some(db) = state.db.as_ref() {
        persist_heartbeat(db, asset_id, ts).await?;
    }
    Ok(())
}

async fn handle_dispatch_ack(state: &AppState, ack: DispatchAck) -> Result<()> {
    let dispatch_id = Uuid::parse_str(&ack.dispatch_id)?;
    let ts = DateTime::parse_from_rfc3339(&ack.timestamp)
        .map(|dt| dt.with_timezone(&Utc))
        .context("parsing dispatch ack timestamp")?;

    if let Some(db) = state.db.as_ref() {
        sqlx::query(
            r#"
            UPDATE dispatches
            SET ack_status = $1,
                acked_at = $2,
                ack_reason = $3
            WHERE id = $4
            "#,
        )
        .bind(&ack.status)
        .bind(ts)
        .bind(&ack.reason)
        .bind(dispatch_id)
        .execute(db)
        .await
        .context("updating dispatch ack")?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SocState {
    BelowMin,
    InRange,
    AboveMax,
}

async fn maybe_emit_soc_event(state: &AppState, db: &PgPool, snap: &Telemetry) -> Result<()> {
    let asset = {
        let sim = state.sim.read().await;
        sim.assets.get(&snap.asset_id).cloned()
    };
    let Some(asset) = asset else {
        return Ok(());
    };
    let (min_soc, max_soc) = soc_bounds(&asset);
    let eps = 1e-6;
    let next_state = if snap.soc_mwhr <= min_soc + eps {
        SocState::BelowMin
    } else if snap.soc_mwhr >= max_soc - eps {
        SocState::AboveMax
    } else {
        SocState::InRange
    };

    let mut states = state.soc_state.write().await;
    let prev = states.insert(snap.asset_id, next_state);
    if prev == Some(next_state) {
        return Ok(());
    }

    let (event_type, message) = match next_state {
        SocState::BelowMin => ("MIN_SOC_REACHED", "Min SOC reached"),
        SocState::AboveMax => ("MAX_SOC_REACHED", "Max SOC reached"),
        SocState::InRange => return Ok(()),
    };

    let event = BessEvent {
        id: Uuid::new_v4(),
        asset_id: snap.asset_id,
        site_id: asset.site_id,
        timestamp: snap.timestamp,
        event_type: event_type.to_string(),
        severity: "warning".to_string(),
        message: message.to_string(),
    };
    persist_event(db, &event).await?;
    Ok(())
}

async fn handle_agent_event(state: &AppState, evt: ProtoEvent) -> Result<()> {
    let asset_id = Uuid::parse_str(&evt.asset_id)?;
    let site_id = Uuid::parse_str(&evt.site_id)?;
    let ts = DateTime::parse_from_rfc3339(&evt.timestamp)
        .map(|dt| dt.with_timezone(&Utc))
        .context("parsing event timestamp")?;

    let event = BessEvent {
        id: Uuid::parse_str(&evt.id).unwrap_or_else(|_| Uuid::new_v4()),
        asset_id,
        site_id,
        timestamp: ts,
        event_type: evt.event_type,
        severity: evt.severity,
        message: evt.message,
    };

    if let Some(db) = state.db.as_ref() {
        persist_event(db, &event).await?;
    }
    Ok(())
}
