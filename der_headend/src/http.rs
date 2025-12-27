use std::collections::HashMap;

use anyhow::{Context, Result};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::QueryBuilder;
use uuid::Uuid;

use sim_core::{Asset, BessEvent, Dispatch, DispatchRequest, Telemetry, tick_asset};

use crate::{
    AppState,
    proto::{HeadendToAgent, Setpoint, headend_to_agent},
};
use crate::db::{persist_dispatch, persist_event, persist_telemetry};
use crate::models::{
    AssetView, HeartbeatRow, IncomingDispatch, SiteAccumulator, SiteAggregateBuilder, SiteView,
    TelemetryRow, TimeRange,
};

pub(crate) async fn ui_home() -> Html<&'static str> {
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
  <p class="sub">Basic controls for assets, telemetry, and dispatch. Use the API for automation.</p>

  <div class="links">
    <a href="/assets">/assets</a>
    <a href="/sites">/sites</a>
    <a href="/agents">/agents</a>
    <a href="/dispatch/history">/dispatch/history</a>
  </div>

  <div class="grid">
    <div class="card">
      <h2>Assets</h2>
      <table>
        <thead>
          <tr>
            <th>Asset</th>
            <th>Site</th>
            <th>SOC</th>
            <th>MW</th>
            <th>Status</th>
          </tr>
        </thead>
        <tbody id="asset_table"></tbody>
      </table>
    </div>

    <div class="card">
      <h2>Telemetry</h2>
      <div class="row small muted">
        <span id="telemetry_status">Select an asset</span>
      </div>
      <pre id="telemetry_json">{}</pre>
      <div class="row small muted" style="margin-top:6px;">
        <span>Extras</span>
      </div>
      <pre id="telemetry_extras">{}</pre>
    </div>

    <div class="card">
      <h2>Dispatch</h2>
      <div class="row">
        <label>Mode</label>
        <select id="dispatch_mode">
          <option value="asset">Per Asset</option>
          <option value="site">Per Site</option>
        </select>
        <label>Target</label>
        <select id="dispatch_target"></select>
        <label>MW</label>
        <input id="dispatch_mw" type="number" step="0.1" value="0" />
        <label>Duration (s)</label>
        <input id="dispatch_duration" type="number" step="1" value="0" />
        <button id="dispatch_send">Send</button>
        <button id="dispatch_copy">Copy curl</button>
      </div>
      <pre id="dispatch_resp">{}</pre>
    </div>

    <div class="card">
      <h2>Agents</h2>
      <pre id="agents_json">[]</pre>
    </div>

    <div class="card">
      <h2>Dispatch History</h2>
      <pre id="dispatch_history">[]</pre>
    </div>
  </div>

  <script>
  const ASSETS = [];
  const SITES = [];

  const $ = (id) => document.getElementById(id);
  const pretty = (obj) => JSON.stringify(obj, null, 2);

  function setStatus(ok, msg) {
    $('telemetry_status').textContent = msg;
    $('telemetry_status').className = ok ? 'pill ok' : 'pill bad';
  }

  async function fetchJson(path, opts) {
    const res = await fetch(path, opts);
    if (!res.ok) {
      const text = await res.text();
      throw new Error(text || res.statusText);
    }
    return res.json();
  }

  async function loadAssets() {
    const data = await fetchJson('/assets');
    ASSETS.length = 0;
    data.forEach(a => ASSETS.push(a));
    renderAssets();
  }

  async function loadSites() {
    const data = await fetchJson('/sites');
    SITES.length = 0;
    data.forEach(s => SITES.push(s));
  }

  async function loadAgents() {
    const data = await fetchJson('/agents');
    $('agents_json').textContent = pretty(data);
  }

  async function loadDispatchHistory() {
    const data = await fetchJson('/dispatch/history');
    $('dispatch_history').textContent = pretty(data);
  }

  function renderAssets() {
    const tbody = $('asset_table');
    tbody.innerHTML = '';
    ASSETS.forEach(asset => {
      const tr = document.createElement('tr');
      tr.addEventListener('click', () => selectAsset(asset.id, asset.name));
      const soc = asset.soc_pct == null ? 'n/a' : asset.soc_pct.toFixed(1) + '%';
      const mw = asset.current_mw == null ? 'n/a' : asset.current_mw.toFixed(2);
      const status = asset.status || 'unknown';
      tr.innerHTML = `
        <td>${asset.name}</td>
        <td>${asset.site_name}</td>
        <td>${soc}</td>
        <td>${mw}</td>
        <td>${status}</td>
      `;
      tbody.appendChild(tr);
    });
    updateDispatchOptions();
  }

  function updateDispatchOptions() {
    const mode = $('dispatch_mode').value;
    const target = $('dispatch_target');
    target.innerHTML = '';
    if (mode === 'site') {
      const seen = new Map();
      ASSETS.forEach(asset => {
        if (!seen.has(asset.site_id)) {
          seen.set(asset.site_id, asset.site_name);
        }
      });
      for (const [id, name] of seen.entries()) {
        const opt = document.createElement('option');
        opt.value = id;
        opt.textContent = name;
        target.appendChild(opt);
      }
    } else {
      ASSETS.forEach(asset => {
        const opt = document.createElement('option');
        opt.value = asset.id;
        opt.textContent = asset.name;
        target.appendChild(opt);
      });
    }
  }

  async function refreshAssetTelemetry() {
    const id = $('dispatch_target').value;
    if (!id) return;
    try {
      const data = await fetchJson(`/telemetry/${id}`);
      $('telemetry_json').textContent = pretty(data);
      $('telemetry_extras').textContent = pretty(data.extras || {});
      setStatus(true, 'Telemetry OK');
    } catch (err) {
      $('telemetry_json').textContent = pretty({ error: String(err) });
      $('telemetry_extras').textContent = pretty({});
      setStatus(false, 'Telemetry error');
    }
  }

  function selectAsset(id, name) {
    $('dispatch_mode').value = 'asset';
    updateDispatchOptions();
    $('dispatch_target').value = id;
    $('telemetry_status').textContent = `Selected ${name}`;
    refreshAssetTelemetry();
  }

  function buildDispatchPayload() {
    const mode = $('dispatch_mode').value;
    const target = $('dispatch_target').value;
    const mw = parseFloat($('dispatch_mw').value);
    const duration_s = parseInt($('dispatch_duration').value, 10);
    const payload = { mw, duration_s: duration_s > 0 ? duration_s : null };
    if (mode === 'site') payload.site_id = target;
    else payload.asset_id = target;
    return payload;
  }

  async function sendDispatch() {
    const payload = buildDispatchPayload();
    $('dispatch_resp').textContent = pretty({ sending: payload });
    try {
      const data = await fetchJson('/dispatch', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(payload),
      });
      $('dispatch_resp').textContent = pretty(data);
    } catch (err) {
      $('dispatch_resp').textContent = pretty({ error: String(err) });
    }
  }

  function copyDispatchCurl() {
    const payload = buildDispatchPayload();
    const cmd = `curl -s -X POST http://localhost:5005/dispatch \\
  -H 'Content-Type: application/json' \\
  -d '${JSON.stringify(payload)}'`;
    navigator.clipboard.writeText(cmd).then(() => {
      $('dispatch_resp').textContent = pretty({ copied: true, cmd });
    });
  }

  function wireUi() {
    $('dispatch_mode').addEventListener('change', () => {
      updateDispatchOptions();
    });
    $('dispatch_send').addEventListener('click', () => sendDispatch());
    $('dispatch_copy').addEventListener('click', () => copyDispatchCurl());
  }

  async function boot() {
    wireUi();
    try {
      await loadAssets();
      await loadSites();
      await refreshAssetTelemetry();
      await loadAgents();
      await loadDispatchHistory();
      if (ASSETS.length) selectAsset(ASSETS[0].id, ASSETS[0].name);
      setInterval(() => loadAssets().catch(() => {}), 4000);
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

pub(crate) async fn health(State(state): State<AppState>) -> Response {
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

pub(crate) async fn list_assets(State(state): State<AppState>) -> Json<Vec<AssetView>> {
    Json(build_asset_views(&state).await)
}

pub(crate) async fn get_asset(State(state): State<AppState>, Path(id): Path<Uuid>) -> Response {
    let assets = build_asset_views(&state).await;
    match assets.into_iter().find(|a| a.id == id) {
        Some(asset) => Json(asset).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

pub(crate) async fn list_sites(State(state): State<AppState>) -> Json<Vec<SiteView>> {
    let assets = build_asset_views(&state).await;
    let mut sites: HashMap<Uuid, SiteAccumulator> = HashMap::new();
    for asset in assets {
        let entry = sites.entry(asset.site_id).or_insert_with(|| SiteAccumulator {
            site_id: asset.site_id,
            site_name: asset.site_name.clone(),
            location: asset.location.clone(),
            assets: Vec::new(),
            aggregate: SiteAggregateBuilder::default(),
        });
        entry.aggregate.add_asset(&asset);
        entry.assets.push(asset);
    }
    let mut rows: Vec<SiteView> = sites
        .into_values()
        .map(|site| SiteView {
            site_id: site.site_id,
            site_name: site.site_name,
            location: site.location,
            assets: site.assets,
            aggregate: site.aggregate.finish(),
        })
        .collect();
    rows.sort_by(|a, b| a.site_name.to_lowercase().cmp(&b.site_name.to_lowercase()));
    Json(rows)
}

async fn build_asset_views(state: &AppState) -> Vec<AssetView> {
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
            available_charge_kw: telemetry.map(|t| t.available_charge_kw),
            available_discharge_kw: telemetry.map(|t| t.available_discharge_kw),
            capacity_mwhr: asset.capacity_mwhr,
            current_a: telemetry.map(|t| t.current_a),
            current_mw: telemetry.map(|t| t.current_mw),
            cycle_count: telemetry.map(|t| t.cycle_count),
            dc_bus_a: telemetry.map(|t| t.dc_bus_a),
            dc_bus_v: telemetry.map(|t| t.dc_bus_v),
            efficiency: asset.efficiency,
            energy_in_mwh: telemetry.map(|t| t.energy_in_mwh),
            energy_out_mwh: telemetry.map(|t| t.energy_out_mwh),
            id: asset.id,
            location: asset.location.clone(),
            max_mw: asset.max_mw,
            max_soc_pct: asset.max_soc_pct,
            min_mw: asset.min_mw,
            min_soc_pct: asset.min_soc_pct,
            name: asset.name.clone(),
            ramp_rate_mw_per_min: asset.ramp_rate_mw_per_min,
            setpoint_mw: telemetry.map(|t| t.setpoint_mw),
            soh_pct: telemetry.map(|t| t.soh_pct),
            soc_mwhr: telemetry.map(|t| t.soc_mwhr),
            soc_pct: telemetry.map(|t| t.soc_pct),
            site_id: asset.site_id,
            site_name: asset.site_name.clone(),
            status: telemetry.map(|t| t.status.clone()),
            temperature_ambient_f: telemetry.map(|t| t.temperature_ambient_f),
            temperature_cell_f: telemetry.map(|t| t.temperature_cell_f),
            temperature_module_f: telemetry.map(|t| t.temperature_module_f),
            voltage_v: telemetry.map(|t| t.voltage_v),
        });
    }
    rows
}

pub(crate) async fn latest_telemetry(
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

pub(crate) async fn history_telemetry(
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

    let mut builder = QueryBuilder::new(
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
            t.voltage_v,
            t.current_a,
            t.dc_bus_v,
            t.dc_bus_a,
            t.temperature_cell_f,
            t.temperature_module_f,
            t.temperature_ambient_f,
            t.soh_pct,
            t.cycle_count,
            t.energy_in_mwh,
            t.energy_out_mwh,
            t.available_charge_kw,
            t.available_discharge_kw,
            t.extras,
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

pub(crate) async fn ingest_telemetry(
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

pub(crate) async fn create_dispatch(
    State(state): State<AppState>,
    Json(req): Json<IncomingDispatch>,
) -> Response {
    // Validate mutual exclusivity.
    if req.asset_id.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()).is_some()
        == req.site_id.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()).is_some()
    {
        return (
            StatusCode::BAD_REQUEST,
            "provide exactly one of asset_id or site_id",
        )
            .into_response();
    }

    // Site dispatch fan-out.
    if let Some(site_id) = req.site_id.as_ref().filter(|s| !s.trim().is_empty()) {
        let site_id = match Uuid::parse_str(site_id) {
            Ok(id) => id,
            Err(_) => {
                return (StatusCode::BAD_REQUEST, "invalid site_id").into_response();
            }
        };
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
            return (
                StatusCode::BAD_REQUEST,
                format!("no online assets for site_id={}", site_id),
            )
                .into_response();
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
            return (
                StatusCode::BAD_REQUEST,
                format!("metadata missing for site_id={}", site_id),
            )
                .into_response();
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
                asset_id: Some(alloc.asset_id.to_string()),
                site_id: Some(site_id.to_string()),
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
            (StatusCode::BAD_REQUEST, err.to_string()).into_response()
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
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .context("asset_id required for single dispatch")?;
    let asset_id = Uuid::parse_str(asset_id).context("invalid asset_id")?;
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

    let soc_mwhr_override = {
        let latest = state.latest.read().await;
        latest.get(&asset_id).map(|t| t.soc_mwhr)
    };

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

    let result = sim.set_dispatch(dispatch_req, soc_mwhr_override);
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

pub(crate) async fn list_agents(State(state): State<AppState>) -> Response {
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
pub(crate) struct EventRequest {
    asset_id: Uuid,
    event_type: String,
    severity: Option<String>,
    message: Option<String>,
}

/// Return recent dispatch records (DB-backed). Empty array if no DB configured.
pub(crate) async fn list_dispatch_history(State(state): State<AppState>) -> Response {
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

pub(crate) async fn create_event(
    State(state): State<AppState>,
    Json(req): Json<EventRequest>,
) -> Response {
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

pub(crate) async fn list_event_history(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Response {
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
pub(crate) async fn latest_heartbeat(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Response {
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
pub(crate) async fn history_heartbeats(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Response {
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
