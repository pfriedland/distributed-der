# MVP User Interface

This document describes the current MVP user interface surface (API-first) and an incremental path to a demo-friendly UI.

## Purpose
- Provide a clear “how do I use it” guide for the MVP.
- Define a realistic UI trajectory without committing to a heavy front-end too early.

## Current UI surface (MVP = API-first)

Today, the MVP user interface is primarily:
- **HTTP/JSON endpoints** on `der_headend` (operator and developer facing)
- **Logs** (headend + agent) for observability during demos

This is intentional: it keeps iteration speed high while the core behaviors (registration, routing, gateway split, durability) stabilize.

## Primary operator workflows (today)

### 1) View assets
- Goal: confirm the headend loaded the correct `ASSETS_PATH` and understand site/asset IDs.

```bash
curl -s http://127.0.0.1:3001/assets | jq .
```

What to look for:
- each asset’s `site_id`, `name`, `capacity_mwhr`, `min_mw`, `max_mw`

### 2) View connected agents
- Goal: confirm edge connectivity and which assets are represented.

```bash
curl -s http://127.0.0.1:3001/agents | jq .
```

What to look for:
- `connected: true`
- `peer`
- `site_id` / `site_name`
- session timestamps (reconnects)

### 3) Dispatch (BESS charge/discharge)
Dispatch uses signed MW:
- **+MW = discharge/export**
- **-MW = charge/import**

#### Per-asset dispatch

```bash
HEADEND=http://127.0.0.1:3001
ASSET_ID=<asset-uuid>

curl -s -X POST "$HEADEND/dispatch" \
  -H 'content-type: application/json' \
  -d "{\"asset_id\":\"$ASSET_ID\",\"mw\":5}" | jq .
```

#### Per-site dispatch (Gateway Mode)

```bash
HEADEND=http://127.0.0.1:3001
SITE_ID=<site-uuid>

curl -s -X POST "$HEADEND/dispatch" \
  -H 'content-type: application/json' \
  -d "{\"site_id\":\"$SITE_ID\",\"mw\":12}" | jq .
```

Expected behavior:
- Per-asset dispatch affects exactly one asset.
- Per-site dispatch fans out across assets at the site using capacity-weighted split + min/max clamps.

### 4) Observe results (logs)
During MVP demos, “UI feedback” is often the logs.

Suggested run commands:

```bash
# headend
RUST_LOG=info cargo run -p der_headend

# edge agent
RUST_LOG=info cargo run -p edge_agent
```

Look for:
- headend logs confirming dispatch routing and asset/site lookup
- agent logs confirming setpoint application and (for Gateway Mode) split allocations

## UX gaps (known MVP limitations)
- No dedicated operator web UI yet.
- No real-time charting built-in.
- Limited “stateful” visualization (e.g., setpoint history, trend lines, event lists) outside the database.

## UI evolution (recommended trajectory)

### Stage 0 — API + logs (current)
- Keep endpoints stable.
- Improve error messages and response shapes.
- Add request IDs and consistent status codes.

### Stage 1 — Demo dashboard (local)
Goal: a low-effort UI that is impressive and debuggable.

Recommended approach:
- Add a small **read-only** dashboard that shows:
  - connected agents and represented assets
  - last setpoint per asset
  - last telemetry timestamp per asset
  - events/alarms list (when implemented)

Options:
- A simple static HTML page served by `der_headend` (no build toolchain)
- OR a minimal React/Vite app if desired (more polish, more complexity)

### Stage 2 — Operator console (pilot)
Goal: pilot-ready workflow UI.

Add:
- dispatch form (per-asset and per-site)
- simple charts (last hour/day) for `site_mw`, voltage, frequency
- event/alarm acknowledgement (if supported)

Recommended stack:
- Time-series DB + Grafana for dashboards
- “Operator console” stays thin (dispatch + workflow)

### Stage 3 — Managed pilot UI (pre-SaaS)
Goal: support multiple pilot sites with access control.

Add:
- authentication/authorization
- audit trail for setpoints
- role-based access to sites

### Stage 4 — SaaS UI (multi-tenant)
Goal: multi-tenant fleet management.

Add:
- tenant isolation
- self-service onboarding (site/asset registration)
- provisioning and monitoring

## UI + Time-series databases
For high-volume telemetry (wind/inverter “by exception” + select fast points):
- use a TSDB designed for large-scale ingestion + fast queries
- pair with Grafana for visualization

This MVP doc set stays TSDB-agnostic for now; see the roadmap and telemetry spec:
- `docs/03_ROADMAP_V1.md`
- `docs/07_TELEMETRY_MINIMAL_PILOT.md`

## References
- Architecture: [docs/01_ARCHITECTURE_CURRENT.md](01_ARCHITECTURE_CURRENT.md)
- Installation: [docs/08_INSTALLATION.md](08_INSTALLATION.md)
- Runbook: [docs/09_RUNBOOK.md](09_RUNBOOK.md)
