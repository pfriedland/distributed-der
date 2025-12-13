//! BESS simulator scaffold with a simple linear model.
//! - Seeds assets from assets.yaml (or defaults) with stable UUIDs.
//! - Ticks every 4 seconds to advance SOC/current MW.
//! - Exposes REST endpoints for assets, telemetry, and dispatch.
//! - Persists assets/telemetry/dispatch records to Postgres/Timescale if `DATABASE_URL` is set.
//! Comments are written for Rust beginners—explaining state, async, and DB usage.

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};
use tokio::sync::RwLock;
use tower_http::trace::TraceLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
    // Simulator state behind an async RwLock so many readers / single writer.
    sim: Arc<RwLock<Simulator>>,
    // Optional Postgres pool; None means run in-memory only.
    db: Option<PgPool>,
}

#[derive(Clone, Serialize, Deserialize, sqlx::FromRow)]
struct Asset {
    id: Uuid,
    name: String,
    location: String,
    capacity_mwhr: f64,
    max_mw: f64,
    min_mw: f64,
    efficiency: f64,           // simple scalar for both charge/discharge
    ramp_rate_mw_per_min: f64, // linear ramp constraint
    site_id: Uuid,
    site_name: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct Telemetry {
    asset_id: Uuid,
    site_id: Uuid,
    site_name: String,
    timestamp: DateTime<Utc>,
    soc_mwhr: f64,
    soc_pct: f64,
    capacity_mwhr: f64,
    current_mw: f64,
    setpoint_mw: f64,
    max_mw: f64,
    min_mw: f64,
    status: String,
}

#[derive(sqlx::FromRow, Serialize)]
struct TelemetryRow {
    asset_id: Uuid,
    site_id: Uuid,
    site_name: Option<String>,
    name: String,
    location: String,
    ramp_rate_mw_per_min: f64,
    ts: DateTime<Utc>,
    soc_mwhr: f64,
    soc_pct: Option<f64>,
    capacity_mwhr: f64,
    current_mw: f64,
    setpoint_mw: f64,
    max_mw: f64,
    min_mw: f64,
    status: String,
}

#[derive(sqlx::FromRow)]
struct LastStateRow {
    asset_id: Uuid,
    soc_mwhr: f64,
    current_mw: f64,
    setpoint_mw: f64,
}

#[derive(Clone, Serialize, Deserialize)]
struct Dispatch {
    id: Uuid,
    asset_id: Uuid,
    mw: f64, // positive = discharge, negative = charge
    duration_s: Option<u64>,
    status: String,
    reason: Option<String>,
    submitted_at: DateTime<Utc>,
}

#[derive(Clone)]
struct BessState {
    soc_mwhr: f64,
    current_mw: f64,
    setpoint_mw: f64,
}

#[derive(Clone)]
struct Simulator {
    assets: HashMap<Uuid, Asset>,
    state: HashMap<Uuid, BessState>,
    dispatches: HashMap<Uuid, Dispatch>,
}

impl Simulator {
    fn seed() -> Self {
        Self::from_assets(default_assets())
    }

    // Build in-memory maps from a list of assets.
    fn from_assets(assets: Vec<Asset>) -> Self {
        let mut assets_map = HashMap::new();
        let mut state = HashMap::new();
        for asset in assets {
            let id = asset.id;
            state.insert(
                id,
                BessState {
                    soc_mwhr: asset.capacity_mwhr * 0.5,
                    current_mw: 0.0,
                    setpoint_mw: 0.0,
                },
            );
            assets_map.insert(id, asset);
        }
        Self {
            assets: assets_map,
            state,
            dispatches: HashMap::new(),
        }
    }

    /// Advance simulation by `dt` seconds and return telemetry snapshots.
    fn tick(&mut self, dt_secs: f64) -> Vec<Telemetry> {
        let mut telemetry = Vec::with_capacity(self.assets.len());
        for (id, asset) in self.assets.iter() {
            if let Some(state) = self.state.get_mut(id) {
                // Ramp toward setpoint.
                let ramp_per_sec = asset.ramp_rate_mw_per_min / 60.0;
                let target = state.setpoint_mw;
                let delta = target - state.current_mw;
                let max_delta = ramp_per_sec * dt_secs;
                let applied_delta = delta.clamp(-max_delta, max_delta);
                state.current_mw =
                    (state.current_mw + applied_delta).clamp(asset.min_mw, asset.max_mw);

                // Update SOC with a simple linear model.
                // Positive MW means discharge (SOC decreases); negative MW charge (SOC increases).
                let energy_delta_mwh = state.current_mw * dt_secs / 3600.0;
                let eff = asset.efficiency;
                let adjusted = if energy_delta_mwh >= 0.0 {
                    energy_delta_mwh / eff
                } else {
                    energy_delta_mwh * eff
                };
                state.soc_mwhr = (state.soc_mwhr - adjusted).clamp(0.0, asset.capacity_mwhr);

                let status = if state.current_mw > 0.1 {
                    "discharging"
                } else if state.current_mw < -0.1 {
                    "charging"
                } else {
                    "idle"
                };

                telemetry.push(Telemetry {
                    asset_id: *id,
                    site_id: asset.site_id,
                    site_name: asset.site_name.clone(),
                    timestamp: Utc::now(),
                    soc_mwhr: state.soc_mwhr,
                    soc_pct: if asset.capacity_mwhr > 0.0 {
                        (state.soc_mwhr / asset.capacity_mwhr * 100.0).clamp(0.0, 100.0)
                    } else {
                        0.0
                    },
                    capacity_mwhr: asset.capacity_mwhr,
                    current_mw: state.current_mw,
                    setpoint_mw: state.setpoint_mw,
                    max_mw: asset.max_mw,
                    min_mw: asset.min_mw,
                    status: status.to_string(),
                });
            }
        }
        telemetry
    }

