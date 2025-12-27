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
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use axum::{Router, routing::{get, post}};
use tonic::codec::CompressionEncoding;
use chrono::Utc;
use sim_core::{Asset, BessState, Dispatch, DispatchRequest, Telemetry, tick_asset};
use sqlx::PgPool;
use tokio::sync::RwLock;
use tonic::transport::Server;
use tower_http::trace::TraceLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;

pub mod proto {
    tonic::include_proto!("agent");
}
mod db;
mod models;
mod http;
mod grpc;
use proto::agent_link_server::AgentLinkServer;

use crate::db::{maybe_connect_db, persist_assets, persist_telemetry};
use crate::models::{AssetsFile, SiteCfg, TelemetrySeed};
use crate::http::{
    create_dispatch, create_event, get_asset, health, history_heartbeats, history_telemetry,
    ingest_telemetry, latest_heartbeat, latest_telemetry, list_agents, list_assets,
    list_dispatch_history, list_event_history, list_sites, ui_home,
};
use crate::grpc::{AgentStream, GrpcApi, SocState};

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
