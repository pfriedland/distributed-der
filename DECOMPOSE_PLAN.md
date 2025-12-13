# Plan: Split Headend and Per-BESS Agents

## Goals
- Headend service owns REST API, config (assets.yaml), DB schema, dispatch logic, and telemetry persistence.
- One agent per BESS asset runs the tick loop and reports telemetry; headend delivers setpoints.
- Keep assets stable via YAML/DB; agents are stateless aside from in-memory sim state.

## Components
- **headend** (new crate/bin):
  - Loads assets.yaml, reconciles DB, restores SOC/setpoints.
  - REST API: `/assets`, `/telemetry/{id}`, `/telemetry/{id}/history`, `/dispatch`.
  - Pushes setpoints to agents; collects telemetry from agents; writes to DB.
  - Maintains registry of active agents (asset_id → agent address).
- **bess_agent** (new crate/bin):
  - Runs tick loop for a single asset (passed via env/args).
  - Exposes a small API to accept setpoint updates (e.g., `POST /setpoint {mw}`) and to report telemetry (`POST /telemetry` to headend).
  - Supports init from optional seed state (SOC/current/setpoint) provided by headend.

## Transport / API contracts (proposed)
- Agent API (HTTP):
  - `POST /setpoint` `{ mw: f64, duration_s?: u64 }` (sign = discharge/charge). Agent applies and returns 202.
  - `GET /health` → {status:"ok"}.
- Agent → Headend (HTTP callbacks):
  - `POST {HEADEND}/telemetry` `{ asset_id, site_id, ts, soc_mwhr, soc_pct, capacity_mwhr, current_mw, setpoint_mw, max_mw, min_mw, status }`
  - Agent sends every tick (e.g., 4s).
- Headend registry:
  - Static (from YAML: agent_url per asset) or dynamic (agent registers on startup with `POST /register {asset_id, agent_url}`).

## Data flow
- Startup:
  - Headend loads YAML, reconciles DB, restores last SOC/current/setpoint.
  - Headend knows agent URLs (from YAML or registration).
  - Headend sends initial setpoint to each agent; optionally seeds agent with state (SOC/current).
- Tick:
  - Agent runs tick and posts telemetry to headend; headend stores in DB.
  - Dispatch: client calls headend `/dispatch`; headend validates, persists, and POSTs setpoint to agent.

## DB ownership
- Headend keeps the authoritative DB for orchestration, API, and history.
- Agents maintain local state, including a local Postgres/Timeseries instance, to log their own telemetry and state (for offline resilience or debugging).
- Headend still ingests telemetry for fleet-wide history; agent DB is local-only and not considered the source of truth for the headend.

## Work breakdown
1) Create workspace with crates: `headend`, `bess_agent`, and a shared `sim-core` (models, tick logic).
2) Move sim logic (BessState, tick) and shared models (Asset, Telemetry, Dispatch) into `sim-core`.
3) Headend:
   - Load YAML → assets/sites.
   - Reconcile DB (sites/assets), restore state (SOC/current/setpoint), reapply dispatch MW.
   - REST API unchanged, but dispatch handler POSTs setpoint to agent URL.
   - `/telemetry` ingest endpoint for agents to push telemetry.
   - Registry: map asset_id → agent_url (from YAML field `agent_url` or via registration endpoint).
4) Agent:
   - Accept asset config via env/args (asset_id, capacity, limits, ramp, efficiency, headend URL).
   - `/setpoint` to update setpoint; tick loop sending telemetry to headend.
   - Optionally accept initial state (SOC/current/setpoint) on startup.
   - Maintain local timeseries/Postgres for telemetry snapshots (for offline/diagnostic use); this DB is agent-local and not authoritative for headend.
5) Update YAML schema:
   - Add `agent_url` per asset.
6) Tests/manual:
   - Run headend (with DB) + two agents (different ports/asset_ids), verify telemetry ingestion and dispatch propagation.
7) Finalize docs and handoff:
   - Document YAML schema (site_id, agent_url) and startup steps.
   - Capture local Postgres/timeseries setup for agents (optional but recommended for local logging).
   - Provide sample docker-compose for headend DB and sample agent configs.

## Open decisions
- Auth between headend/agents? (e.g., shared token).
- Retries/backpressure for telemetry and setpoint delivery.
- Whether agents auto-register vs. static `agent_url` in YAML.
- Whether headend pushes initial state to agents or agents pull on startup.