    fn set_dispatch(&mut self, req: DispatchRequest) -> Result<Dispatch> {
        let asset = self
            .assets
            .get(&req.asset_id)
            .context("asset not found for dispatch")?;

        // Infer setpoint: positive = discharge, negative = charge.
        let desired = req.mw;

        if desired > asset.max_mw || desired < asset.min_mw {
            anyhow::bail!("setpoint outside asset limits");
        }

        if let Some(state) = self.state.get_mut(&req.asset_id) {
            state.setpoint_mw = desired;
        }

        let dispatch = Dispatch {
            id: Uuid::new_v4(),
            asset_id: req.asset_id,
            mw: req.mw,
            duration_s: req.duration_s,
            status: "accepted".to_string(),
            reason: None,
            submitted_at: Utc::now(),
        };
        self.dispatches.insert(dispatch.id, dispatch.clone());
        Ok(dispatch)
    }
}

#[derive(Deserialize)]
struct DispatchRequest {
    asset_id: Uuid,
    mw: f64, // positive = discharge, negative = charge
    duration_s: Option<u64>,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    // 1) Load configuration (YAML or defaults) and build the in-memory simulator.
    let db = init_db().await?;

    let asset_cfg = load_asset_config();
    let cfg_assets = asset_cfg.clone().to_assets();
    let sim = Arc::new(RwLock::new(Simulator::from_assets(cfg_assets.clone())));

    if let Some(pool) = &db {
        // 2) Keep DB in sync with config: upsert sites/assets, prune orphan records.
        reconcile_db(pool, &asset_cfg).await?;
        // 3) Restore last known SOC/current/setpoint from telemetry history if present.
        apply_last_state_from_db(pool, sim.clone()).await?;
        // 3) Reapply last known dispatch setpoints so restarts keep behavior.
        apply_saved_setpoints(pool, sim.clone()).await?;
    }

    // 4) Spawn simulation loop (4s tick) to advance SOC/current MW and write telemetry.
    let sim_for_task = sim.clone();
    let db_for_task = db.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(4));
        loop {
            interval.tick().await;
            let snapshots = {
                let mut guard = sim_for_task.write().await;
                guard.tick(4.0)
            };
            if let Some(pool) = &db_for_task {
                if let Err(err) = persist_telemetry(pool, &snapshots).await {
                    tracing::error!(?err, "failed to persist telemetry");
                }
            }
        }
    });

    let app_state = AppState { sim, db };
    let app = Router::new()
        .route("/", get(ui_index))
        .route("/history.html", get(ui_history))
        .route("/assets", get(list_assets))
        .route("/assets/{id}", get(get_asset))
        .route("/telemetry/{id}", get(get_latest_telemetry))
        .route("/telemetry/{id}/history", get(get_telemetry_history))
        .route("/dispatch", post(create_dispatch))
        .with_state(app_state)
        .layer(TraceLayer::new_for_http());

    let addr: SocketAddr = "127.0.0.1:3001".parse().unwrap();
    tracing::info!("bess simulator listening on http://{addr}");

    axum::serve(tokio::net::TcpListener::bind(addr).await?, app).await?;
    Ok(())
}

async fn list_assets(State(state): State<AppState>) -> impl IntoResponse {
    // Simple read-only list of all assets currently in the simulator.
    let sim = state.sim.read().await;
    Json(sim.assets.values().cloned().collect::<Vec<_>>())
}

