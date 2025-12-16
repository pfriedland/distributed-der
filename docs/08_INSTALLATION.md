# Installation and Local Dev Setup

This document describes how to run the **headend** and **edge agent** locally for development and demos.

## Prerequisites

### Required
- Rust toolchain (stable) with `cargo`
- `git`
- `curl`

### Optional (recommended)
- Docker/Podman + Compose (for a local Postgres/TimescaleDB)
- `jq` (pretty-print API responses)

## Repo layout (relevant)
- `der_headend/` — headend REST + gRPC server
- `edge_agent/` — outbound edge agent client
- `agent_launcher/` — testing harness (spawns agents; optional)
- `docs/` — design docs
- `docker-compose.yml` — optional local database (TimescaleDB/Postgres)

## Step 1 — Clone and build

```bash
git clone <your-repo-url>
cd integrated_system
cargo build
```

## Step 2 — Configure assets

The headend loads asset metadata from an assets YAML file.

### Option A: Use the default assets file
- `der_headend/assets.yaml` (or your `assets_test.yaml` in `der_headend/`)

### Option B: Point to a specific assets file
Set `ASSETS_PATH` to an absolute or repo-relative path:

```bash
export ASSETS_PATH=der_headend/assets_test.yaml
```

If you see:
`Error: assets.yaml not found. Set ASSETS_PATH or place assets.yaml in the headend cwd or workspace root.`
…it means the process can’t find the assets file from the current working directory. Use `ASSETS_PATH` (recommended).

## Step 3 — (Optional) Start the database

The headend can run without a DB (in-memory), but for persistence and /agents session history you’ll want Postgres.

### Using docker-compose
From repo root:

```bash
docker compose up -d
# or: podman-compose up -d
```

Default compose settings (see `docker-compose.yml`):
- host port: `5432`
- user: `bess`
- password: `besspassword`
- database: `bess`

Set:

```bash
export DATABASE_URL=postgres://bess:besspassword@localhost:5432/bess
```

### Resetting the DB

There are two reset options:

#### Option A: Reset using `DB_RESET` (recommended)
`der_headend` supports a destructive DB reset on startup when `DB_RESET` is set to a truthy value.

Example:

```bash
export DB_RESET=1
cargo run -p der_headend
```

Notes:
- This is **destructive** (intended for local dev / demos).
- Use this only against a local/dev database.

#### Option B: Reset by dropping the container volume
If you are running Postgres via compose, you can also reset by dropping the volume:

```bash
docker compose down -v
docker compose up -d
```

## Step 4 — Start the headend

The headend exposes:
- HTTP (currently fixed): `127.0.0.1:3001`
- gRPC (default): `127.0.0.1:50051` (override with `HEADEND_GRPC_ADDR`)

Recommended env vars:

```bash
export ASSETS_PATH=der_headend/assets_test.yaml
export HEADEND_GRPC_ADDR=127.0.0.1:50070
# optional:
export DATABASE_URL=postgres://bess:besspassword@localhost:5432/bess
# export DB_RESET=1
```

Run:

```bash
cargo run -p der_headend
```

Sanity checks:

```bash
curl -s http://127.0.0.1:3001/assets | jq .
curl -s http://127.0.0.1:3001/agents | jq .
```

## Step 5 — Start an edge agent

The edge agent connects to headend gRPC using `HEADEND_GRPC`.

### Option A: Single-asset mode (classic)
This mode uses explicit identity parameters (see `.env.agent`).

Example:

```bash
export HEADEND_GRPC=127.0.0.1:50070
export ASSET_ID=7ba7b810-9dad-11d1-80b4-00c04fd430c8
export ASSET_NAME=BESS-1
export SITE_ID=6ba7b810-9dad-11d1-80b4-00c04fd430c8
export SITE_NAME=Jersey
export ASSET_LOCATION=Region-A
export CAPACITY_MWHR=120
export MAX_MW=60
export MIN_MW=-60
export EFFICIENCY=0.92
export RAMP_RATE_MW_PER_MIN=1000

cargo run -p edge_agent
```

### Option B: Gateway Mode (one agent per site)
In Gateway Mode, one agent represents multiple assets at a site.

Typical configuration (current direction):
- `HEADEND_GRPC` — headend gRPC address
- `SITE_ID` — the site the gateway represents
- `ASSETS_PATH` — used to discover all assets under the site
- (optional) `GATEWAY_ID` — defaults to `SITE_ID` if not set

Example:

```bash
export HEADEND_GRPC=127.0.0.1:50070
export SITE_ID=6ba7b810-9dad-11d1-80b4-00c04fd430c8
export ASSETS_PATH=der_headend/assets_test.yaml
# optional:
# export GATEWAY_ID=$SITE_ID

cargo run -p edge_agent
```

Verify registration:

```bash
curl -s http://127.0.0.1:3001/agents | jq .
```

## Step 6 — Send dispatch commands

### Per-asset dispatch

```bash
HEADEND=http://127.0.0.1:3001
ASSET_A=7ba7b810-9dad-11d1-80b4-00c04fd430c8

curl -s -X POST "$HEADEND/dispatch" \
  -H 'content-type: application/json' \
  -d "{\"asset_id\":\"$ASSET_A\",\"mw\":5}" | jq .
```

### Per-site dispatch (Gateway Mode)

```bash
HEADEND=http://127.0.0.1:3001
SITE_ID=6ba7b810-9dad-11d1-80b4-00c04fd430c8

curl -s -X POST "$HEADEND/dispatch" \
  -H 'content-type: application/json' \
  -d "{\"site_id\":\"$SITE_ID\",\"mw\":12}" | jq .
```

In Gateway Mode, the agent may split the site dispatch across assets using capacity weighting and asset min/max clamps (see `docs/05_TEST_PLAN_GATEWAY_MODE.md`).

## Troubleshooting

### gRPC port mismatch
If agents never appear in `/agents`, verify:
- headend gRPC is on the port you think it is (`HEADEND_GRPC_ADDR`)
- edge agent `HEADEND_GRPC` points to the same port

Common mismatch: headend default `50051` vs expected `50070`.

### Assets not loading
- Ensure `ASSETS_PATH` points to the file you’re editing.
- Ensure `site_id` and `asset_id` values match what your agent is using.

### DB not persisting
- Confirm `DATABASE_URL` is set in the environment of the headend process.
- Confirm Postgres is reachable: `psql $DATABASE_URL` (or container healthcheck).
- If you intended a fresh start, use `DB_RESET=1` for a destructive reset on headend startup.

## Next steps
- Follow the Gateway Mode test plan: `docs/05_TEST_PLAN_GATEWAY_MODE.md`
- Review the minimal pilot telemetry spec (Wind + PV + events): `docs/07_TELEMETRY_MINIMAL_PILOT.md`
