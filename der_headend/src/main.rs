//! Headend service for the multi-crate workspace.
//! - Loads BESS assets from YAML (with sites) into a simulator.
//! - Optionally connects to Postgres/Timescale to persist assets and telemetry.
//! - Exposes a REST-ish API for assets, live telemetry, history, and dispatch.
//! - Uses `sim_core` for shared models and the tick step so agents/headend stay aligned.
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
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sim_core::{tick_asset, Asset, BessState, Dispatch, DispatchRequest, Telemetry};
use sqlx::{postgres::PgPoolOptions, PgPool};
use tokio::sync::{mpsc, RwLock};
use tokio_stream::{wrappers::ReceiverStream, StreamExt as TokioStreamExt};
use tonic::{transport::Server, Request, Response as GrpcResponse, Status};
use tower_http::trace::TraceLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;

pub mod proto {
    tonic::include_proto!("agent");
}
use proto::{
    agent_link_server::{AgentLink, AgentLinkServer},
    agent_to_headend, headend_to_agent, AgentToHeadend, HeadendToAgent, Register, Setpoint,
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
    // Last peer IP for logging.
    peer_ip: Option<String>,
}

/// Connection info for a live agent stream.
#[derive(Clone)]
struct AgentStream {
    tx: mpsc::Sender<HeadendToAgent>,
    peer: String,
    asset_name: String,
    site_name: String,
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
            // Start each asset at 50% SOC and zero MW.
            let soc = asset.capacity_mwhr * 0.5;
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
        };
        self.dispatches.insert(req.asset_id, dispatch.clone());
        Ok(dispatch)
    }

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
    efficiency: f64,
    ramp_rate_mw_per_min: f64,
}

async fn load_assets_from_yaml() -> Result<Vec<Asset>> {
    // Read the YAML file and map site/asset records into `sim_core::Asset`.
    // Python-ish: `data = yaml.safe_load(open(path))`
    let (_path, raw) = read_assets_file().await?;
    let parsed: AssetsFile = serde_yaml::from_str(&raw).context("parsing assets.yaml")?;
    let sites: HashMap<Uuid, SiteCfg> =
        parsed.sites.into_iter().map(|s| (s.id, s)).collect();

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
            efficiency: cfg.efficiency,
            ramp_rate_mw_per_min: cfg.ramp_rate_mw_per_min,
        });
    }
    Ok(assets)
}