async fn get_asset(State(state): State<AppState>, Path(id): Path<Uuid>) -> Response {
    // Fetch a single asset by id.
    let sim = state.sim.read().await;
    match sim.assets.get(&id) {
        Some(asset) => Json(asset).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn get_latest_telemetry(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    // Build a one-off telemetry snapshot from in-memory state.
    let sim = state.sim.read().await;
    if let Some(asset) = sim.assets.get(&id) {
        if let Some(state) = sim.state.get(&id) {
            let telemetry = Telemetry {
                asset_id: asset.id,
                site_id: asset.site_id,
                site_name: asset.site_name.clone(),
                timestamp: Utc::now(),
                soc_mwhr: state.soc_mwhr,
                soc_pct: if asset.capacity_mwhr > 0.0 {
                    (state.soc_mwhr / asset.capacity_mwhr * 100.0).clamp(0.0, 100.0)
                } else {
                    0.0
                },
                capacity_mwhr: asset.capacity_mwhr,
                current_mw: state.current_mw,
                setpoint_mw: state.setpoint_mw,
                max_mw: asset.max_mw,
                min_mw: asset.min_mw,
                status: if state.current_mw > 0.1 {
                    "discharging".into()
                } else if state.current_mw < -0.1 {
                    "charging".into()
                } else {
                    "idle".into()
                },
            };
            return Json(telemetry).into_response();
        }
    }
    StatusCode::NOT_FOUND.into_response()
}

async fn get_telemetry_history(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    // DB-backed history; return last 100 records for this asset.
    let pool = match &state.db {
        Some(p) => p,
        None => return (StatusCode::BAD_REQUEST, "database not configured").into_response(),
    };

    let rows: Result<Vec<TelemetryRow>> = sqlx::query_as::<_, TelemetryRow>(
        "SELECT
            t.asset_id,
            t.site_id,
            a.name,
            COALESCE(s.name, a.location) AS site_name,
            a.location,
            a.ramp_rate_mw_per_min,
            t.ts,
            t.soc_mwhr,
            t.soc_pct,
            a.capacity_mwhr,
            t.current_mw,
            t.setpoint_mw,
            t.max_mw,
            t.min_mw,
            t.status
         FROM telemetry t
         JOIN assets a ON a.id = t.asset_id
         LEFT JOIN sites s ON s.id = t.site_id
         WHERE t.asset_id = $1
         ORDER BY t.ts DESC
         LIMIT 100",
    )
    .bind(id)
    .fetch_all(pool)
    .await
    .context("fetch telemetry history");

    match rows {
        Ok(data) => Json(data).into_response(),
        Err(err) => {
            tracing::error!(?err, "history query failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "history query failed").into_response()
        }
    }
}

fn default_assets() -> Vec<Asset> {
    load_asset_config().to_assets()
}

async fn create_dispatch(
    State(state): State<AppState>,
    Json(payload): Json<DispatchRequest>,
) -> impl IntoResponse {
    // Accept a dispatch request and apply a setpoint in-memory (and persist).
    // Note: positive MW = discharge, negative MW = charge.
    let mut sim = state.sim.write().await;
    match sim.set_dispatch(payload) {
        Ok(dispatch) => {
            // Persist dispatch if DB available.
            if let Some(pool) = &state.db {
                if let Err(err) = persist_dispatch(pool, &dispatch).await {
                    tracing::warn!(?err, "failed to persist dispatch");
                }
            }
            (StatusCode::ACCEPTED, Json(dispatch)).into_response()
        }
        Err(err) => {
            let msg = format!("{err}");
            (StatusCode::BAD_REQUEST, msg).into_response()
        }
    }
}

async fn ui_index() -> Html<&'static str> {
    // Simple HTML dashboard using fetch calls to the REST endpoints.
    Html(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <title>BESS Simulator</title>
  <style>
    :root {
      color-scheme: light;
      --bg: #f6f8fb;
      --card: #fff;
      --border: #e3e8f0;
      --text: #111827;
      --muted: #6b7280;
      --accent: #2563eb;
      --accent-light: #dbeafe;
    }
    * { box-sizing: border-box; }
    body { font-family: "Inter", system-ui, -apple-system, sans-serif; margin: 0; background: var(--bg); color: var(--text); }
    header { padding: 1.5rem 2rem; background: #fff; border-bottom: 1px solid var(--border); display: flex; align-items: center; justify-content: space-between; }
    h1 { margin: 0; font-size: 1.5rem; }
    main { padding: 1.5rem 2rem; display: flex; flex-direction: column; gap: 1rem; max-width: 1400px; margin: 0 auto; }
    .card { background: var(--card); border: 1px solid var(--border); padding: 1rem; border-radius: 12px; box-shadow: 0 4px 12px rgba(17,24,39,0.04); width: 100%; }
    .card h2 { margin: 0 0 0.5rem; font-size: 1rem; }
    .card p { margin: 0 0 0.75rem; color: var(--muted); font-size: 0.95rem; }
    pre { background: #0f172a; color: #e5e7eb; padding: 0.75rem; border-radius: 8px; overflow: auto; font-size: 0.9rem; }
    label { display: block; margin-top: 0.5rem; font-weight: 600; font-size: 0.9rem; }
    input { padding: 0.5rem 0.6rem; width: 100%; border: 1px solid var(--border); border-radius: 8px; margin-top: 0.25rem; font-size: 0.95rem; }
    button { margin-top: 0.75rem; padding: 0.55rem 0.9rem; border: none; border-radius: 8px; background: var(--accent); color: #fff; cursor: pointer; font-weight: 600; }
    button.secondary { background: #fff; color: var(--text); border: 1px solid var(--border); }
    .pill { display: inline-block; padding: 0.2rem 0.6rem; border-radius: 999px; background: var(--accent-light); color: var(--accent); font-weight: 600; font-size: 0.8rem; }
    .row { display: flex; gap: 0.5rem; align-items: center; flex-wrap: wrap; }
    table { width: 100%; border-collapse: collapse; font-size: 0.95rem; }
    th, td { padding: 0.35rem 0.5rem; text-align: left; border-bottom: 1px solid var(--border); }
    th { font-weight: 700; color: var(--muted); }
  </style>
</head>
<body>
  <header>
    <h1>BESS Simulator Dashboard</h1>
    <div class="row">
      <button class="secondary" onclick="loadAssets()">Refresh Assets</button>
    </div>
  </header>
  <main>
    <div class="card">
      <h2>Live Telemetry (auto-refresh)</h2>
      <p>Polling every 4s for all assets.</p>
      <div id="telemetry-table">Waiting for data...</div>
    </div>

    <div class="card">
      <h2>Create Dispatch</h2>
      <p>Set a charge/discharge setpoint.</p>
      <label>Asset
        <select id="dispatch-asset-id"></select>
      </label>
      <label>MW (positive=discharge, negative=charge) <input id="dispatch-mw" type="number" value="5"></label>
      <button onclick="createDispatch()">Send Dispatch</button>
      <pre id="dispatch-result"></pre>
    </div>

    <div class="card">
      <h2>Assets</h2>
      <p>Seeded units with current limits and capacity.</p>
      <div id="assets-table">Loading...</div>
    </div>
  </main>

<script>
let assetsCache = [];

async function loadAssets() {
  try {
    const res = await fetch('/assets');
    const data = await res.json();
    assetsCache = data;
    renderAssets(data);
    populateDispatchDropdown(data);
  } catch (err) {
    document.getElementById('assets-table').textContent = 'Error: ' + err;
  }
}

function renderAssets(data) {
  if (!Array.isArray(data) || data.length === 0) {
    document.getElementById('assets-table').textContent = 'No assets found.';
    return;
  }
  data.sort((a, b) => {
    const siteCmp = String(a.site_id || '').localeCompare(String(b.site_id || ''));
    if (siteCmp !== 0) return siteCmp;
    return String(a.name || '').localeCompare(String(b.name || ''));
  });
  let html = '<table><thead><tr><th>Name</th><th>Site</th><th>Cap (MWh)</th><th>Max MW</th><th>Min MW</th><th>Ramp MW/min</th></tr></thead><tbody>';
  for (const a of data) {
    html += `<tr>
      <td>${a.name}<div class="pill">${a.id}</div></td>
      <td>${a.site_name || a.site_id}</td>
      <td>${a.capacity_mwhr}</td>
      <td>${a.max_mw}</td>
      <td>${a.min_mw}</td>
      <td>${a.ramp_rate_mw_per_min}</td>
    </tr>`;
  }
  html += '</tbody></table>';
  document.getElementById('assets-table').innerHTML = html;
}

function populateDispatchDropdown(data) {
  const sel = document.getElementById('dispatch-asset-id');
  if (!Array.isArray(data) || data.length === 0) {
    sel.innerHTML = '<option value="">No assets</option>';
    return;
  }
  const options = data
    .map(a => `<option value="${a.id}">${a.name} (${a.site_name || a.site_id || 'unknown'})</option>`)
    .join('');
  sel.innerHTML = options;
}

function renderTelemetry(data) {
  if (!Array.isArray(data) || data.length === 0) {
    document.getElementById('telemetry-table').textContent = 'No telemetry yet.';
    return;
  }
  data.sort((a, b) => {
    const siteCmp = String(a.site_id || '').localeCompare(String(b.site_id || ''));
    if (siteCmp !== 0) return siteCmp;
    return String(a.name || '').localeCompare(String(b.name || ''));
  });
  let html = `<table><thead><tr>
    <th>Name</th>
    <th>Site</th>
    <th>Status</th>
    <th>Cap (MWh)</th>
    <th>SOC (MWh)</th>
    <th>MW</th>
    <th>Setpoint MW</th>
    <th>Max/Min</th>
    <th>Ramp MW/min</th>
    <th>Timestamp</th>
    <th>History</th>
  </tr></thead><tbody>`;
  for (const t of data) {
    if (t.error) {
      html += `<tr><td>${t.name || t.asset_id}</td><td>${t.site_name || t.site_id || ''}</td><td colspan="7" style="color:red;">Error: ${t.error}</td><td></td></tr>`;
      continue;
    }
    html += `<tr>
      <td>${t.name || t.asset_id}</td>
      <td>${t.site_name || t.site_id || ''}</td>
      <td><span class="pill">${t.status}</span></td>
      <td>${t.capacity_mwhr?.toFixed(0)}</td>
      <td>${t.soc_mwhr?.toFixed(2)}</td>
      <td>${t.current_mw?.toFixed(2)}</td>
      <td>${t.setpoint_mw?.toFixed(2)}</td>
      <td>${t.max_mw}/${t.min_mw}</td>
      <td>${t.ramp_rate_mw_per_min ?? ''}</td>
      <td>${formatTs(t.timestamp)}</td>
      <td><button onclick="openHistoryId('${t.asset_id}')">History</button></td>
    </tr>`;
  }
  html += '</tbody></table>';
  document.getElementById('telemetry-table').innerHTML = html;
}

async function loadTelemetryAll() {
  if (!Array.isArray(assetsCache) || assetsCache.length === 0) {
    document.getElementById('telemetry-table').textContent = 'No assets loaded.';
    return;
  }
  const results = [];
  for (const a of assetsCache) {
    const id = a.id;
    try {
      const res = await fetch(`/telemetry/${id}`);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const data = await res.json();
      results.push({
        ...data,
        name: a.name,
        location: a.location,
        ramp_rate_mw_per_min: a.ramp_rate_mw_per_min,
      });
    } catch (err) {
      results.push({
        asset_id: id,
        name: a.name,
        location: a.location,
        ramp_rate_mw_per_min: a.ramp_rate_mw_per_min,
        error: String(err),
      });
    }
  }
  renderTelemetry(results);
}

async function loadTelemetrySingle() {
  const id = prompt('Enter asset ID'); 
  if (!id) { return; }
  try {
    const res = await fetch(`/telemetry/${id}`);
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    const data = await res.json();
    document.getElementById('telemetry').textContent = JSON.stringify(data, null, 2);
  } catch (err) {
    document.getElementById('telemetry-table').textContent = 'Error: ' + err;
  }
}

async function createDispatch() {
  const id = document.getElementById('dispatch-asset-id').value.trim();
  const mw = parseFloat(document.getElementById('dispatch-mw').value);
  if (!id || isNaN(mw)) { alert('Fill all fields'); return; }
  try {
    const res = await fetch('/dispatch', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ asset_id: id, mw })
    });
    const text = await res.text();
    document.getElementById('dispatch-result').textContent = text;
  } catch (err) {
    document.getElementById('dispatch-result').textContent = 'Error: ' + err;
  }
}

function openHistoryId(id) {
  window.open(`/history.html?asset_id=${encodeURIComponent(id)}`, '_blank');
}

function formatTs(ts) {
  if (!ts) return '';
  const d = new Date(ts);
  return d.toLocaleString(undefined, {
    year: 'numeric',
    month: '2-digit',
    day: '2-digit',
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
    timeZoneName: 'short',
    hour12: false,
  });
}

loadAssets();
setInterval(loadTelemetryAll, 4000);
</script>
</body>
</html>
"#,
    )
}

async fn init_db() -> Result<Option<PgPool>> {
    let url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            tracing::warn!("DATABASE_URL not set; running without Postgres persistence");
            return Ok(None);
        }
    };

    // Connect a small pool; Timescale is optional but enabled if available.
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .context("connect postgres")?;

    // Dev helper: truncate tables if RESET_DB=1. Avoid in normal use.
    if std::env::var("RESET_DB").ok().as_deref() == Some("1") {
        tracing::warn!("RESET_DB=1 -> truncating assets/telemetry/dispatches");
        sqlx::query("TRUNCATE TABLE telemetry, dispatches, assets CASCADE;")
            .execute(&pool)
            .await
            .context("truncate tables")?;
    }

    // Create extension (best-effort) and tables.
    if let Err(err) = sqlx::query("CREATE EXTENSION IF NOT EXISTS timescaledb;")
        .execute(&pool)
        .await
    {
        tracing::warn!(?err, "could not enable timescaledb extension; continuing");
    }

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS sites (
            id uuid PRIMARY KEY,
            name text NOT NULL,
            location text NOT NULL
        );
        "#,
    )
    .execute(&pool)
    .await
    .context("create sites table")?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS assets (
            id uuid PRIMARY KEY,
            site_id uuid REFERENCES sites(id),
            name text NOT NULL,
            location text NOT NULL,
            capacity_mwhr double precision NOT NULL,
            max_mw double precision NOT NULL,
            min_mw double precision NOT NULL,
            efficiency double precision NOT NULL,
            ramp_rate_mw_per_min double precision NOT NULL
        );
        "#,
    )
    .execute(&pool)
    .await
    .context("create assets table")?;

    // Backward compatibility for existing assets table without site_id.
    sqlx::query("ALTER TABLE assets ADD COLUMN IF NOT EXISTS site_id uuid;")
        .execute(&pool)
        .await
        .context("alter assets table add site_id")?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS telemetry (
            asset_id uuid REFERENCES assets(id),
            site_id uuid REFERENCES sites(id),
            ts timestamptz NOT NULL,
            soc_mwhr double precision NOT NULL,
            soc_pct double precision NOT NULL,
            current_mw double precision NOT NULL,
            setpoint_mw double precision NOT NULL,
            max_mw double precision NOT NULL,
            min_mw double precision NOT NULL,
            status text NOT NULL
        );
        "#,
    )
    .execute(&pool)
    .await
    .context("create telemetry table")?;

    // Backward compatibility: add setpoint_mw if the table already existed without it.
    sqlx::query("ALTER TABLE telemetry ADD COLUMN IF NOT EXISTS setpoint_mw double precision;")
        .execute(&pool)
        .await
        .context("alter telemetry table add setpoint_mw")?;
    sqlx::query("ALTER TABLE telemetry ADD COLUMN IF NOT EXISTS site_id uuid;")
        .execute(&pool)
        .await
        .context("alter telemetry table add site_id")?;
    sqlx::query("ALTER TABLE telemetry ADD COLUMN IF NOT EXISTS soc_pct double precision;")
        .execute(&pool)
        .await
        .context("alter telemetry table add soc_pct")?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS dispatches (
            id uuid PRIMARY KEY,
            asset_id uuid REFERENCES assets(id),
            mw double precision NOT NULL,
            duration_s bigint,
            status text NOT NULL,
            reason text,
            submitted_at timestamptz NOT NULL
        );
        "#,
    )
    .execute(&pool)
    .await
    .context("create dispatches table")?;

    // Backward compatibility: drop mode column if present.
    let _ = sqlx::query("ALTER TABLE dispatches DROP COLUMN IF EXISTS mode;")
        .execute(&pool)
        .await;

    Ok(Some(pool))
}

