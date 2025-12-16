# Runbook — Local Demo and Pilot Operations (MVP)

This runbook is a practical operator guide for bringing up the MVP, running a demo, and diagnosing common issues.

## Audience
- Developers running the stack locally
- Demo operators
- Early pilot operators (non-production)

## Quick definitions
- **Headend**: `der_headend` (HTTP + gRPC server)
- **Edge agent**: `edge_agent` (connects to headend gRPC; single-asset or Gateway Mode)
- **Gateway Mode**: one agent represents a site and can fan-out commands to multiple assets

## Standard ports (local)
- Headend HTTP: `127.0.0.1:3001`
- Headend gRPC: default `127.0.0.1:50051` (commonly overridden to `50070`)

## Environment variable cheat sheet

### Headend (`der_headend`)
- `ASSETS_PATH` (required) — path to assets YAML
- `HEADEND_GRPC_ADDR` (optional) — gRPC bind address (e.g., `127.0.0.1:50070`)
- `DATABASE_URL` (optional) — Postgres connection string
- `DB_RESET` (optional) — destructive reset on startup when truthy (local/dev only)

### Edge agent (`edge_agent`)
- `HEADEND_GRPC` (required) — headend gRPC address (e.g., `127.0.0.1:50070`)

Single-asset mode:
- `ASSET_ID`, `ASSET_NAME`
- `SITE_ID`, `SITE_NAME`
- `ASSET_LOCATION`
- `CAPACITY_MWHR`, `MAX_MW`, `MIN_MW`, `EFFICIENCY`, `RAMP_RATE_MW_PER_MIN`

Gateway Mode:
- `SITE_ID` (required)
- `ASSETS_PATH` (required) — used to discover all assets for the site
- `GATEWAY_ID` (optional) — defaults to `SITE_ID`

### Logging
- `RUST_LOG` (recommended) — e.g., `info`, `debug`

## Startup checklist (local)

### 1) Start DB (optional)
If using compose:

```bash
docker compose up -d
# or: podman-compose up -d
```

### 2) Start headend

```bash
export ASSETS_PATH=der_headend/assets_test.yaml
export HEADEND_GRPC_ADDR=127.0.0.1:50070
# optional:
export DATABASE_URL=postgres://bess:besspassword@localhost:5432/bess
# optional destructive reset:
# export DB_RESET=1

RUST_LOG=info cargo run -p der_headend
```

### 3) Sanity check headend

```bash
curl -s http://127.0.0.1:3001/assets | jq .
curl -s http://127.0.0.1:3001/agents | jq .
```

Expected:
- `/assets` returns assets from `ASSETS_PATH`
- `/agents` is empty until an agent connects

### 4) Start edge agent

#### Single-asset mode

```bash
export HEADEND_GRPC=127.0.0.1:50070
export ASSET_ID=7ba7b810-9dad-11d1-80b4-00c04fd430c8
export ASSET_NAME=SA-BESS-1
export SITE_ID=6ba7b810-9dad-11d1-80b4-00c04fd430c8
export SITE_NAME=SingleAsset-Test
export ASSET_LOCATION=Local
export CAPACITY_MWHR=120
export MAX_MW=60
export MIN_MW=-60
export EFFICIENCY=0.92
export RAMP_RATE_MW_PER_MIN=1000

RUST_LOG=info cargo run -p edge_agent
```

#### Gateway Mode (one agent per site)

```bash
export HEADEND_GRPC=127.0.0.1:50070
export SITE_ID=6ba7b810-9dad-11d1-80b4-00c04fd430c8
export ASSETS_PATH=der_headend/assets_test.yaml
# optional:
# export GATEWAY_ID=$SITE_ID

RUST_LOG=info cargo run -p edge_agent
```

### 5) Verify registration

```bash
curl -s http://127.0.0.1:3001/agents | jq .
```

Expected:
- One “connected” peer entry
- In Gateway Mode, all assets in that site appear under the agent’s session/representation

## Demo flow (script)

### 1) Identify IDs

```bash
HEADEND=http://127.0.0.1:3001
curl -s $HEADEND/assets | jq .
```

Capture:
- `SITE_ID`
- one `ASSET_ID` (for per-asset dispatch)

### 2) Per-asset dispatch

```bash
ASSET_ID=7ba7b810-9dad-11d1-80b4-00c04fd430c8
curl -s -X POST "$HEADEND/dispatch" \
  -H 'content-type: application/json' \
  -d "{\"asset_id\":\"$ASSET_ID\",\"mw\":5}" | jq .
```

### 3) Per-site dispatch (Gateway Mode)

```bash
SITE_ID=6ba7b810-9dad-11d1-80b4-00c04fd430c8
curl -s -X POST "$HEADEND/dispatch" \
  -H 'content-type: application/json' \
  -d "{\"site_id\":\"$SITE_ID\",\"mw\":12}" | jq .
```

Expected:
- Edge agent logs show the split across assets (capacity weighted), with clamp behavior as needed.

## Operational checks

### Check connected agents

```bash
curl -s http://127.0.0.1:3001/agents | jq .
```

Look for:
- `connected: true`
- correct `site_id` / `site_name`
- session timestamps increasing only when reconnects occur

### Check loaded assets

```bash
curl -s http://127.0.0.1:3001/assets | jq .
```

Look for:
- correct `site_id` grouping
- `capacity_mwhr`, `min_mw`, `max_mw` values

## Common issues and fixes

### Agents don’t appear in `/agents`
Likely causes:
- gRPC port mismatch (headend vs agent)
- headend not running
- `HEADEND_GRPC` wrong

Checklist:
- confirm headend gRPC bind: `HEADEND_GRPC_ADDR`
- confirm agent target: `HEADEND_GRPC`
- common mismatch: `50051` vs `50070`

### Headend can’t find assets
Symptom:
- `Error: assets.yaml not found. Set ASSETS_PATH or place assets.yaml in the headend cwd or workspace root.`

Fix:
- set `ASSETS_PATH` explicitly to the YAML you want.

### Dispatch seems to route “wrong”
Reminders:
- **Per-asset** dispatch uses `asset_id`.
- **Per-site** dispatch uses `site_id`.
- Precedence rule (MVP): if a command includes both, treat it as **per-asset**.

### Duplicate sessions in `/agents`
This can happen if agents were restarted without clearing the DB.

Fix options:
- run headend with `DB_RESET=1` (destructive local reset), or
- reset the Postgres volume if using compose.

### Capacity-weighted split doesn’t sum exactly
Small residuals can occur due to floating-point rounding.

Expected behavior:
- allocations should sum to the requested total **within a small tolerance**.
- the agent should log raw, clamped, and residual adjustments.

## Reset procedures

### Reset headend state
If using DB:

```bash
export DB_RESET=1
cargo run -p der_headend
```

If using compose-managed Postgres:

```bash
docker compose down -v
docker compose up -d
```

### Reset agent state
- stop the agent process
- restart it with the same env vars

## Escalation checklist (when filing a bug)
Include:
- exact command lines used to start headend/agent
- environment variables relevant to the run
- headend logs around the failing request
- agent logs around connect/register/dispatch
- `/assets` and `/agents` output (sanitized)

## References
- Installation: [docs/08_INSTALLATION.md](08_INSTALLATION.md)
- Test plan: [docs/05_TEST_PLAN_GATEWAY_MODE.md](05_TEST_PLAN_GATEWAY_MODE.md)
- Telemetry spec: [docs/07_TELEMETRY_MINIMAL_PILOT.md](07_TELEMETRY_MINIMAL_PILOT.md)
