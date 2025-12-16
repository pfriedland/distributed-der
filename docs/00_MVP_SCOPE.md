# integrated_system/docs/00_MVP_SCOPE.md

# MVP Scope — DERMS Core (Fleet Connectivity, Telemetry, Dispatch)

## Product intent
Build a **DER Management System (DERMS) core** that manages a fleet of distributed energy resources by providing **secure, outbound-initiated edge connectivity**, **normalized telemetry ingestion**, and **low-latency dispatch/setpoint control**. The MVP prioritizes operational reliability and simple integration APIs, establishing the foundation for DERMS capabilities such as program orchestration, constraint-aware control, and utility/market integrations in later phases.

## DERMS definition used in this document
In this project, “DERMS” refers to the software system responsible for **fleet connectivity, telemetry management, and dispatch/control** of DER assets. Advanced DERMS functions (for example: constraint optimization, hosting capacity analysis, feeder models, Volt/VAR control, market bidding/settlement, and deep ADMS/EMS integration) are explicitly **out of scope** for the MVP unless stated otherwise.

## Outbound-initiated edge connectivity (what “outbound-only” means here)
In this MVP, the `edge_agent` is **outbound-initiated**: it **dials out** to the headend and keeps a long-lived connection open. The site does **not** need any inbound firewall/NAT rule that allows the headend (or the internet) to initiate a new connection into the site.

- **Agent → Headend:** the agent initiates and maintains the connection (typically TLS).
- **Headend → Agent:** setpoints are delivered **back to the agent over the same established connection** (bidirectional gRPC stream), not via a separate inbound connection.

Practically, this often means you only need to allow **outbound TCP** from the site to the headend on the chosen port.

Note: “outbound-only” can be misread as “one-way data.” This design is **two-way**, but **outbound-initiated**.

## Primary users
- **DER Operations / Control Center** (utility, IPP, aggregator): needs reliable “what’s happening now?” and basic dispatch.
- **Integration / OT-IT engineers**: wants simple APIs, stable IDs, predictable data model, easy onboarding.
- **Site commissioning / field engineering**: needs repeatable site bring-up and troubleshooting.
- **Platform/SRE**: wants observable services, safe failure modes, and straightforward rollout.

## MVP = what is true today in this repo
### Core capabilities
- **Asset config**: assets defined in `assets.yaml` (sites + assets with stable UUIDs).
- **Edge agent** (`edge_agent`): outbound bidirectional gRPC stream to headend:
  - Sends **Register**, periodic **Telemetry** (~4s), periodic **Heartbeat** (~30s)
  - Receives **Setpoint** (mw + optional duration_s)
  - Reconnects on failure with simple backoff
- **Headend** (`der_headend`):
  - Hosts **REST API** for assets, telemetry, dispatch, agent presence
  - Hosts **gRPC** server that accepts agent streams and pushes setpoints
  - Keeps **latest telemetry** in-memory for fast reads
  - Optionally persists telemetry, dispatches, sessions, heartbeats to Postgres/Timescale (if configured)
- **Launcher** (`agent_launcher`): *test-only* utility that reads `assets.yaml` and spawns one `edge_agent` per asset for local simulation/bring-up; **not used for production deployments**.

### MVP APIs (behavioral contract)
- REST (headend):
  - `GET /assets` — list assets
  - `GET /agents` — connected agents (best-effort, current session view)
  - `GET /telemetry/{id}` — latest telemetry snapshot (in-memory)
  - `GET /telemetry/{id}/history` — history from DB (if configured)
  - `POST /dispatch` — create dispatch; headend pushes setpoint to agent if online, or stores as pending if offline
  - `GET /dispatch/history` — dispatch log (DB-backed if configured)
  - `GET /heartbeat/{id}` and `/history` — last heartbeat / history (DB-backed if configured)
- gRPC (agent-initiated):
  - Stream: agent sends Register/Telemetry/Heartbeat; headend pushes Setpoint messages on the same stream.

## Edge deployment model (production direction)
### Current (MVP): single-asset agent
- One `edge_agent` instance represents **one** DER asset (`asset_id`).
- This is the simplest model for bring-up, integration testing, and early pilots.

### Planned (V1): Edge Gateway Mode (VPP site controller)
- One `edge_agent` instance represents a **site gateway** that manages **multiple** DER assets behind it (PV, BESS, meter, genset, etc.).
- The headend continues to address **individual assets** by `asset_id`.
- Telemetry and setpoints are **multiplexed** over a single outbound connection.
- Detailed design is captured in `RFC_EDGE_GATEWAY_MODE.md`.

## Non-goals (explicitly NOT MVP)
### Platform capabilities not in MVP
- Multi-tenant isolation (org/project/workspace boundaries)
- AuthN/AuthZ (mTLS, OAuth/JWT, RBAC)
- Durable event bus / message queue and formal delivery guarantees
- Exactly-once ingestion, idempotency keys, replay protection
- Fleet management (remote upgrade, config rollout, device provisioning)
- High availability / geo-redundancy claims
- Edge Gateway Mode (multi-asset fan-out from a single agent process)

### Advanced “full DERMS” functions not in MVP
- Hosting capacity analysis, feeder constraint modeling, network state estimation
- Constraint-aware or optimal dispatch (OPF, Volt/VAR optimization, feeder limits)
- Program orchestration (DR events, TOU programs, VPP programs)
- Utility enterprise integrations (ADMS/OMS/EMS/SCADA) beyond simple APIs
- Market participation workflows (bidding, awards, settlement)

## MVP success criteria (ship / pilot readiness bar)
- 1 headend + N agents can run continuously for **7 days** with:
  - No manual restarts required after transient network drops
  - Telemetry available “near-real-time” (≤10s end-to-end typical)
  - Dispatch reaches agent and is applied within ≤2s typical when connected
- A new DER site/asset can be added via YAML + restart and appears with a **stable asset ID**
- When Postgres is enabled, a basic audit trail exists for:
  - Telemetry history
  - Dispatch history
  - Agent sessions / heartbeats
- Commissioning bar: a new site can be onboarded using a **repeatable checklist** (docs) without reading source code.

## “MVP-plus” (allowed if it doesn’t explode scope)
- Better docs, diagrams, and runbooks
- Structured logs; basic metrics endpoint; minimal healthcheck
- Clean separation inside the agent between “simulation” and “real DER adapter”
- Hardened reconnect behavior (bounded retries/backoff, clearer error messages)
- Documented path to Edge Gateway Mode (RFC + proto/interface plan) without implementing full VPP optimization