async fn persist_assets(pool: &PgPool, assets: &HashMap<Uuid, Asset>) -> Result<()> {
    for asset in assets.values() {
        sqlx::query(
            r#"INSERT INTO sites (id, name, location)
               VALUES ($1,$2,$3)
               ON CONFLICT (id) DO UPDATE SET
                 name = EXCLUDED.name,
                 location = EXCLUDED.location
            "#,
        )
        .bind(asset.site_id)
        .bind(&asset.location) // reuse location as site name if not present
        .bind(&asset.location)
        .execute(pool)
        .await?;

        sqlx::query(
            r#"INSERT INTO assets (id, site_id, name, location, capacity_mwhr, max_mw, min_mw, efficiency, ramp_rate_mw_per_min)
               VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)
               ON CONFLICT (id) DO UPDATE SET
                 name = EXCLUDED.name,
                 site_id = EXCLUDED.site_id,
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
        .bind(&asset.location)
        .bind(asset.capacity_mwhr)
        .bind(asset.max_mw)
        .bind(asset.min_mw)
        .bind(asset.efficiency)
        .bind(asset.ramp_rate_mw_per_min)
        .execute(pool)
        .await?;
    }
    Ok(())
}

async fn persist_telemetry(pool: &PgPool, telemetry: &[Telemetry]) -> Result<()> {
    for t in telemetry {
        sqlx::query(
            r#"INSERT INTO telemetry (asset_id, site_id, ts, soc_mwhr, current_mw, setpoint_mw, max_mw, min_mw, status)
               VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)
            "#,
        )
        .bind(t.asset_id)
        .bind(t.site_id)
        .bind(t.timestamp)
        .bind(t.soc_mwhr)
        .bind(t.current_mw)
        .bind(t.setpoint_mw)
        .bind(t.max_mw)
        .bind(t.min_mw)
        .bind(&t.status)
        .execute(pool)
        .await?;
    }
    Ok(())
}

