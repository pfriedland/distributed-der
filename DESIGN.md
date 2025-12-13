# BESS Simulator Design (current implementation)

## What’s implemented now (MVP)
- **Assets from YAML**: `assets.yaml` defines sites and BESS units with stable UUIDs and operating params (`site_id`, `capacity_mwhr`, `max_mw`, `min_mw`, `ramp_rate_mw_per_min`, `efficiency`). If the file is missing/invalid, a deterministic default set is used.
  - Assets are loaded into memory on startup. If `DATABASE_URL` is set, the DB is reconciled to the YAML (upsert sites/assets, prune orphan telemetry/dispatch rows).
  - Initial SOC = 50% of capacity; setpoint = 0 MW unless a saved dispatch re-applies the setpoint.
- **Tick loop**: runs every 4 seconds; for each asset:
  - Ramps current MW toward the setpoint, limited by ramp_rate/60 per second, and clamps to min/max MW.
  - Updates SOC linearly: `energy_delta_mwh = current_mw * dt / 3600`; applies efficiency (discharge divides by eff, charge multiplies by eff); clamps SOC to [0, capacity].
  - Status: `discharging` if MW > 0.1, `charging` if MW < -0.1, else `idle`.
  - Emits telemetry snapshot (timestamp, SOC, current MW, limits, status).
- **Dispatch**: `POST /dispatch` with `{asset_id, mw, duration_s?}`:
  - Validates asset exists and MW is within min/max.
  - Sets the simulator setpoint (positive discharge, negative charge).
  - Records a dispatch with status `accepted` (no duration handling yet).
- **REST endpoints**:
  - `GET /assets` – list all assets.
  - `GET /assets/{id}` – fetch metadata.
  - `GET /telemetry/{id}` – latest (live) telemetry from in-memory state.
  - `GET /telemetry/{id}/history` – last 100 telemetry points from DB (if configured).
  - `POST /dispatch` – create a dispatch (see above).
- **Persistence (optional)**:
  - If `DATABASE_URL` is set (e.g., `postgres://bess:besspassword@localhost:5432/bess`), the service will:
    - Create Timescale extension (best effort) and tables (`sites`, `assets`, `telemetry`, `dispatches`); add missing columns for backward compat.
    - Upsert sites/assets from YAML on startup; prune telemetry/dispatch rows for unknown assets.
    - Insert telemetry snapshots on each tick.
    - Insert dispatch records on creation and reapply latest setpoints on restart.
    - Optional `RESET_DB=1` env var truncates `assets/telemetry/dispatches` on startup (one-time cleanup).
  - If `DATABASE_URL` is not set, the simulator runs in-memory only.
- **Tracing**: basic `TraceLayer` with env-configurable filter (`RUST_LOG=info` or `debug`).
- **Ports**: HTTP server on `127.0.0.1:3001`.

## Key data structures
- `Asset`: id, site_id, name, location, capacity_mwhr, max_mw, min_mw, efficiency, ramp_rate_mw_per_min.
- `BessState`: soc_mwhr, current_mw, setpoint_mw (per asset).
- `Telemetry`: asset_id, site_id, timestamp, soc_mwhr, soc_pct, capacity_mwhr, current_mw, setpoint_mw, max_mw, min_mw, status.
- `Dispatch`: id, asset_id, mw (sign carries mode), duration_s, status, reason, submitted_at.
- `Simulator`: holds maps of assets, state, dispatches; methods to `tick(dt_secs)` and `set_dispatch`.

## Code structure (Rust)
- `src/main.rs`: single binary with everything inside (for simplicity).
  - **AppState**: shared state passed into Axum handlers; wraps `Arc<RwLock<Simulator>>` and optional `PgPool`.
  - **Simulator**:
    - `from_assets(assets)`: build in-memory maps (assets, state, dispatches).
    - `tick(dt_secs)`: advance SOC/current MW with ramp limits and efficiency; produce telemetry snapshots.
    - `set_dispatch(req)`: validate limits, set per-asset setpoint (positive=discharge, negative=charge), and record a dispatch.
  - **Models**:
    - Asset, BessState, Telemetry (live), TelemetryRow (DB history), Dispatch, DispatchRequest.
  - **Startup flow**:
    1) Load YAML (`assets.yaml`, with site_id and params) via `load_asset_config().to_assets()`.
    2) If DB is configured: `reconcile_db` (upsert sites/assets, prune unknown), `apply_last_state_from_db` (restore SOC/current/setpoint from latest telemetry per asset), `apply_saved_setpoints` (reapply last dispatch MW).
    3) Spawn a 4s tick task: `Simulator::tick` updates state; `persist_telemetry` writes snapshots if DB exists.
    4) Build Axum router with endpoints: `/` UI, `/assets`, `/assets/{id}`, `/telemetry/{id}`, `/telemetry/{id}/history`, `/dispatch`.
  - **DB helpers**:
    - `init_db`: create extensions/tables, add missing columns (site_id, setpoint_mw, soc_pct), drop legacy dispatch.mode.
    - `reconcile_db`: upsert sites/assets from YAML; prune telemetry/dispatch rows for unknown assets.
    - `apply_last_state_from_db`: pull last telemetry per asset and restore SOC/current/setpoint.
    - `apply_saved_setpoints`: pull last dispatch per asset and restore setpoint.
    - `persist_assets`, `persist_telemetry`, `persist_dispatch`: upserts/inserts as needed.
  - **UI** (HTML/JS served inline): dashboard showing assets, live telemetry (auto-refresh), per-row history buttons, dispatch form (sign indicates charge/discharge), and a history page with sortable table.

