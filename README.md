# Distributed DER Telemetry + Dispatch Platform (MVP)

This repo contains a Rust-based MVP for a distributed DER telemetry + dispatch platform (DERMS foundation) with outbound-only edge connectivity and an evolving path toward a **managed pilot service** (with a credible trajectory to multi-tenant SaaS).

## Table of contents

### Product and design docs
- **Cutsheet (product positioning):** [docs/06_CUTSHEET.md](docs/06_CUTSHEET.md)
- **Current architecture:** [docs/01_ARCHITECTURE_CURRENT.md](docs/01_ARCHITECTURE_CURRENT.md)
- **Capability matrix:** [docs/02_CAPABILITY_MATRIX.md](docs/02_CAPABILITY_MATRIX.md)
- **Roadmap (V1 → pilots → SaaS trajectory):** [docs/03_ROADMAP_V1.md](docs/03_ROADMAP_V1.md)
- **Gateway mode design note (RFC):** [docs/04_RFC_EDGE_GATEWAY_MODE.md](docs/04_RFC_EDGE_GATEWAY_MODE.md)
- **Gateway mode test plan:** [docs/05_TEST_PLAN_GATEWAY_MODE.md](docs/05_TEST_PLAN_GATEWAY_MODE.md)
- **Minimal pilot telemetry spec (Wind + PV + events):** [docs/07_TELEMETRY_MINIMAL_PILOT.md](docs/07_TELEMETRY_MINIMAL_PILOT.md)

### Code layout
- `der_headend/` — headend REST + gRPC services (control plane)
- `edge_agent/` — outbound edge agent (single-asset or Gateway Mode)
- `agent_launcher/` — local testing harness (spawns agents)
- `docs/` — design docs, test plans, and narrative

## Dev quickstart (local)

### Quick links
- Architecture overview: [docs/01_ARCHITECTURE_CURRENT.md](docs/01_ARCHITECTURE_CURRENT.md)
- Test plan (Gateway Mode): [docs/05_TEST_PLAN_GATEWAY_MODE.md](docs/05_TEST_PLAN_GATEWAY_MODE.md)
- Telemetry spec (pilot): [docs/07_TELEMETRY_MINIMAL_PILOT.md](docs/07_TELEMETRY_MINIMAL_PILOT.md)

### 1) Start the headend
Typical environment variables:
- `ASSETS_PATH` — path to an assets YAML file (e.g., `der_headend/assets_test.yaml`)
- `DATABASE_URL` — Postgres connection string (optional, but recommended)
- `HEADEND_GRPC_ADDR` — gRPC bind address (server-side)

Run:
```bash
export ASSETS_PATH=der_headend/assets_test.yaml
# export DATABASE_URL=postgres://...
# export HEADEND_GRPC_ADDR=127.0.0.1:50070
cargo run -p der_headend
```

### 2) Start an edge agent
Typical environment variables:
- `HEADEND_GRPC` — headend gRPC address (client-side)

**Single-asset mode**
- `ASSET_ID`

**Gateway mode (one agent per site)**
- `SITE_ID`
- `ASSETS_PATH` (used to discover all assets for the site)

Run:
```bash
export HEADEND_GRPC=127.0.0.1:50070
# Single-asset:
# export ASSET_ID=...
# Gateway mode:
# export SITE_ID=...
# export ASSETS_PATH=der_headend/assets_test.yaml
cargo run -p edge_agent
```

## Notes
- The MVP supports both **per-asset** and **per-site** dispatch.
- For Gateway Mode, preserve the precedence rule: if a command includes both `asset_id` and `site_id`, treat it as **per-asset**.
- Telemetry durability and event/alarming support are planned V1 items; see [docs/03_ROADMAP_V1.md](docs/03_ROADMAP_V1.md) and [docs/07_TELEMETRY_MINIMAL_PILOT.md](docs/07_TELEMETRY_MINIMAL_PILOT.md).