async fn persist_dispatch(pool: &PgPool, dispatch: &Dispatch) -> Result<()> {
    sqlx::query(
        r#"INSERT INTO dispatches (id, asset_id, mw, duration_s, status, reason, submitted_at)
           VALUES ($1,$2,$3,$4,$5,$6,$7)
           ON CONFLICT (id) DO UPDATE SET
             status = EXCLUDED.status,
             reason = EXCLUDED.reason
        "#,
    )
    .bind(dispatch.id)
    .bind(dispatch.asset_id)
    .bind(dispatch.mw)
    .bind(dispatch.duration_s.map(|d| d as i64))
    .bind(&dispatch.status)
    .bind(&dispatch.reason)
    .bind(dispatch.submitted_at)
    .execute(pool)
    .await?;
    Ok(())
}

async fn load_assets_from_db(pool: &PgPool) -> Result<Vec<Asset>> {
    let rows = sqlx::query_as::<_, Asset>(
        r#"
        SELECT id, site_id, name, location, capacity_mwhr, max_mw, min_mw, efficiency, ramp_rate_mw_per_min
        FROM assets
        "#,
    )
    .fetch_all(pool)
    .await
    .context("load assets")?;
    Ok(rows)
}

#[derive(sqlx::FromRow)]
struct LastDispatchRow {
    mw: f64,
}

