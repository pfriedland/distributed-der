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
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use sim_core::{Asset, BessEvent, BessState, Dispatch, DispatchRequest, Telemetry, tick_asset};
use sqlx::{PgPool, postgres::PgPoolOptions};
use tokio::sync::{RwLock, mpsc};
use tokio_stream::{StreamExt as TokioStreamExt, wrappers::ReceiverStream};
use tonic::{Request, Response as GrpcResponse, Status, transport::Server};
use tower_http::trace::TraceLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;

pub mod proto {
    tonic::include_proto!("agent");
}
use proto::{
    AgentToHeadend, AssetBootstrap, BootstrapRequest, BootstrapResponse, DispatchAck,
    Event as ProtoEvent, HeadendToAgent, Register, Setpoint,
    agent_link_server::{AgentLink, AgentLinkServer},
    agent_to_headend, headend_to_agent,
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

    fn set_dispatch(&mut self, req: DispatchRequest) -> Result<Dispatch, anyhow::Error> {
        // Validate the asset exists.
        let asset = self
            .assets
            .get(&req.asset_id)
            .context("asset not found for dispatch")?;
        // Reject dispatches that push beyond SOC bounds.
        if let Some(state) = self.state.get(&req.asset_id) {
            let (min_soc, max_soc) = soc_bounds(asset);
            let eps = 1e-6;
            if req.mw > 0.0 && state.soc_mwhr <= min_soc + eps {
                anyhow::bail!("dispatch rejected: at min SOC");
            }
            if req.mw < 0.0 && state.soc_mwhr >= max_soc - eps {
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

#[derive(Debug, Deserialize)]
struct AssetsFile {
    sites: Vec<SiteCfg>,
    assets: Vec<AssetCfg>,
}

#[derive(Debug, Deserialize)]
struct SiteCfg {
    id: Uuid,
    name: String,
    location: String,
}

#[derive(Debug, Deserialize)]
struct AssetCfg {
    id: Uuid,
    site_id: Uuid,
    name: String,
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

#[derive(sqlx::FromRow)]
struct TelemetrySeed {
    asset_id: Uuid,
    soc_mwhr: f64,
    current_mw: f64,
    setpoint_mw: f64,
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
        .add_service(AgentLinkServer::new(GrpcApi { state: grpc_state }))
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

                                state.agent_streams.write().await.insert(
                                    uuid,
                                    AgentStream {
                                        tx: tx.clone(),
                                        peer: peer_ip.clone(),
                                        asset_name: asset_name.clone(),
                                        site_name: site_name.clone(),
                                        site_id: site_uuid,
                                        connected_at: Utc::now(),
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
                                state.agent_streams.write().await.insert(
                                    uuid,
                                    AgentStream {
                                        tx: tx.clone(),
                                        peer: peer_ip.clone(),
                                        asset_name: asset_name.clone(),
                                        site_name: site_name.clone(),
                                        site_id: site_uuid,
                                        connected_at: Utc::now(),
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
                    status
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

fn env_truthy(name: &str) -> bool {
    match std::env::var(name) {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            matches!(v.as_str(), "1" | "true" | "yes" | "y" | "on")
        }
        Err(_) => false,
    }
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

async fn reset_db(pool: &PgPool) -> Result<()> {
    // TRUNCATE is fast and keeps table definitions intact.
    // NOTE: there are no foreign keys today, but CASCADE keeps this resilient if you add them later.
    sqlx::query(
        r#"
        TRUNCATE TABLE
            telemetry,
            agent_sessions,
            dispatches,
            heartbeats,
            events,
            assets
        CASCADE
        "#,
    )
    .execute(pool)
    .await
    .context("resetting database tables")?;
    Ok(())
}

async fn maybe_connect_db() -> Result<Option<PgPool>> {
    // DATABASE_URL is optional; if not set we run in-memory only.
    let url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => return Ok(None),
    };
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .context("connecting to DATABASE_URL")?;
    init_db(&pool).await?;
    // Optional: wipe all tables on startup for development convenience.
    // Usage: RESET_DB=1 cargo run -p der_headend
    if env_truthy("RESET_DB") {
        tracing::warn!("RESET_DB is set; truncating database tables");
        reset_db(&pool).await?;
    }
    Ok(Some(pool))
}

async fn init_db(pool: &PgPool) -> Result<()> {
    // Create simple tables if they do not exist.
    // Note: run statements separately to avoid the "cannot insert multiple commands"
    // error some Postgres drivers produce when using prepared statements.
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS assets (
            id uuid PRIMARY KEY,
            site_id uuid NOT NULL,
            name text NOT NULL,
            site_name text NOT NULL,
            location text NOT NULL,
            capacity_mwhr double precision NOT NULL,
            max_mw double precision NOT NULL,
            min_mw double precision NOT NULL,
            efficiency double precision NOT NULL,
            ramp_rate_mw_per_min double precision NOT NULL
        );
    "#,
    )
    .execute(pool)
    .await
    .context("creating assets table")?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS telemetry (
            asset_id uuid NOT NULL,
            site_id uuid NOT NULL,
            ts timestamptz NOT NULL,
            soc_mwhr double precision NOT NULL,
            soc_pct double precision NOT NULL,
            capacity_mwhr double precision NOT NULL,
            current_mw double precision NOT NULL,
            setpoint_mw double precision NOT NULL,
            max_mw double precision NOT NULL,
            min_mw double precision NOT NULL,
            status text NOT NULL
        );
    "#,
    )
    .execute(pool)
    .await
    .context("creating telemetry table")?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS agent_sessions (
            asset_id uuid NOT NULL,
            peer text NOT NULL,
            asset_name text,
            site_name text,
            connected_at timestamptz NOT NULL,
            disconnected_at timestamptz
        );
    "#,
    )
    .execute(pool)
    .await
    .context("creating agent_sessions table")?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS dispatches (
            id uuid PRIMARY KEY,
            asset_id uuid NOT NULL,
            mw double precision NOT NULL,
            duration_s bigint,
            status text NOT NULL,
            reason text,
            submitted_at timestamptz NOT NULL,
            clamped boolean NOT NULL DEFAULT false
        );
    "#,
    )
    .execute(pool)
    .await
    .context("creating dispatches table")?;

    // Best-effort add missing columns if the table already existed.
    // Add missing columns individually to avoid multi-statement prepared issues.
    sqlx::query(r#"ALTER TABLE agent_sessions ADD COLUMN IF NOT EXISTS asset_name text;"#)
        .execute(pool)
        .await
        .context("altering agent_sessions.asset_name")?;
    sqlx::query(r#"ALTER TABLE agent_sessions ADD COLUMN IF NOT EXISTS site_name text;"#)
        .execute(pool)
        .await
        .context("altering agent_sessions.site_name")?;
    sqlx::query(
        r#"ALTER TABLE agent_sessions ADD COLUMN IF NOT EXISTS disconnected_at timestamptz;"#,
    )
    .execute(pool)
    .await
    .context("altering agent_sessions.disconnected_at")?;
    sqlx::query(r#"ALTER TABLE dispatches ADD COLUMN IF NOT EXISTS clamped boolean NOT NULL DEFAULT false;"#)
        .execute(pool)
        .await
        .context("altering dispatches.clamped")?;
    sqlx::query(r#"ALTER TABLE dispatches ADD COLUMN IF NOT EXISTS ack_status text;"#)
        .execute(pool)
        .await
        .context("altering dispatches.ack_status")?;
    sqlx::query(r#"ALTER TABLE dispatches ADD COLUMN IF NOT EXISTS acked_at timestamptz;"#)
        .execute(pool)
        .await
        .context("altering dispatches.acked_at")?;
    sqlx::query(r#"ALTER TABLE dispatches ADD COLUMN IF NOT EXISTS ack_reason text;"#)
        .execute(pool)
        .await
        .context("altering dispatches.ack_reason")?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS heartbeats (
            asset_id uuid NOT NULL,
            ts timestamptz NOT NULL
        );
        "#,
    )
    .execute(pool)
    .await
    .context("creating heartbeats table")?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS events (
            id uuid PRIMARY KEY,
            asset_id uuid NOT NULL,
            site_id uuid NOT NULL,
            ts timestamptz NOT NULL,
            event_type text NOT NULL,
            severity text NOT NULL,
            message text NOT NULL
        );
        "#,
    )
    .execute(pool)
    .await
    .context("creating events table")?;
    Ok(())
}

async fn persist_telemetry(pool: &PgPool, snaps: &[Telemetry]) -> Result<()> {
    // Write each snapshot into the telemetry table.
    for snap in snaps {
        sqlx::query(
            r#"
            INSERT INTO telemetry (
                asset_id, site_id, ts, soc_mwhr, soc_pct,
                capacity_mwhr, current_mw, setpoint_mw, max_mw, min_mw, status
            ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)
        "#,
        )
        .bind(snap.asset_id)
        .bind(snap.site_id)
        .bind(snap.timestamp)
        .bind(snap.soc_mwhr)
        .bind(snap.soc_pct)
        .bind(snap.capacity_mwhr)
        .bind(snap.current_mw)
        .bind(snap.setpoint_mw)
        .bind(snap.max_mw)
        .bind(snap.min_mw)
        .bind(&snap.status)
        .execute(pool)
        .await
        .context("inserting telemetry row")?;
    }
    Ok(())
}

async fn persist_assets(pool: &PgPool, assets: &[Asset]) -> Result<()> {
    // Upsert asset metadata so history queries can join for names.
    for asset in assets {
        sqlx::query(
            r#"
            INSERT INTO assets (
                id, site_id, name, site_name, location, capacity_mwhr,
                max_mw, min_mw, efficiency, ramp_rate_mw_per_min
            ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)
            ON CONFLICT (id) DO UPDATE SET
                site_id = EXCLUDED.site_id,
                name = EXCLUDED.name,
                site_name = EXCLUDED.site_name,
                location = EXCLUDED.location,
                capacity_mwhr = EXCLUDED.capacity_mwhr,
                max_mw = EXCLUDED.max_mw,
                min_mw = EXCLUDED.min_mw,
                efficiency = EXCLUDED.efficiency,
                ramp_rate_mw_per_min = EXCLUDED.ramp_rate_mw_per_min
        "#,
        )
        .bind(asset.id)
        .bind(asset.site_id)
        .bind(&asset.name)
        .bind(&asset.site_name)
        .bind(&asset.location)
        .bind(asset.capacity_mwhr)
        .bind(asset.max_mw)
        .bind(asset.min_mw)
        .bind(asset.efficiency)
        .bind(asset.ramp_rate_mw_per_min)
        .execute(pool)
        .await
        .context("upserting asset")?;
    }
    Ok(())
}

async fn persist_dispatch(pool: &PgPool, d: &Dispatch) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO dispatches (id, asset_id, mw, duration_s, status, reason, submitted_at, clamped, ack_status, acked_at, ack_reason)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NULL, NULL, NULL)
        "#,
    )
    .bind(d.id)
    .bind(d.asset_id)
    .bind(d.mw)
    .bind(d.duration_s.map(|v| v as i64))
    .bind(&d.status)
    .bind(&d.reason)
    .bind(d.submitted_at)
    .bind(d.clamped)
    .execute(pool)
    .await
    .context("inserting dispatch row")?;
    Ok(())
}

async fn persist_event(pool: &PgPool, event: &BessEvent) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO events (id, asset_id, site_id, ts, event_type, severity, message)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        "#,
    )
    .bind(event.id)
    .bind(event.asset_id)
    .bind(event.site_id)
    .bind(event.timestamp)
    .bind(&event.event_type)
    .bind(&event.severity)
    .bind(&event.message)
    .execute(pool)
    .await
    .context("inserting event")?;
    Ok(())
}

async fn persist_heartbeat(pool: &PgPool, asset_id: Uuid, ts: DateTime<Utc>) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO heartbeats (asset_id, ts)
        VALUES ($1, $2)
        "#,
    )
    .bind(asset_id)
    .bind(ts)
    .execute(pool)
    .await
    .context("inserting heartbeat")?;
    Ok(())
}

// --- HTTP handlers ---

async fn ui_home() -> Html<&'static str> {
    // Lightweight demo UI: single HTML page with inline JS calling the existing REST endpoints.
    // No frontend toolchain required.
    Html(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>DER Headend — MVP UI</title>
  <style>
    body { font-family: ui-sans-serif, system-ui, -apple-system, Segoe UI, Roboto, Helvetica, Arial; margin: 16px; }
    h1 { margin: 0 0 6px 0; }
    .sub { color: #555; margin: 0 0 16px 0; }
    .grid { display: grid; grid-template-columns: 1fr 1fr; gap: 12px; }
    .card { border: 1px solid #ddd; border-radius: 10px; padding: 12px; }
    .card h2 { font-size: 16px; margin: 0 0 10px 0; }
    table { width: 100%; border-collapse: collapse; font-size: 13px; table-layout: auto; }
    th, td { border-bottom: 1px solid #eee; padding: 6px 8px; text-align: left; vertical-align: top; }
    th { color: #444; font-weight: 600; background: #fafafa; position: sticky; top: 0; }
    tr:hover { background: #fcfcff; cursor: pointer; }
    .row { display: flex; gap: 8px; flex-wrap: wrap; align-items: center; }
    input, select, button { font-size: 13px; padding: 6px 8px; }
    button { cursor: pointer; }
    .pill { display: inline-block; padding: 2px 8px; border-radius: 999px; font-size: 12px; }
    .ok { background: #e8fff0; border: 1px solid #b9f1cc; }
    .bad { background: #fff0f0; border: 1px solid #f1b9b9; }
    pre { margin: 0; font-size: 12px; overflow: auto; max-height: 260px; background: #0b1020; color: #e9eefc; padding: 10px; border-radius: 10px; }
    .muted { color: #666; }
    .links a { margin-right: 10px; }
    .small { font-size: 12px; }
    .clamped-row { background: #fff6e5; }
  </style>
</head>
<body>
  <h1>DER Headend — MVP UI</h1>
  <p class="sub">Local dashboard for assets, agents, telemetry, and dispatch. (API-first; this page uses the same endpoints you can call with <code>curl</code>.)</p>

  <div class="links small">
    <span class="muted">Quick links:</span>
    <a href="/assets">/assets</a>
    <a href="/agents">/agents</a>
    <a href="/dispatch/history">/dispatch/history</a>
  </div>

  <div class="grid" style="margin-top: 12px;">
    <div class="card">
      <h2>Dispatch</h2>
      <div class="row" style="margin-bottom: 8px;">
        <label><input type="radio" name="dispatch_mode" value="asset" checked /> Per-asset</label>
        <label><input type="radio" name="dispatch_mode" value="site" /> Per-site</label>
        <span class="muted">BESS convention: <b>+MW discharge</b>, <b>-MW charge</b></span>
      </div>

      <div class="row" style="margin-bottom: 8px;">
        <select id="asset_select" style="min-width: 380px;"></select>
        <select id="site_select" style="min-width: 380px; display:none;"></select>
      </div>

      <div class="row" style="margin-bottom: 8px;">
        <label>MW <input id="mw" type="number" step="0.1" value="5" style="width: 120px;" /></label>
        <label>Duration (s) <input id="duration_s" type="number" step="1" placeholder="optional" style="width: 140px;" /></label>
        <button id="dispatch_btn">Send dispatch</button>
        <span id="dispatch_status" class="pill ok" style="display:none;"></span>
      </div>

      <div class="small muted">Tip: in Gateway Mode, per-site dispatch will be split across online assets at that site (capacity-weighted + clamped).</div>

      <div style="margin-top: 10px;">
        <div class="row" style="justify-content: space-between;">
          <div class="small muted">Last response</div>
          <button id="clear_resp" class="small">Clear</button>
        </div>
        <pre id="dispatch_resp">{}</pre>
        <div id="allocations_box" class="small" style="margin-top: 10px; display:none;">
          <div class="muted" style="margin-bottom: 6px;">Allocations (clamped rows highlighted)</div>
          <table>
            <thead>
              <tr>
                <th>site_name</th>
                <th>asset_name</th>
                <th>mw_raw</th>
                <th>mw_final</th>
                <th>clamped</th>
              </tr>
            </thead>
            <tbody id="allocations_tbody"></tbody>
          </table>
        </div>
      </div>
    </div>

    <div class="card">
      <h2>Selected asset: telemetry + heartbeat</h2>
      <div class="small muted" style="margin-bottom: 8px;">Click an asset row below to load its latest telemetry and heartbeat.</div>
      <div class="row" style="margin-bottom: 8px;">
        <span class="small muted">Asset:</span> <span id="selected_asset" class="small">(none)</span>
        <button id="refresh_selected" class="small">Refresh</button>
      </div>
      <div class="row" style="gap: 12px;">
        <div style="flex: 1; min-width: 320px;">
          <div class="small muted" style="margin-bottom: 6px;">Telemetry (latest)</div>
          <pre id="telemetry_json">{}</pre>
        </div>
        <div style="flex: 1; min-width: 320px;">
          <div class="small muted" style="margin-bottom: 6px;">Heartbeat (latest)</div>
          <pre id="heartbeat_json">{}</pre>
        </div>
      </div>
    </div>

    <div class="card" style="grid-column: 1 / span 2;">
      <div class="row" style="justify-content: space-between;">
        <h2 style="margin:0;">Assets</h2>
        <div class="row">
          <button id="refresh_assets" class="small">Refresh</button>
          <span id="assets_meta" class="small muted"></span>
        </div>
      </div>
      <div style="max-height: 320px; overflow:auto; margin-top: 8px;">
        <table>
          <thead>
            <tr>
              <th>site_name</th>
              <th>asset_name</th>
              <th>current_mw</th>
              <th>soc_pct</th>
              <th>setpoint_mw</th>
              <th>cap_mwhr</th>
              <th>min_mw</th>
              <th>max_mw</th>
              <th>min_soc_pct</th>
              <th>max_soc_pct</th>
            </tr>
          </thead>
          <tbody id="assets_tbody"></tbody>
        </table>
      </div>
    </div>

    <div style="grid-column: 1 / span 2; display:grid; grid-template-columns: 1fr 2fr; gap: 12px;">
      <div class="card">
        <div class="row" style="justify-content: space-between;">
          <h2 style="margin:0;">Agents</h2>
          <div class="row">
            <button id="refresh_agents" class="small">Refresh</button>
            <span id="agents_meta" class="small muted"></span>
          </div>
        </div>
        <div style="max-height: 320px; overflow:auto; margin-top: 8px;">
          <table>
            <thead>
              <tr>
                <th>connected_at</th>
                <th>connected</th>
                <th>site_name</th>
                <th>asset_name</th>
                <th>peer</th>
                <th>sessions</th>
              </tr>
            </thead>
            <tbody id="agents_tbody"></tbody>
          </table>
        </div>
      </div>

      <div class="card">
        <div class="row" style="justify-content: space-between;">
          <h2 style="margin:0;">Events history (selected asset)</h2>
          <div class="row">
            <span class="small muted">Auto-refreshes with selected asset.</span>
          </div>
        </div>
        <div style="max-height: 360px; overflow:auto; margin-top: 8px;">
          <table>
            <thead>
              <tr>
                <th>timestamp</th>
                <th>site_name</th>
                <th>asset_name</th>
                <th>event_info</th>
              </tr>
            </thead>
            <tbody id="events_tbody"></tbody>
          </table>
        </div>
      </div>
    </div>

    <div class="card" style="grid-column: 1 / span 2;">
      <div class="row" style="justify-content: space-between;">
        <h2 style="margin:0;">Recent dispatch history</h2>
        <div class="row">
          <button id="refresh_dispatch_hist" class="small">Refresh</button>
          <span id="dispatch_hist_meta" class="small muted"></span>
        </div>
      </div>
      <div class="small muted" style="margin-top: 6px;">Note: returns an empty array if no DB is configured.</div>
      <div style="max-height: 280px; overflow:auto; margin-top: 8px;">
        <table>
          <thead>
            <tr>
              <th>submitted_at</th>
              <th>site_name</th>
              <th>asset_name</th>
              <th>mw</th>
              <th>clamped</th>
              <th>ack_status</th>
              <th>acked_at</th>
              <th>duration_s</th>
              <th>status</th>
              <th>reason</th>
            </tr>
          </thead>
          <tbody id="dispatch_hist_tbody"></tbody>
        </table>
      </div>
    </div>

    <div class="card" style="grid-column: 1 / span 2;">
      <div class="row" style="justify-content: space-between;">
        <h2 style="margin:0;">Telemetry history (selected asset)</h2>
        <div class="row">
          <button id="load_hist" class="small">Load history</button>
          <span id="hist_meta" class="small muted"></span>
        </div>
      </div>

      <div class="row" style="margin-top: 6px;">
        <label class="small">asset
          <select id="hist_asset_select" style="min-width: 360px;"></select>
        </label>
        <label class="small">start
          <input id="hist_start" type="datetime-local" step="1" style="width: 300px;" />
        </label>
        <label class="small">end
          <input id="hist_end" type="datetime-local" step="1" style="width: 300px;" />
        </label>
        <span class="small muted">Note: returns an empty list if no DB is configured.</span>
      </div>

      <div style="max-height: 360px; overflow:auto; margin-top: 8px;">
        <table style="min-width: 1800px;">
          <thead>
            <tr>
              <th>ts</th>
              <th>site_name</th>
              <th>asset_name</th>
              <th>soc_pct</th>
              <th>soc_mwhr</th>
              <th>capacity_mwhr</th>
              <th>current_mw</th>
              <th>setpoint_mw</th>
              <th>min_mw</th>
              <th>max_mw</th>
              <th>status</th>
            </tr>
          </thead>
          <tbody id="telemetry_hist_tbody"></tbody>
        </table>
      </div>
    </div>
  </div>

<script>
  const $ = (id) => document.getElementById(id);

  let ASSETS = [];
  let SITES = new Map(); // site_id -> site_name
  let TELEMETRY = new Map();
  let selectedAssetId = null;

  function pretty(obj) {
    try { return JSON.stringify(obj, null, 2); } catch { return String(obj); }
  }

  async function fetchJson(path, opts) {
    const res = await fetch(path, opts);
    const text = await res.text();
    let data;
    try { data = text ? JSON.parse(text) : null; } catch { data = text; }
    if (!res.ok) {
      const msg = typeof data === 'string' ? data : (data && data.message) ? data.message : res.statusText;
      throw new Error(`${res.status} ${msg}`);
    }
    return data;
  }

  function setStatus(ok, msg) {
    const el = $('dispatch_status');
    el.style.display = 'inline-block';
    el.className = `pill ${ok ? 'ok' : 'bad'}`;
    el.textContent = msg;
    setTimeout(() => { el.style.display = 'none'; }, 2500);
  }

  function dispatchMode() {
    return document.querySelector('input[name="dispatch_mode"]:checked').value;
  }

  function rebuildSelectors() {
    const assetSel = $('asset_select');
    assetSel.innerHTML = '';
    for (const a of ASSETS) {
      const opt = document.createElement('option');
      opt.value = a.id;
      opt.textContent = `${a.name}  (${a.id})  — ${a.site_name}`;
      assetSel.appendChild(opt);
    }

    const siteSel = $('site_select');
    siteSel.innerHTML = '';
    const sites = Array.from(SITES.entries()).sort((a,b) => a[1].localeCompare(b[1]));
    for (const [siteId, siteName] of sites) {
      const opt = document.createElement('option');
      opt.value = siteId;
      opt.textContent = `${siteName}  (${siteId})`;
      siteSel.appendChild(opt);
    }

    const histSel = $('hist_asset_select');
    histSel.innerHTML = '';
    const histList = Array.isArray(ASSETS) ? [...ASSETS] : [];
    histList.sort((a, b) => {
      const as = (a.site_name || '').toLowerCase();
      const bs = (b.site_name || '').toLowerCase();
      if (as < bs) return -1;
      if (as > bs) return 1;
      const aa = (a.name || '').toLowerCase();
      const ba = (b.name || '').toLowerCase();
      if (aa < ba) return -1;
      if (aa > ba) return 1;
      return 0;
    });
    for (const a of histList) {
      const opt = document.createElement('option');
      opt.value = a.id;
      opt.textContent = `${a.site_name} — ${a.name}`;
      histSel.appendChild(opt);
    }
    if (selectedAssetId) {
      histSel.value = selectedAssetId;
    }
  }

  function renderAssetsTable() {
    const tbody = $('assets_tbody');
    tbody.innerHTML = '';
    const list = Array.isArray(ASSETS) ? [...ASSETS] : [];
    list.sort((a, b) => {
      const as = (a.site_name || '').toLowerCase();
      const bs = (b.site_name || '').toLowerCase();
      if (as < bs) return -1;
      if (as > bs) return 1;
      const aa = (a.name || '').toLowerCase();
      const ba = (b.name || '').toLowerCase();
      if (aa < ba) return -1;
      if (aa > ba) return 1;
      return 0;
    });
    for (const a of list) {
      const tr = document.createElement('tr');
      tr.onclick = () => selectAsset(a.id, a.name);
      const t = TELEMETRY.get(a.id);
      tr.innerHTML = `
        <td>${escapeHtml(a.site_name)}</td>
        <td>${escapeHtml(a.name)}</td>
        <td>${fmt(t ? t.current_mw : null)}</td>
        <td>${fmt(t ? t.soc_pct : null)}</td>
        <td>${fmt(t ? t.setpoint_mw : null)}</td>
        <td>${fmt(a.capacity_mwhr)}</td>
        <td>${fmt(a.min_mw)}</td>
        <td>${fmt(a.max_mw)}</td>
        <td>${fmt(a.min_soc_pct)}</td>
        <td>${fmt(a.max_soc_pct)}</td>
      `;
      tbody.appendChild(tr);
    }
    $('assets_meta').textContent = `${ASSETS.length} assets`;
  }

  function renderAgentsTable(agents) {
    const tbody = $('agents_tbody');
    tbody.innerHTML = '';
    const list = Array.isArray(agents) ? [...agents] : [];
    list.sort((a, b) => {
      const as = (a.site_name || '').toLowerCase();
      const bs = (b.site_name || '').toLowerCase();
      if (as < bs) return -1;
      if (as > bs) return 1;
      const aa = (a.asset_name || '').toLowerCase();
      const ba = (b.asset_name || '').toLowerCase();
      if (aa < ba) return -1;
      if (aa > ba) return 1;
      return 0;
    });
    for (const a of list) {
      const tr = document.createElement('tr');
      const ok = a.connected ? 'ok' : 'bad';
      const connText = a.connected ? 'connected' : 'disconnected';
      const sessionCount = Array.isArray(a.sessions) ? a.sessions.length : 0;
      tr.innerHTML = `
        <td><code>${escapeHtml(formatTs(a.connected_at || ''))}</code></td>
        <td><span class="pill ${ok}">${connText}</span></td>
        <td>${escapeHtml(a.site_name || '')}</td>
        <td>${escapeHtml(a.asset_name || '')}</td>
        <td><code>${escapeHtml(a.peer || '')}</code></td>
        <td>${sessionCount}</td>
      `;
      tbody.appendChild(tr);
    }
    $('agents_meta').textContent = `${agents.length} rows`;
  }

  function renderDispatchHistory(rows) {
    const tbody = $('dispatch_hist_tbody');
    tbody.innerHTML = '';
    for (const r of rows) {
      const tr = document.createElement('tr');
      tr.innerHTML = `
        <td><code>${escapeHtml(formatTs(r.submitted_at))}</code></td>
        <td>${escapeHtml(r.site_name || '')}</td>
        <td>${escapeHtml(r.asset_name || '')}</td>
        <td>${fmt(r.mw)}</td>
        <td>${r.clamped ? 'yes' : 'no'}</td>
        <td>${escapeHtml(r.ack_status || '')}</td>
        <td><code>${escapeHtml(formatTs(r.acked_at || ''))}</code></td>
        <td>${r.duration_s == null ? '' : escapeHtml(String(r.duration_s))}</td>
        <td>${escapeHtml(r.status || '')}</td>
        <td>${escapeHtml(r.reason || '')}</td>
      `;
      tbody.appendChild(tr);
    }
    $('dispatch_hist_meta').textContent = `${rows.length} rows`;
  }

  function renderAllocations(res) {
    const box = $('allocations_box');
    const tbody = $('allocations_tbody');
    tbody.innerHTML = '';
    const list = res && Array.isArray(res.allocations) ? res.allocations : [];
    if (!list.length) {
      box.style.display = 'none';
      return;
    }
    box.style.display = 'block';
    for (const a of list) {
      const tr = document.createElement('tr');
      if (a.clamped) tr.classList.add('clamped-row');
      const asset = ASSETS.find(x => x.id === a.asset_id);
      tr.innerHTML = `
        <td>${escapeHtml(asset ? asset.site_name : '')}</td>
        <td>${escapeHtml(asset ? asset.name : '')}</td>
        <td>${fmt(a.mw_raw)}</td>
        <td>${fmt(a.mw)}</td>
        <td>${a.clamped ? 'yes' : 'no'}</td>
      `;
      tbody.appendChild(tr);
    }
  }

  function renderTelemetryHistory(rows) {
    const tbody = $('telemetry_hist_tbody');
    tbody.innerHTML = '';
    const list = Array.isArray(rows) ? rows : [];
    for (const r of list) {
      const tr = document.createElement('tr');
      tr.innerHTML = `
        <td><code>${escapeHtml(formatTs(r.ts))}</code></td>
        <td>${escapeHtml(r.site_name || '')}</td>
        <td>${escapeHtml(r.asset_name || '')}</td>
        <td>${fmt(r.soc_pct)}</td>
        <td>${fmt(r.soc_mwhr)}</td>
        <td>${fmt(r.capacity_mwhr)}</td>
        <td>${fmt(r.current_mw)}</td>
        <td>${fmt(r.setpoint_mw)}</td>
        <td>${fmt(r.min_mw)}</td>
        <td>${fmt(r.max_mw)}</td>
        <td>${escapeHtml(r.status || '')}</td>
      `;
      tbody.appendChild(tr);
    }
    $('hist_meta').textContent = `${list.length} rows`;
  }

  function renderEvents(rows) {
    const tbody = $('events_tbody');
    tbody.innerHTML = '';
    const list = Array.isArray(rows) ? rows : [];
    for (const r of list) {
      const info = `${r.event_type || ''}${r.severity ? ` (${r.severity})` : ''}${r.message ? ` — ${r.message}` : ''}`;
      const tr = document.createElement('tr');
      tr.innerHTML = `
        <td><code>${escapeHtml(formatTs(r.ts))}</code></td>
        <td>${escapeHtml(r.site_name || '')}</td>
        <td>${escapeHtml(r.asset_name || '')}</td>
        <td>${escapeHtml(info)}</td>
      `;
      tbody.appendChild(tr);
    }
  }

  async function loadTelemetryHistory() {
    if (!selectedAssetId) return;
    const start = toIso($('hist_start').value.trim());
    const end = toIso($('hist_end').value.trim());
    const qs = new URLSearchParams();
    if (start) qs.set('start', start);
    if (end) qs.set('end', end);
    const url = qs.toString() ? `/telemetry/${selectedAssetId}/history?${qs}` : `/telemetry/${selectedAssetId}/history`;
    try {
      const rows = await fetchJson(url);
      renderTelemetryHistory(rows);
    } catch (e) {
      renderTelemetryHistory([]);
      $('hist_meta').textContent = `error: ${String(e)}`;
    }
  }

  function fmt(v) {
    if (v == null) return '';
    if (typeof v === 'number') return Number.isFinite(v) ? v.toFixed(2) : String(v);
    return String(v);
  }

  function toIso(value) {
    if (!value) return '';
    const dt = new Date(value);
    if (Number.isNaN(dt.getTime())) return '';
    return dt.toISOString();
  }

  function formatTs(ts) {
    if (!ts) return '';
    const dt = new Date(ts);
    if (Number.isNaN(dt.getTime())) return String(ts);
    const dtf = new Intl.DateTimeFormat(undefined, {
      year: 'numeric',
      month: '2-digit',
      day: '2-digit',
      hour: '2-digit',
      minute: '2-digit',
      second: '2-digit',
      hour12: false,
      timeZoneName: 'short',
    });
    const parts = dtf.formatToParts(dt);
    const get = (type) => parts.find(p => p.type === type)?.value || '';
    const year = get('year');
    const month = get('month');
    const day = get('day');
    const hour = get('hour');
    const min = get('minute');
    const sec = get('second');
    const tz = get('timeZoneName').replace(/\s+/g, '');
    return `${year}-${month}-${day}T${hour}:${min}:${sec} ${tz}`;
  }

  function escapeHtml(s) {
    return String(s)
      .replaceAll('&', '&amp;')
      .replaceAll('<', '&lt;')
      .replaceAll('>', '&gt;')
      .replaceAll('"', '&quot;')
      .replaceAll("'", '&#39;');
  }

  async function loadAssets() {
    const assets = await fetchJson('/assets');
    ASSETS = Array.isArray(assets) ? assets : [];
    SITES = new Map();
    for (const a of ASSETS) {
      if (a.site_id && a.site_name) SITES.set(a.site_id, a.site_name);
    }
    rebuildSelectors();
    renderAssetsTable();
  }

  async function refreshAssetTelemetry() {
    const list = Array.isArray(ASSETS) ? ASSETS : [];
    const requests = list.map(a =>
      fetchJson(`/telemetry/${a.id}`)
        .then(t => ({ id: a.id, t }))
        .catch(() => null)
    );
    const results = await Promise.all(requests);
    TELEMETRY = new Map();
    for (const item of results) {
      if (item && item.t) TELEMETRY.set(item.id, item.t);
    }
    renderAssetsTable();
  }

  async function loadAgents() {
    const agents = await fetchJson('/agents');
    renderAgentsTable(Array.isArray(agents) ? agents : []);
  }

  async function loadDispatchHistory() {
    const rows = await fetchJson('/dispatch/history');
    renderDispatchHistory(Array.isArray(rows) ? rows : []);
  }

  async function selectAsset(id, name) {
    selectedAssetId = id;
    $('selected_asset').textContent = `${name} (${id})`;
    $('hist_asset_select').value = id;
    await refreshSelected();
  }

  async function refreshSelected() {
    if (!selectedAssetId) return;
    try {
      const t = await fetchJson(`/telemetry/${selectedAssetId}`);
      $('telemetry_json').textContent = pretty(t);
    } catch (e) {
      $('telemetry_json').textContent = pretty({ error: String(e) });
    }

    try {
      const hb = await fetchJson(`/heartbeat/${selectedAssetId}`);
      $('heartbeat_json').textContent = pretty(hb);
    } catch (e) {
      $('heartbeat_json').textContent = pretty({ error: String(e) });
    }
    try {
      const events = await fetchJson(`/events/${selectedAssetId}/history`);
      renderEvents(events);
    } catch (e) {
      renderEvents([]);
    }
    await loadTelemetryHistory();
  }

  async function sendDispatch() {
    const mode = dispatchMode();
    const mw = Number($('mw').value);
    const durRaw = $('duration_s').value;
    const duration_s = durRaw === '' ? null : Number(durRaw);

    if (!Number.isFinite(mw)) {
      setStatus(false, 'MW must be a number');
      return;
    }

    const payload = { mw };
    if (duration_s != null && Number.isFinite(duration_s)) payload.duration_s = duration_s;

    if (mode === 'asset') {
      const asset_id = $('asset_select').value;
      if (!asset_id) { setStatus(false, 'Select an asset'); return; }
      payload.asset_id = asset_id;
    } else {
      const site_id = $('site_select').value;
      if (!site_id) { setStatus(false, 'Select a site'); return; }
      payload.site_id = site_id;
    }

    try {
      const res = await fetchJson('/dispatch', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(payload),
      });
      $('dispatch_resp').textContent = pretty(res);
      renderAllocations(res);
      setStatus(true, 'Dispatch accepted');
      loadDispatchHistory().catch(() => {});
      if (payload.asset_id) {
        selectedAssetId = payload.asset_id;
        const a = ASSETS.find(x => x.id === payload.asset_id);
        $('selected_asset').textContent = a ? `${a.name} (${a.id})` : payload.asset_id;
        refreshSelected().catch(() => {});
      }
    } catch (e) {
      $('dispatch_resp').textContent = pretty({ error: String(e), payload });
      renderAllocations(null);
      setStatus(false, 'Dispatch failed');
    }
  }

  function wireUi() {
    const radios = document.querySelectorAll('input[name="dispatch_mode"]');
    for (const r of radios) {
      r.addEventListener('change', () => {
        const mode = dispatchMode();
        $('asset_select').style.display = (mode === 'asset') ? 'inline-block' : 'none';
        $('site_select').style.display = (mode === 'site') ? 'inline-block' : 'none';
      });
    }

    $('dispatch_btn').addEventListener('click', () => sendDispatch());
    $('clear_resp').addEventListener('click', () => {
      $('dispatch_resp').textContent = '{}';
      renderAllocations(null);
    });
    $('refresh_assets').addEventListener('click', () => loadAssets().catch(err => setStatus(false, String(err))));
    $('refresh_agents').addEventListener('click', () => loadAgents().catch(err => setStatus(false, String(err))));
    $('refresh_dispatch_hist').addEventListener('click', () => loadDispatchHistory().catch(err => setStatus(false, String(err))));
    $('refresh_selected').addEventListener('click', () => refreshSelected().catch(() => {}));
    $('load_hist').addEventListener('click', () => loadTelemetryHistory().catch(() => {}));
    $('hist_asset_select').addEventListener('change', async (e) => {
      const id = e.target.value;
      const asset = ASSETS.find(a => a.id === id);
      selectedAssetId = id;
      $('selected_asset').textContent = asset ? `${asset.name} (${asset.id})` : id;
      await refreshSelected();
    });

    $('site_select').style.display = 'none';
  }

  async function boot() {
    wireUi();
    try {
      await loadAssets();
      await refreshAssetTelemetry();
      await loadAgents();
      await loadDispatchHistory();
      if (ASSETS.length) selectAsset(ASSETS[0].id, ASSETS[0].name);
      setInterval(() => loadAgents().catch(() => {}), 4000);
      setInterval(() => loadDispatchHistory().catch(() => {}), 4000);
      setInterval(() => refreshAssetTelemetry().catch(() => {}), 4000);
    } catch (e) {
      $('dispatch_resp').textContent = pretty({ error: String(e) });
      setStatus(false, 'UI boot failed');
    }
  }

  boot();
</script>

</body>
</html>"#,
    )
}

#[derive(Serialize)]
struct HealthResponse {
    status: String,
    db: String,
}

async fn health(State(state): State<AppState>) -> Response {
    if let Some(db) = state.db.as_ref() {
        match sqlx::query("SELECT 1").execute(db).await {
            Ok(_) => Json(HealthResponse {
                status: "ok".to_string(),
                db: "ok".to_string(),
            })
            .into_response(),
            Err(err) => {
                tracing::warn!(error = %err, "health db check failed");
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(HealthResponse {
                        status: "degraded".to_string(),
                        db: "error".to_string(),
                    }),
                )
                    .into_response()
            }
        }
    } else {
        Json(HealthResponse {
            status: "ok".to_string(),
            db: "disabled".to_string(),
        })
        .into_response()
    }
}

async fn list_assets(State(state): State<AppState>) -> Json<Vec<AssetView>> {
    let sim = state.sim.read().await;
    let mut latest = state.latest.read().await.clone();
    for (id, asset) in sim.assets.iter() {
        if latest.contains_key(id) {
            continue;
        }
        if let Some(st) = sim.state.get(id) {
            let mut tmp_state = st.clone();
            let snap = tick_asset(asset, &mut tmp_state, 0.0);
            latest.insert(*id, snap);
        }
    }
    let mut rows = Vec::new();
    for asset in sim.assets.values() {
        let telemetry = latest.get(&asset.id);
        rows.push(AssetView {
            id: asset.id,
            site_id: asset.site_id,
            site_name: asset.site_name.clone(),
            name: asset.name.clone(),
            location: asset.location.clone(),
            capacity_mwhr: asset.capacity_mwhr,
            max_mw: asset.max_mw,
            min_mw: asset.min_mw,
            min_soc_pct: asset.min_soc_pct,
            max_soc_pct: asset.max_soc_pct,
            efficiency: asset.efficiency,
            ramp_rate_mw_per_min: asset.ramp_rate_mw_per_min,
            soc_pct: telemetry.map(|t| t.soc_pct),
            soc_mwhr: telemetry.map(|t| t.soc_mwhr),
            status: telemetry.map(|t| t.status.clone()),
            current_mw: telemetry.map(|t| t.current_mw),
            setpoint_mw: telemetry.map(|t| t.setpoint_mw),
        });
    }
    Json(rows)
}

async fn latest_telemetry(State(state): State<AppState>, Path(id): Path<Uuid>) -> Response {
    // Prefer the latest snapshot received from an agent; fall back to sim.
    if let Some(snap) = state.latest.read().await.get(&id).cloned() {
        return Json(snap).into_response();
    }
    let sim = state.sim.read().await;
    // Grab live state and synthesize a telemetry snapshot on demand.
    if let (Some(asset), Some(st)) = (sim.assets.get(&id), sim.state.get(&id)) {
        let mut tmp_state = st.clone();
        let snap = tick_asset(asset, &mut tmp_state, 0.0);
        Json(snap).into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

#[derive(Serialize, sqlx::FromRow)]
struct TelemetryRow {
    ts: DateTime<Utc>,
    asset_name: String,
    site_name: String,
    soc_mwhr: f64,
    soc_pct: f64,
    capacity_mwhr: f64,
    current_mw: f64,
    setpoint_mw: f64,
    max_mw: f64,
    min_mw: f64,
    status: String,
    asset_id: Uuid,
    site_id: Uuid,
}

#[derive(sqlx::FromRow)]
struct TelemetrySnapshotRow {
    asset_id: Uuid,
    site_id: Uuid,
    site_name: String,
    ts: DateTime<Utc>,
    soc_mwhr: f64,
    soc_pct: f64,
    capacity_mwhr: f64,
    current_mw: f64,
    setpoint_mw: f64,
    max_mw: f64,
    min_mw: f64,
    status: String,
}

#[derive(sqlx::FromRow)]
struct DispatchSnapshotRow {
    id: Uuid,
    asset_id: Uuid,
    mw: f64,
    duration_s: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct TimeRange {
    start: Option<String>,
    end: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
struct IncomingDispatch {
    asset_id: Option<Uuid>,
    site_id: Option<Uuid>,
    mw: f64,
    duration_s: Option<u64>,
}

#[derive(Serialize, sqlx::FromRow)]
struct HeartbeatRow {
    asset_id: Uuid,
    ts: DateTime<Utc>,
    asset_name: String,
    site_name: String,
}

#[derive(Serialize)]
struct AssetView {
    id: Uuid,
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
    soc_pct: Option<f64>,
    soc_mwhr: Option<f64>,
    status: Option<String>,
    current_mw: Option<f64>,
    setpoint_mw: Option<f64>,
}

async fn history_telemetry(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(range): Query<TimeRange>,
) -> Response {
    // If we don't have a DB configured, return an empty history so callers
    // don't get a 501. Beginners: this keeps the API predictable even in
    // in-memory mode.
    let Some(db) = state.db.as_ref() else {
        return Json(Vec::<TelemetryRow>::new()).into_response();
    };
    // Parse optional start/end params as RFC3339.
    let start = match range.start {
        Some(ref s) => match DateTime::parse_from_rfc3339(s) {
            Ok(dt) => Some(dt.with_timezone(&Utc)),
            Err(_) => return StatusCode::BAD_REQUEST.into_response(),
        },
        None => None,
    };
    let end = match range.end {
        Some(ref s) => match DateTime::parse_from_rfc3339(s) {
            Ok(dt) => Some(dt.with_timezone(&Utc)),
            Err(_) => return StatusCode::BAD_REQUEST.into_response(),
        },
        None => None,
    };

    let mut builder = sqlx::QueryBuilder::new(
        r#"
        SELECT
            t.ts,
            COALESCE(a.name, '<unknown>') AS asset_name,
            COALESCE(a.site_name, '<unknown>') AS site_name,
            t.soc_mwhr,
            t.soc_pct,
            t.capacity_mwhr,
            t.current_mw,
            t.setpoint_mw,
            t.max_mw,
            t.min_mw,
            t.status,
            t.asset_id,
            t.site_id
        FROM telemetry t
        LEFT JOIN assets a ON t.asset_id = a.id
        WHERE t.asset_id = "#,
    );
    builder.push_bind(id);
    if let Some(start) = start {
        builder.push(" AND t.ts >= ").push_bind(start);
    }
    if let Some(end) = end {
        builder.push(" AND t.ts <= ").push_bind(end);
    }
    builder.push(" ORDER BY t.ts DESC LIMIT 100");

    let rows = builder.build_query_as::<TelemetryRow>().fetch_all(db).await;

    match rows {
        Ok(rows) => Json(rows).into_response(),
        Err(err) => {
            tracing::error!("history query failed: {err}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn ingest_telemetry(State(state): State<AppState>, Json(t): Json<Telemetry>) -> Response {
    // Store the latest snapshot in-memory for fast GET /telemetry/:id responses.
    {
        let mut map = state.latest.write().await;
        map.insert(t.asset_id, t.clone());
    }

    // Optionally persist to Postgres if configured.
    if let Some(db) = state.db.as_ref() {
        if let Err(err) = persist_telemetry(db, &[t.clone()]).await {
            tracing::warn!("failed to persist pushed telemetry: {err}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }
    StatusCode::ACCEPTED.into_response()
}

async fn create_dispatch(
    State(state): State<AppState>,
    Json(req): Json<IncomingDispatch>,
) -> Response {
    // Validate mutual exclusivity.
    if req.asset_id.is_some() == req.site_id.is_some() {
        return StatusCode::BAD_REQUEST.into_response();
    }

    // Site dispatch fan-out.
    if let Some(site_id) = req.site_id {
        // Online assets for this site.
        let online_assets: Vec<Uuid> = {
            let streams = state.agent_streams.read().await;
            streams
                .iter()
                .filter(|(_, s)| s.site_id == site_id)
                .map(|(id, _)| *id)
                .collect()
        };
        if online_assets.is_empty() {
            tracing::error!("dispatch failed: no online assets for site_id={}", site_id);
            return StatusCode::BAD_REQUEST.into_response();
        }

        // Fetch asset metadata for weighting/clamping.
        let mut assets_meta: Vec<Asset> = {
            let sim = state.sim.read().await;
            online_assets
                .iter()
                .filter_map(|id| sim.assets.get(id).cloned())
                .collect()
        };
        // Deterministic order so allocation/residual behavior is stable across runs.
        assets_meta.sort_by_key(|a| a.id);
        if assets_meta.is_empty() {
            tracing::error!("dispatch failed: metadata missing for site_id={}", site_id);
            return StatusCode::BAD_REQUEST.into_response();
        }

        let allocations = compute_site_allocations(&assets_meta, req.mw);
        tracing::info!(
            "site dispatch split site_id={} mw_total={} allocations={:?}",
            site_id,
            req.mw,
            allocations
        );
        if allocations.iter().any(|a| a.clamped) {
            tracing::info!(
                "site dispatch clamped allocations site_id={} mw_total={}",
                site_id,
                req.mw
            );
        }

        let mut results = Vec::new();
        for alloc in allocations.iter() {
            let one = IncomingDispatch {
                asset_id: Some(alloc.asset_id),
                site_id: Some(site_id),
                mw: alloc.mw,
                duration_s: req.duration_s,
            };
            match handle_single_dispatch(&state, one, alloc.clamped).await {
                Ok(dispatch) => results.push(dispatch),
                Err(err) => {
                    tracing::error!(
                        "dispatch failed for asset_id={} site_id={}: {err}",
                        alloc.asset_id,
                        site_id
                    );
                }
            }
        }
        return Json(SiteDispatchResult {
            allocations,
            dispatches: results,
        })
        .into_response();
    }

    // Single-asset dispatch.
    match handle_single_dispatch(&state, req, false).await {
        Ok(dispatch) => Json(dispatch).into_response(),
        Err(err) => {
            tracing::error!("dispatch failed: {err}");
            StatusCode::BAD_REQUEST.into_response()
        }
    }
}

async fn handle_single_dispatch(
    state: &AppState,
    req: IncomingDispatch,
    clamped: bool,
) -> Result<Dispatch> {
    let asset_id = req
        .asset_id
        .context("asset_id required for single dispatch")?;
    // Cache limits up front so we can log them without holding the write lock.
    let cached_meta = {
        let sim = state.sim.read().await;
        sim.assets.get(&asset_id).map(|a| {
            (
                a.min_mw,
                a.max_mw,
                a.name.clone(),
                a.site_name.clone(),
                a.site_id,
            )
        })
    };

    // Log the incoming dispatch payload with asset/site context.
    if let Some((min_mw, max_mw, asset_name, site_name, _)) = cached_meta.clone() {
        tracing::info!(
            "dispatch request asset_id={} asset_name={} site_name={} mw_req={} min_mw={} max_mw={}",
            asset_id,
            asset_name,
            site_name,
            req.mw,
            min_mw,
            max_mw
        );
    } else {
        tracing::info!(
            "dispatch request asset_id={} (unknown asset) mw_req={}",
            asset_id,
            req.mw
        );
    }

    // Acquire write access to update the setpoint.
    let mut sim = state.sim.write().await;
    let site_id_meta = cached_meta.as_ref().map(|(_, _, _, _, sid)| *sid);
    let dispatch_req = DispatchRequest {
        asset_id,
        site_id: site_id_meta,
        mw: req.mw,
        duration_s: req.duration_s,
        clamped,
    };

    let result = sim.set_dispatch(dispatch_req);
    drop(sim); // release lock before downstream I/O

    match result {
        Ok(dispatch) => {
            // Persist dispatch to DB first so acks can update reliably.
            if let Some(db) = state.db.as_ref() {
                if let Err(err) = persist_dispatch(db, &dispatch).await {
                    tracing::warn!("failed to persist dispatch: {err}");
                }
            }

            // If we know the agent stream, push the setpoint downstream.
            if let Err(err) = push_setpoint_to_agent(state, &dispatch).await {
                tracing::warn!("failed to forward setpoint to agent: {err}");
                // Keep the pending setpoint so it can be delivered when the agent reconnects.
                state
                    .pending_setpoints
                    .write()
                    .await
                    .insert(dispatch.asset_id, dispatch.clone());
            }
            Ok(dispatch)
        }
        Err(err) => {
            let (min_mw, max_mw, asset_name, site_name, _site_id) = cached_meta.unwrap_or((
                f64::NAN,
                f64::NAN,
                "<unknown>".into(),
                "<unknown>".into(),
                Uuid::nil(),
            ));
            let peer = {
                let streams = state.agent_streams.read().await;
                streams
                    .get(&asset_id)
                    .map(|s| s.peer.clone())
                    .unwrap_or_else(|| "<not_connected>".into())
            };
            tracing::error!(
                "dispatch failed: {err} asset_id={} asset_name={} site_name={} mw_req={} min_mw={} max_mw={} peer={}",
                asset_id,
                asset_name,
                site_name,
                req.mw,
                min_mw,
                max_mw,
                peer
            );
            Err(err)
        }
    }
}

/// Compute per-asset MW allocations for a site dispatch using capacity weights with clamping.
fn compute_site_allocations(assets: &[Asset], mw_total: f64) -> Vec<AllocationView> {
    if assets.is_empty() {
        return Vec::new();
    }
    let cap_sum: f64 = assets.iter().map(|a| a.capacity_mwhr).sum();
    if cap_sum <= f64::EPSILON {
        return assets
            .iter()
            .map(|a| AllocationView {
                asset_id: a.id,
                mw_raw: 0.0,
                mw: 0.0,
                clamped: false,
            })
            .collect();
    }

    let raw: Vec<f64> = assets
        .iter()
        .map(|a| mw_total * a.capacity_mwhr / cap_sum)
        .collect();
    let mut clamped: Vec<f64> = raw
        .iter()
        .zip(assets.iter())
        .map(|(mw, a)| mw.clamp(a.min_mw, a.max_mw))
        .collect();

    // Adjust residual to try to hit mw_total after clamping.
    let tolerance = 1e-6;
    let mut residual = mw_total - clamped.iter().sum::<f64>();
    let mut attempts = 0;
    while residual.abs() > tolerance && attempts < 3 {
        attempts += 1;
        let direction_positive = residual > 0.0;
        // Compute available headroom in the direction of the residual.
        let headrooms: Vec<f64> = clamped
            .iter()
            .zip(assets.iter())
            .map(|(val, a)| {
                if direction_positive {
                    a.max_mw - *val
                } else {
                    a.min_mw - *val
                }
            })
            .collect();
        let total_headroom: f64 = headrooms
            .iter()
            .filter(|h| {
                if direction_positive {
                    **h > 0.0
                } else {
                    **h < 0.0
                }
            })
            .map(|h| h.abs())
            .sum();
        if total_headroom <= f64::EPSILON {
            break;
        }

        for ((val, headroom), asset) in clamped.iter_mut().zip(headrooms.iter()).zip(assets.iter())
        {
            if (direction_positive && *headroom <= 0.0) || (!direction_positive && *headroom >= 0.0)
            {
                continue;
            }
            let share = headroom.abs() / total_headroom;
            let delta = residual * share;
            let candidate = *val + delta;
            *val = candidate.clamp(asset.min_mw, asset.max_mw);
        }
        residual = mw_total - clamped.iter().sum::<f64>();
    }

    let weights: Vec<f64> = assets.iter().map(|a| a.capacity_mwhr / cap_sum).collect();
    let debug_rows: Vec<AllocationDebugView> = assets
        .iter()
        .enumerate()
        .map(|(i, a)| AllocationDebugView {
            asset_id: a.id,
            capacity_mwhr: a.capacity_mwhr,
            weight: weights.get(i).copied().unwrap_or(0.0),
            raw_mw: raw.get(i).copied().unwrap_or(0.0),
            clamped_mw: clamped.get(i).copied().unwrap_or(0.0),
            min_mw: a.min_mw,
            max_mw: a.max_mw,
        })
        .collect();

    tracing::info!(
        "site dispatch allocation: mw_total={} residual={} rows={:?}",
        mw_total,
        residual,
        debug_rows
    );

    assets
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let raw_mw = raw.get(i).copied().unwrap_or(0.0);
            let mw = clamped.get(i).copied().unwrap_or(0.0);
            let clamped_flag = (raw_mw - mw).abs() > 1e-6;
            if clamped_flag {
                tracing::info!(
                    "dispatch allocation clamped asset_id={} raw_mw={} clamped_mw={}",
                    a.id,
                    raw_mw,
                    mw
                );
            }
            AllocationView {
                asset_id: a.id,
                mw_raw: raw_mw,
                mw,
                clamped: clamped_flag,
            }
        })
        .collect()
}

async fn push_setpoint_to_agent(state: &AppState, dispatch: &Dispatch) -> Result<()> {
    let site_id = {
        let sim = state.sim.read().await;
        sim.assets.get(&dispatch.asset_id).map(|a| a.site_id)
    };

    let streams = state.agent_streams.read().await;
    let Some(agent) = streams.get(&dispatch.asset_id) else {
        anyhow::bail!("agent stream not connected for asset {}", dispatch.asset_id);
    };

    agent
        .tx
        .send(HeadendToAgent {
            msg: Some(headend_to_agent::Msg::Setpoint(Setpoint {
                asset_id: dispatch.asset_id.to_string(),
                mw: dispatch.mw,
                duration_s: dispatch.duration_s.map(|d| d as u64),
                site_id: site_id.map(|s| s.to_string()),
                group_id: None,
                dispatch_id: Some(dispatch.id.to_string()),
            })),
        })
        .await
        .context("sending setpoint over gRPC")?;
    Ok(())
}

async fn record_agent_connect(
    db: &PgPool,
    asset_id: Uuid,
    peer: &str,
    asset_name: &str,
    site_name: &str,
) -> Result<()> {
    // Defensive: if we see a duplicate Register (or a reconnect without a clean disconnect),
    // ensure we don't accumulate multiple "open" sessions for the same asset.
    sqlx::query(
        r#"
        UPDATE agent_sessions
        SET disconnected_at = now()
        WHERE asset_id = $1 AND disconnected_at IS NULL
        "#,
    )
    .bind(asset_id)
    .execute(db)
    .await
    .context("closing prior open agent session (if any)")?;

    sqlx::query(
        r#"
        INSERT INTO agent_sessions (asset_id, peer, asset_name, site_name, connected_at)
        VALUES ($1, $2, $3, $4, now())
        "#,
    )
    .bind(asset_id)
    .bind(peer)
    .bind(asset_name)
    .bind(site_name)
    .execute(db)
    .await
    .context("inserting agent session start")?;
    Ok(())
}

async fn record_agent_disconnect(db: &PgPool, asset_id: Uuid) -> Result<()> {
    sqlx::query(
        r#"
        UPDATE agent_sessions
        SET disconnected_at = now()
        WHERE asset_id = $1 AND disconnected_at IS NULL
        "#,
    )
    .bind(asset_id)
    .execute(db)
    .await
    .context("updating agent session end")?;
    Ok(())
}

/// Return currently connected agents from in-memory registry.
#[derive(Serialize)]
struct AgentView {
    asset_id: Uuid,
    asset_name: String,
    site_id: Uuid,
    site_name: String,
    peer: String,
    connected: bool,
    connected_at: Option<DateTime<Utc>>,
    sessions: Vec<AgentSessionView>,
}

#[derive(Serialize)]
struct AgentSessionView {
    peer: String,
    connected_at: DateTime<Utc>,
    disconnected_at: Option<DateTime<Utc>>,
}

#[derive(sqlx::FromRow)]
struct AgentSessionRow {
    asset_id: Uuid,
    asset_name: Option<String>,
    site_name: Option<String>,
    peer: String,
    connected_at: DateTime<Utc>,
    disconnected_at: Option<DateTime<Utc>>,
}

#[derive(Serialize, Debug, Clone)]
struct AllocationView {
    asset_id: Uuid,
    mw_raw: f64,
    mw: f64,
    clamped: bool,
}

#[derive(Serialize, Debug)]
struct AllocationDebugView {
    asset_id: Uuid,
    capacity_mwhr: f64,
    weight: f64,
    raw_mw: f64,
    clamped_mw: f64,
    min_mw: f64,
    max_mw: f64,
}

#[derive(Serialize, Debug)]
struct SiteDispatchResult {
    allocations: Vec<AllocationView>,
    dispatches: Vec<Dispatch>,
}

async fn list_agents(State(state): State<AppState>) -> Response {
    // Lookup configured asset metadata so /agents can always include site_id/site_name.
    let asset_meta: HashMap<Uuid, (Uuid, String, String)> = {
        let sim = state.sim.read().await;
        sim.assets
            .iter()
            .map(|(id, a)| (*id, (a.site_id, a.name.clone(), a.site_name.clone())))
            .collect()
    };

    // Current connections (in-memory).
    let current = state.agent_streams.read().await.clone();

    // Recent sessions from DB (if available); limited to last 5 per asset.
    let mut history: HashMap<Uuid, Vec<AgentSessionView>> = HashMap::new();
    let mut names: HashMap<Uuid, (String, String)> = HashMap::new();
    if let Some(db) = state.db.as_ref() {
        if let Ok(rows) = sqlx::query_as::<_, AgentSessionRow>(
            r#"
            SELECT asset_id, asset_name, site_name, peer, connected_at, disconnected_at
            FROM agent_sessions
            ORDER BY connected_at DESC
            LIMIT 200
            "#,
        )
        .fetch_all(db)
        .await
        {
            for row in rows {
                let entry = history.entry(row.asset_id).or_default();
                if entry.len() < 5 {
                    entry.push(AgentSessionView {
                        peer: row.peer,
                        connected_at: row.connected_at,
                        disconnected_at: row.disconnected_at,
                    });
                }
                names.insert(
                    row.asset_id,
                    (
                        row.asset_name.unwrap_or_else(|| "<unknown>".into()),
                        row.site_name.unwrap_or_else(|| "<unknown>".into()),
                    ),
                );
            }
        }
    }

    // Merge current status with history; ensure current peer is first if connected.
    let mut list = Vec::new();
    for (asset_id, stream) in current.iter() {
        let mut sessions = history.remove(asset_id).unwrap_or_default();
        // Avoid duplicate "open" sessions.
        // If the DB already has an open session anywhere in the list, update its peer to
        // the current peer and drop any additional open sessions.
        let mut found_open = false;
        for s in sessions.iter_mut() {
            if s.disconnected_at.is_none() {
                if !found_open {
                    s.peer = stream.peer.clone();
                    found_open = true;
                } else {
                    // Mark extras to be removed below.
                    s.disconnected_at = Some(stream.connected_at);
                }
            }
        }

        if !found_open {
            sessions.insert(
                0,
                AgentSessionView {
                    peer: stream.peer.clone(),
                    connected_at: stream.connected_at,
                    disconnected_at: None,
                },
            );
        }

        // Now drop any session entries that are still "open" beyond the first.
        // (After the loop above, extras will have been marked disconnected.)
        let mut open_seen = false;
        sessions.retain(|s| {
            if s.disconnected_at.is_none() {
                if open_seen {
                    false
                } else {
                    open_seen = true;
                    true
                }
            } else {
                true
            }
        });

        // Prefer configured metadata when available (more stable than what the agent sent).
        let (site_id, asset_name, site_name) =
            asset_meta.get(asset_id).cloned().unwrap_or_else(|| {
                (
                    stream.site_id,
                    stream.asset_name.clone(),
                    stream.site_name.clone(),
                )
            });

        list.push(AgentView {
            asset_id: *asset_id,
            asset_name,
            site_id,
            site_name,
            peer: stream.peer.clone(),
            connected: true,
            connected_at: sessions.first().map(|s| s.connected_at),
            sessions,
        });
    }

    // Include recent but currently disconnected agents (from history only).
    for (asset_id, sessions) in history.into_iter() {
        let (asset_name, site_name) = names
            .get(&asset_id)
            .cloned()
            .unwrap_or_else(|| ("<unknown>".into(), "<unknown>".into()));

        let (site_id, asset_name_cfg, site_name_cfg) = asset_meta
            .get(&asset_id)
            .cloned()
            .unwrap_or_else(|| (Uuid::nil(), asset_name.clone(), site_name.clone()));

        // Prefer configured names when present.
        let asset_name = if asset_name_cfg.is_empty() {
            asset_name
        } else {
            asset_name_cfg
        };
        let site_name = if site_name_cfg.is_empty() {
            site_name
        } else {
            site_name_cfg
        };

        list.push(AgentView {
            asset_id,
            asset_name,
            site_id,
            site_name,
            peer: sessions
                .first()
                .map(|s| s.peer.clone())
                .unwrap_or_else(|| "<not_connected>".into()),
            connected: false,
            connected_at: sessions.first().map(|s| s.connected_at),
            sessions,
        });
    }

    Json(list).into_response()
}

#[derive(Serialize, sqlx::FromRow)]
struct DispatchRow {
    id: Uuid,
    asset_id: Uuid,
    asset_name: String,
    site_name: String,
    mw: f64,
    duration_s: Option<i64>,
    status: String,
    reason: Option<String>,
    submitted_at: DateTime<Utc>,
    clamped: bool,
    ack_status: Option<String>,
    acked_at: Option<DateTime<Utc>>,
}

#[derive(Serialize, sqlx::FromRow)]
struct EventRow {
    id: Uuid,
    asset_id: Uuid,
    asset_name: String,
    site_id: Uuid,
    site_name: String,
    ts: DateTime<Utc>,
    event_type: String,
    severity: String,
    message: String,
}

#[derive(Serialize, Deserialize)]
struct EventRequest {
    asset_id: Uuid,
    event_type: String,
    severity: Option<String>,
    message: Option<String>,
}

/// Return recent dispatch records (DB-backed). Empty array if no DB configured.
async fn list_dispatch_history(State(state): State<AppState>) -> Response {
    let Some(db) = state.db.as_ref() else {
        return Json(Vec::<DispatchRow>::new()).into_response();
    };
    let rows = sqlx::query_as::<_, DispatchRow>(
        r#"
        SELECT
            d.id,
            d.asset_id,
            COALESCE(a.name, '<unknown>') AS asset_name,
            COALESCE(a.site_name, '<unknown>') AS site_name,
            d.mw,
            d.duration_s,
            d.status,
            d.reason,
            d.submitted_at,
            d.clamped,
            d.ack_status,
            d.acked_at
        FROM dispatches d
        LEFT JOIN assets a ON d.asset_id = a.id
        ORDER BY d.submitted_at DESC
        LIMIT 200
        "#,
    )
    .fetch_all(db)
    .await;

    match rows {
        Ok(list) => Json(list).into_response(),
        Err(err) => {
            tracing::error!("dispatch history query failed: {err}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn create_event(State(state): State<AppState>, Json(req): Json<EventRequest>) -> Response {
    let Some(db) = state.db.as_ref() else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let asset = {
        let sim = state.sim.read().await;
        sim.assets.get(&req.asset_id).cloned()
    };
    let Some(asset) = asset else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let event = BessEvent {
        id: Uuid::new_v4(),
        asset_id: req.asset_id,
        site_id: asset.site_id,
        timestamp: Utc::now(),
        event_type: req.event_type,
        severity: req.severity.unwrap_or_else(|| "warning".into()),
        message: req.message.unwrap_or_default(),
    };
    if let Err(err) = persist_event(db, &event).await {
        tracing::warn!("failed to persist event: {err}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    Json(event).into_response()
}

async fn list_event_history(State(state): State<AppState>, Path(id): Path<Uuid>) -> Response {
    let Some(db) = state.db.as_ref() else {
        return Json(Vec::<EventRow>::new()).into_response();
    };
    let rows = sqlx::query_as::<_, EventRow>(
        r#"
        SELECT
            e.id,
            e.asset_id,
            COALESCE(a.name, '<unknown>') AS asset_name,
            e.site_id,
            COALESCE(a.site_name, '<unknown>') AS site_name,
            e.ts,
            e.event_type,
            e.severity,
            e.message
        FROM events e
        LEFT JOIN assets a ON e.asset_id = a.id
        WHERE e.asset_id = $1
        ORDER BY e.ts DESC
        LIMIT 100
        "#,
    )
    .bind(id)
    .fetch_all(db)
    .await;

    match rows {
        Ok(list) => Json(list).into_response(),
        Err(err) => {
            tracing::error!("event history query failed: {err}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Latest heartbeat for an asset.
async fn latest_heartbeat(State(state): State<AppState>, Path(id): Path<Uuid>) -> Response {
    let Some(db) = state.db.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let row = sqlx::query_as::<_, HeartbeatRow>(
        r#"
        SELECT
            h.asset_id,
            h.ts,
            COALESCE(a.name, '<unknown>') AS asset_name,
            COALESCE(a.site_name, '<unknown>') AS site_name
        FROM heartbeats h
        LEFT JOIN assets a ON h.asset_id = a.id
        WHERE h.asset_id = $1
        ORDER BY h.ts DESC
        LIMIT 1
        "#,
    )
    .bind(id)
    .fetch_optional(db)
    .await;

    match row {
        Ok(Some(row)) => Json(row).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(err) => {
            tracing::error!("heartbeat latest query failed: {err}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Last 100 heartbeats for an asset.
async fn history_heartbeats(State(state): State<AppState>, Path(id): Path<Uuid>) -> Response {
    let Some(db) = state.db.as_ref() else {
        return Json(Vec::<HeartbeatRow>::new()).into_response();
    };

    let rows = sqlx::query_as::<_, HeartbeatRow>(
        r#"
        SELECT
            h.asset_id,
            h.ts,
            COALESCE(a.name, '<unknown>') AS asset_name,
            COALESCE(a.site_name, '<unknown>') AS site_name
        FROM heartbeats h
        LEFT JOIN assets a ON h.asset_id = a.id
        WHERE h.asset_id = $1
        ORDER BY h.ts DESC
        LIMIT 100
        "#,
    )
    .bind(id)
    .fetch_all(db)
    .await;

    match rows {
        Ok(rows) => Json(rows).into_response(),
        Err(err) => {
            tracing::error!("heartbeat history query failed: {err}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