## How the simulation step works
1. Compute allowed ramp per second (`ramp_rate_mw_per_min / 60`).
2. Move current MW toward setpoint, clamped by ramp and min/max.
3. Compute energy delta over dt (seconds), adjust for efficiency:
   - Discharge (MW > 0): SOC decreases by `energy_delta / eff`.
   - Charge (MW < 0): SOC increases by `energy_delta * eff`.
4. Clamp SOC to [0, capacity].
5. Derive status from current MW.
6. Emit telemetry snapshot.

## DB schema (auto-created)
- `sites`: id (uuid PK), name, location.
- `assets`: id (uuid PK), site_id (FK), name, location, capacity_mwhr, max_mw, min_mw, efficiency, ramp_rate_mw_per_min.
- `telemetry`: asset_id (FK), site_id (FK), ts (timestamptz), soc_mwhr, soc_pct, capacity_mwhr, current_mw, setpoint_mw, max_mw, min_mw, status.
- `dispatches`: id (uuid PK), asset_id (FK), mw, duration_s, status, reason, submitted_at.

## How to add a new asset (YAML-driven)
1) Edit `assets.yaml` and add a site (if needed) under `sites:` with a stable UUID, name, and location.
2) Add a new entry under `assets:` with a stable UUID, `site_id` (matching one of the sites), and operating params (`capacity_mwhr`, `max_mw`, `min_mw`, `ramp_rate_mw_per_min`, `efficiency`).
3) Restart the simulator:
   - If `DATABASE_URL` is set, the service will upsert the new site/asset into the DB and start simulating it immediately.
   - If running in-memory only, the asset appears as soon as the service restarts.

## What if I remove an asset from YAML?
- On restart, the reconciliation step prunes telemetry and dispatch rows for assets not listed in YAML (when `DATABASE_URL` is set). The asset will disappear from the simulator and its DB rows will be removed to avoid drift.
- If you want to keep history for removed assets, avoid deleting from YAML; instead, mark them separately (this simulator currently prunes unknown assets).

## How to run locally with Postgres/Timescale
1. `docker-compose up -d` (uses `timescale/timescaledb:2.15.2-pg15`, user/pass/db = `bess`/`besspassword`/`bess`).
2. `DATABASE_URL=postgres://bess:besspassword@localhost:5432/bess cargo run`
3. Try requests:
   - `GET http://127.0.0.1:3001/assets`
   - `GET http://127.0.0.1:3001/telemetry/{asset_id}`
   - `POST http://127.0.0.1:3001/dispatch` with JSON:
     ```json
     {"asset_id": "UUID_HERE", "mode": "discharge", "mw": 10.0}
     ```

## Not yet implemented (future work)
- Duration-based dispatch completion and status progression.
- Asset registration endpoint (currently seeded only).
- Historical telemetry query endpoints (currently only live/latest via in-memory; DB fills history).
- Validation of SOC bounds for dispatch duration (energy check).
- Streaming interfaces (WebSocket/gRPC) and advanced protocols.
- Fault injection, latency/jitter, richer models (efficiency curves, temperature).
- **Multi-agent split (proposed)**: run each BESS simulator as its own agent/process with a headend orchestrator.
  - Headend: owns REST/API, YAML/config, DB, and dispatch logic; pushes setpoints to agents; ingests telemetry/health.
  - BESS Agent: one per asset; runs the local tick loop and publishes telemetry back to headend (HTTP/gRPC/event bus).
  - Comms: headend→agent for setpoints, agent→headend for telemetry; design for retries/backpressure and fault isolation.
  - Benefits: isolates faults per asset, scales BESS simulation independently, supports heterogeneous models per agent.