async fn reconcile_db(pool: &PgPool, cfg: &AssetConfig) -> Result<()> {
    // Upsert sites from config.
    for site in &cfg.sites {
        sqlx::query(
            r#"INSERT INTO sites (id, name, location)
               VALUES ($1,$2,$3)
               ON CONFLICT (id) DO UPDATE SET
                 name = EXCLUDED.name,
                 location = EXCLUDED.location
            "#,
        )
        .bind(site.id)
        .bind(&site.name)
        .bind(&site.location)
        .execute(pool)
        .await?;
    }

    // Upsert assets from config.
    let assets = cfg.clone().to_assets();
    let asset_ids: Vec<Uuid> = assets.iter().map(|a| a.id).collect();
    let asset_map: HashMap<Uuid, Asset> = assets.into_iter().map(|a| (a.id, a)).collect();
    persist_assets(pool, &asset_map).await?;

    // Drop telemetry/dispatches for unknown assets (keep DB clean).
    if !asset_ids.is_empty() {
        sqlx::query("DELETE FROM telemetry WHERE asset_id <> ALL($1)")
            .bind(&asset_ids)
            .execute(pool)
            .await
            .context("prune telemetry for unknown assets")?;
        sqlx::query("DELETE FROM dispatches WHERE asset_id <> ALL($1)")
            .bind(&asset_ids)
            .execute(pool)
            .await
            .context("prune dispatches for unknown assets")?;
    }

    Ok(())
}