async fn read_assets_file() -> Result<(PathBuf, String)> {
    // Allow an override via ASSETS_PATH, otherwise try common relative locations.
    let candidates = if let Ok(p) = std::env::var("ASSETS_PATH") {
        vec![PathBuf::from(p)]
    } else {
        vec![PathBuf::from("assets.yaml"), PathBuf::from("../assets.yaml")]
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
        .with(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Load assets from YAML (with beginner-friendly error messages).
    let assets = load_assets_from_yaml().await?;
    let simulator = Simulator::from_assets(assets);
    let db = maybe_connect_db().await?;

    // Wrap shared state in Arc<RwLock> for Axum handlers and the tick loop.
    let state = AppState {
        sim: Arc::new(RwLock::new(simulator)),
        db,
        latest: Arc::new(RwLock::new(HashMap::new())),
        agent_streams: Arc::new(RwLock::new(HashMap::new())),
        pending_setpoints: Arc::new(RwLock::new(HashMap::new())),
        peer_ip: None,
    };

    // Spawn the tick loop on a background task.
    //spawn_tick_loop(state.clone());

    // Build the Axum router.
    let app = Router::new()
        .route("/", get(ui_home))
        .route("/assets", get(list_assets))
        .route("/agents", get(list_agents))
        .route("/telemetry/{id}", get(latest_telemetry))
        .route("/telemetry/{id}/history", get(history_telemetry))
        .route("/telemetry", post(ingest_telemetry))
        .route("/dispatch", post(create_dispatch))
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
                    |res: &axum::http::Response<_>, latency: std::time::Duration, span: &tracing::Span| {
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
                    |error: tower_http::classify::ServerErrorsFailureClass, latency: std::time::Duration, span: &tracing::Span| {
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
    let http_addr: SocketAddr = "127.0.0.1:3001".parse().unwrap();
    // Default gRPC port can be overridden by HEADEND_GRPC_ADDR (e.g., 127.0.0.1:50070)
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
    tokio::try_join!(
        async { http.await.map_err(anyhow::Error::from) },
        async { grpc.await.map_err(anyhow::Error::from) }
    )?;
    Ok(())
}

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
    type StreamStream = Pin<Box<dyn futures_util::Stream<Item = Result<HeadendToAgent, Status>> + Send>>;

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
            let mut asset_id: Option<Uuid> = None;
            while let Some(msg) = tokio_stream::StreamExt::next(&mut inbound).await {
                match msg {
                    Ok(AgentToHeadend {
                        msg: Some(agent_to_headend::Msg::Register(Register { asset_id: id, asset_name, site_name, .. })),
                    }) => {
                        if let Ok(uuid) = Uuid::parse_str(&id) {
                            asset_id = Some(uuid);
                            // Remember this stream sender for dispatch push.
                            state.agent_streams.write().await.insert(
                                uuid,
                                AgentStream {
                                    tx: tx.clone(),
                                    peer: peer_ip.clone(),
                                    asset_name: asset_name.clone(),
                                    site_name: site_name.clone(),
                                },
                            );

                            tracing::info!(
                                "agent connected asset_id={} asset_name={} site_name={} peer={}",
                                uuid,
                                asset_name,
                                site_name,
                                peer_ip,
                            );

                            // Persist session start if DB is configured.
                            if let Some(db) = state.db.as_ref() {
                                if let Err(err) =
                                    record_agent_connect(db, uuid, &peer_ip, &asset_name, &site_name).await
                                {
                                    tracing::warn!("failed to record agent connect: {err}");
                                }
                            }

                            // Deliver any pending setpoint.
                            if let Some(pending) =
                                state.pending_setpoints.write().await.remove(&uuid)
                            {
                                let _ = tx
                                    .send(HeadendToAgent {
                                        msg: Some(headend_to_agent::Msg::Setpoint(Setpoint {
                                            asset_id: pending.asset_id.to_string(),
                                            mw: pending.mw,
                                            duration_s: pending.duration_s.map(|d| d as u64),
                                        })),
                                    })
                                    .await;
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
                    Ok(_) => {}
                    Err(err) => {
                        tracing::info!(
                            "agent stream closed/errored (asset={:?}, peer={}): {err}",
                            asset_id,
                            peer_ip
                        );
                        break;
                    }
                }
            }

            // Clean up on disconnect.
            if let Some(id) = asset_id {
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
        persist_telemetry(db, &[snap]).await?;
    }

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
    sqlx::query(r#"ALTER TABLE agent_sessions ADD COLUMN IF NOT EXISTS disconnected_at timestamptz;"#)
        .execute(pool)
        .await
        .context("altering agent_sessions.disconnected_at")?;
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

// --- HTTP handlers ---

async fn ui_home() -> Html<&'static str> {
    // Minimal placeholder UI so you can see something in the browser.
    Html(r#"<!DOCTYPE html><html><body><h1>BESS Headend</h1><p>Use the API: /assets, /telemetry/:id, /telemetry/:id/history, /dispatch</p></body></html>"#)
}

async fn list_assets(State(state): State<AppState>) -> Json<Vec<Asset>> {
    let sim = state.sim.read().await;
    Json(sim.assets())
}

async fn latest_telemetry(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Response {
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

async fn history_telemetry(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Response {
    // If we don't have a DB configured, return an empty history so callers
    // don't get a 501. Beginners: this keeps the API predictable even in
    // in-memory mode.
    let Some(db) = state.db.as_ref() else {
        return Json(Vec::<TelemetryRow>::new()).into_response();
    };
    let rows = sqlx::query_as::<_, TelemetryRow>(
        r#"
        SELECT * FROM telemetry
        WHERE asset_id = $1
        ORDER BY ts DESC
        LIMIT 100
    "#,
    )
    .bind(id)
    .fetch_all(db)
    .await;

    match rows {
        Ok(rows) => Json(rows).into_response(),
        Err(err) => {
            tracing::error!("history query failed: {err}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn ingest_telemetry(
    State(state): State<AppState>,
    Json(t): Json<Telemetry>,
) -> Response {
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
    Json(req): Json<DispatchRequest>,
) -> Response {
    // Cache limits up front so we can log them without holding the write lock.
    let cached_meta = {
        let sim = state.sim.read().await;
        sim.assets.get(&req.asset_id).map(|a| {
            (
                a.min_mw,
                a.max_mw,
                a.name.clone(),
                a.site_name.clone(),
            )
        })
    };

    // Log the incoming dispatch payload with asset/site context.
    if let Some((min_mw, max_mw, asset_name, site_name)) = cached_meta.clone() {
        tracing::info!(
            "dispatch request asset_id={} asset_name={} site_name={} mw_req={} min_mw={} max_mw={}",
            req.asset_id,
            asset_name,
            site_name,
            req.mw,
            min_mw,
            max_mw
        );
    } else {
        tracing::info!(
            "dispatch request asset_id={} (unknown asset) mw_req={}",
            req.asset_id,
            req.mw
        );
    }

    let req_clone = req.clone();
    // Acquire write access to update the setpoint.
    let mut sim = state.sim.write().await;
    match sim.set_dispatch(req) {
        Ok(dispatch) => {
            // If we know the agent stream, push the setpoint downstream.
            if let Err(err) = push_setpoint_to_agent(&state, &dispatch).await {
                tracing::warn!("failed to forward setpoint to agent: {err}");
                // Keep the pending setpoint so it can be delivered when the agent reconnects.
                state
                    .pending_setpoints
                    .write()
                    .await
                    .insert(dispatch.asset_id, dispatch.clone());
            }

            // Return a simple JSON acknowledgment.
            #[derive(Serialize)]
            struct Ack<'a> {
                status: &'a str,
                asset_id: Uuid,
                mw: f64,
                duration_s: Option<u64>,
            }
            Json(Ack {
                status: "ok",
                asset_id: dispatch.asset_id,
                mw: dispatch.mw,
                duration_s: dispatch.duration_s,
            })
            .into_response()
        }
        Err(err) => {
            let (min_mw, max_mw, asset_name, site_name) =
                cached_meta.unwrap_or((f64::NAN, f64::NAN, "<unknown>".into(), "<unknown>".into()));
            let peer = {
                let streams = state.agent_streams.read().await;
                streams
                    .get(&req_clone.asset_id)
                    .map(|s| s.peer.clone())
                    .unwrap_or_else(|| "<not_connected>".into())
            };
            tracing::error!(
                "dispatch failed: {err} asset_id={} asset_name={} site_name={} mw_req={} min_mw={} max_mw={} peer={}",
                req_clone.asset_id,
                asset_name,
                site_name,
                req_clone.mw,
                min_mw,
                max_mw,
                peer
            );
            StatusCode::BAD_REQUEST.into_response()
        }
    }
}

async fn push_setpoint_to_agent(state: &AppState, dispatch: &Dispatch) -> Result<()> {
    let streams = state.agent_streams.read().await;
    let Some(agent) = streams.get(&dispatch.asset_id) else {
        anyhow::bail!("agent stream not connected for asset {}", dispatch.asset_id);
    };

    agent.tx.send(HeadendToAgent {
        msg: Some(headend_to_agent::Msg::Setpoint(Setpoint {
            asset_id: dispatch.asset_id.to_string(),
            mw: dispatch.mw,
            duration_s: dispatch.duration_s.map(|d| d as u64),
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
    site_name: String,
    peer: String,
}

async fn list_agents(State(state): State<AppState>) -> Response {
    let agents = state.agent_streams.read().await;
    let list: Vec<AgentView> = agents
        .iter()
        .map(|(id, s)| AgentView {
            asset_id: *id,
            asset_name: s.asset_name.clone(),
            site_name: s.site_name.clone(),
            peer: s.peer.clone(),
        })
        .collect();
    Json(list).into_response()
}