// Pull the latest telemetry row per asset to restore SOC/current/setpoint after restart.
async fn apply_last_state_from_db(pool: &PgPool, sim: Arc<RwLock<Simulator>>) -> Result<()> {
    let rows: Vec<LastStateRow> = sqlx::query_as::<_, LastStateRow>(
        r#"
        SELECT DISTINCT ON (asset_id)
            asset_id, soc_mwhr, current_mw, setpoint_mw
        FROM telemetry
        ORDER BY asset_id, ts DESC
        "#,
    )
    .fetch_all(pool)
    .await
    .context("fetch last telemetry per asset")?;

    let mut guard = sim.write().await;
    for row in rows {
        if let Some(asset) = guard.assets.get(&row.asset_id) {
            let (min_mw, max_mw, cap) = (asset.min_mw, asset.max_mw, asset.capacity_mwhr);
            let state = guard.state.entry(row.asset_id).or_insert(BessState {
                soc_mwhr: cap * 0.5,
                current_mw: 0.0,
                setpoint_mw: 0.0,
            });
            state.soc_mwhr = row.soc_mwhr.clamp(0.0, cap);
            state.current_mw = row.current_mw.clamp(min_mw, max_mw);
            state.setpoint_mw = row.setpoint_mw.clamp(min_mw, max_mw);
        }
    }

    Ok(())
}

async fn apply_saved_setpoints(pool: &PgPool, sim: Arc<RwLock<Simulator>>) -> Result<()> {
    let asset_ids: Vec<Uuid> = sim.read().await.assets.keys().copied().collect();
    for id in asset_ids {
        if let Ok(Some(row)) = sqlx::query_as::<_, LastDispatchRow>(
            "SELECT mw FROM dispatches WHERE asset_id = $1 ORDER BY submitted_at DESC LIMIT 1",
        )
        .bind(id)
        .fetch_optional(pool)
        .await
        {
            let desired = row.mw;
            let mut guard = sim.write().await;
            if let Some((min, max)) = guard.assets.get(&id).map(|a| (a.min_mw, a.max_mw)) {
                if let Some(state) = guard.state.get_mut(&id) {
                    // Clamp to asset limits just in case.
                    state.setpoint_mw = desired.clamp(min, max);
                }
            }
        }
    }
    Ok(())
}

fn init_tracing() {
    let subscriber = tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()),
        ))
        .with(tracing_subscriber::fmt::layer());
    subscriber.init();
}

async fn ui_history() -> Html<&'static str> {
    Html(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <title>BESS Telemetry History</title>
  <style>
    body { font-family: "Inter", system-ui, -apple-system, sans-serif; margin: 1.5rem; background: #f6f8fb; color: #111827; }
    .card { background: #fff; border: 1px solid #e3e8f0; padding: 1rem; border-radius: 12px; box-shadow: 0 4px 12px rgba(17,24,39,0.04); max-width: 960px; margin: 0 auto; }
    table { width: 100%; border-collapse: collapse; font-size: 0.95rem; }
    th, td { padding: 0.35rem 0.5rem; text-align: left; border-bottom: 1px solid #e3e8f0; }
    th { font-weight: 700; color: #6b7280; }
    .pill { display: inline-block; padding: 0.2rem 0.6rem; border-radius: 999px; background: #dbeafe; color: #2563eb; font-weight: 600; font-size: 0.8rem; }
  </style>
</head>
<body>
  <div class="card">
    <h2>Telemetry History</h2>
    <p id="asset-label"></p>
    <div id="history-table" style="max-height: 70vh; overflow: auto;">Loading...</div>
  </div>
<script>
const params = new URLSearchParams(window.location.search);
const assetId = params.get('asset_id');
document.getElementById('asset-label').textContent = assetId ? `Asset: ${assetId}` : 'No asset selected';

let histSortField = 'ts';
let histSortDir = 'desc';

async function loadHistory() {
  if (!assetId) { document.getElementById('history-table').textContent = 'No asset_id provided.'; return; }
  try {
    const res = await fetch(`/telemetry/${assetId}/history`);
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    const data = await res.json();
    renderHistory(data);
  } catch (err) {
    document.getElementById('history-table').textContent = 'Error: ' + err;
  }
}

function setHistSort(field) {
  if (histSortField === field) {
    histSortDir = histSortDir === 'asc' ? 'desc' : 'asc';
  } else {
    histSortField = field;
    histSortDir = 'asc';
  }
  renderHistory(lastHistoryData || []);
}

let lastHistoryData = [];

function renderHistory(data) {
  lastHistoryData = data;
  if (!Array.isArray(data) || data.length === 0) {
    document.getElementById('history-table').textContent = 'No history found.';
    return;
  }
  const header = data[0];
  document.getElementById('asset-label').textContent = header
    ? `Asset: ${header.name || header.asset_id} (${header.location || ''})`
    : `Asset: ${assetId}`;

  const sorted = [...data].sort((a, b) => {
    const dir = histSortDir === 'asc' ? 1 : -1;
    const va = a[histSortField];
    const vb = b[histSortField];
    if (va == null && vb == null) return 0;
    if (va == null) return -1 * dir;
    if (vb == null) return 1 * dir;
    if (typeof va === 'number' && typeof vb === 'number') return (va - vb) * dir;
    return String(va).localeCompare(String(vb)) * dir;
  });

  const si = (f) => histSortField === f ? (histSortDir === 'asc' ? ' ▲' : ' ▼') : '';

  let html = `<table><thead><tr>
    <th onclick="setHistSort('ts')">Timestamp${si('ts')}</th>
    <th onclick="setHistSort('asset_id')">Asset${si('asset_id')}</th>
    <th onclick="setHistSort('site_id')">Site${si('site_id')}</th>
    <th onclick="setHistSort('status')">Status${si('status')}</th>
    <th onclick="setHistSort('soc_mwhr')">SOC (MWh)${si('soc_mwhr')}</th>
    <th onclick="setHistSort('current_mw')">MW${si('current_mw')}</th>
    <th onclick="setHistSort('setpoint_mw')">Setpoint MW${si('setpoint_mw')}</th>
    <th onclick="setHistSort('max_mw')">Max/Min${si('max_mw')}</th>
    <th onclick="setHistSort('ramp_rate_mw_per_min')">Ramp MW/min${si('ramp_rate_mw_per_min')}</th>
  </tr></thead><tbody>`;
  for (const t of sorted) {
    html += `<tr>
      <td>${formatTs(t.ts)}</td>
      <td>${t.name || t.asset_id}</td>
      <td>${t.site_name || t.site_id || ''}</td>
      <td><span class="pill">${t.status}</span></td>
      <td>${t.soc_mwhr?.toFixed(2)}</td>
      <td>${t.current_mw?.toFixed(2)}</td>
      <td>${t.setpoint_mw?.toFixed(2)}</td>
      <td>${t.max_mw}/${t.min_mw}</td>
      <td>${t.ramp_rate_mw_per_min ?? ''}</td>
    </tr>`;
  }
  html += '</tbody></table>';
  document.getElementById('history-table').innerHTML = html;
}

loadHistory();
setInterval(loadHistory, 1000);

function formatTs(ts) {
  if (!ts) return '';
  const d = new Date(ts);
  return d.toLocaleString(undefined, {
    year: 'numeric',
    month: '2-digit',
    day: '2-digit',
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
    timeZoneName: 'short',
    hour12: false,
  });
}
</script>
</body>
</html>
"#,
    )
}
#[derive(Deserialize, Clone)]
struct AssetConfig {
    sites: Vec<SiteConfig>,
    assets: Vec<AssetConfigEntry>,
}

#[derive(Deserialize, Clone)]
struct SiteConfig {
    id: Uuid,
    name: String,
    location: String,
}

#[derive(Deserialize, Clone)]
struct AssetConfigEntry {
    id: Uuid,
    name: String,
    site_id: Uuid,
    capacity_mwhr: f64,
    max_mw: f64,
    min_mw: f64,
    efficiency: f64,
    ramp_rate_mw_per_min: f64,
}

impl AssetConfig {
    fn load_from_file(path: &str) -> Result<Self> {
        let content = std::fs::read_to_string(path).context("read asset config")?;
        let cfg: AssetConfig = serde_yaml::from_str(&content).context("parse asset config yaml")?;
        Ok(cfg)
    }

    fn to_assets(self) -> Vec<Asset> {
        self.assets
            .into_iter()
            .map(|a| {
                let site = self.sites.iter().find(|s| s.id == a.site_id);
                let (site_name, location) = match site {
                    Some(site) => (site.name.clone(), site.location.clone()),
                    None => ("unknown".to_string(), "unknown".to_string()),
                };
                Asset {
                    id: a.id,
                    name: a.name,
                    location,
                    capacity_mwhr: a.capacity_mwhr,
                    max_mw: a.max_mw,
                    min_mw: a.min_mw,
                    efficiency: a.efficiency,
                    ramp_rate_mw_per_min: a.ramp_rate_mw_per_min,
                    site_id: a.site_id,
                    site_name,
                }
            })
            .collect()
    }
}

fn load_asset_config() -> AssetConfig {
    AssetConfig::load_from_file("assets.yaml").unwrap_or_else(|err| {
        tracing::warn!(?err, "failed to load assets.yaml, falling back to defaults");
        AssetConfig {
            sites: (1..=5)
                .map(|i| SiteConfig {
                    id: Uuid::new_v5(&Uuid::NAMESPACE_OID, format!("Site-{i}").as_bytes()),
                    name: format!("Site-{i}"),
                    location: format!("Location-{i}"),
                })
                .collect(),
            assets: (1..=5)
                .map(|i| AssetConfigEntry {
                    id: Uuid::new_v5(&Uuid::NAMESPACE_OID, format!("BESS-{i}").as_bytes()),
                    name: format!("BESS-{i}"),
                    site_id: Uuid::new_v5(&Uuid::NAMESPACE_OID, format!("Site-{i}").as_bytes()),
                    capacity_mwhr: 100.0,
                    max_mw: 50.0,
                    min_mw: -50.0,
                    efficiency: 0.92,
                    ramp_rate_mw_per_min: 20.0,
                })
                .collect(),
        }
    })
}
