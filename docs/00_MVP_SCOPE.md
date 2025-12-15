# integrated_system/docs/00_MVP_SCOPE.md

# MVP Scope — Distributed DER Telemetry + Dispatch (Headend + Agents)

## Product intent (1 paragraph)
Provide a lightweight, outbound-only edge agent + central headend that (a) standardizes DER telemetry ingestion, (b) exposes simple APIs for downstream apps, and (c) supports low-latency dispatch/setpoints — without dragging customers into a monolithic SCADA/headend rebuild.

## Primary users
- **Ops / Control room support**: needs reliable “what’s happening right now?” and basic dispatch.
- **Integration engineers**: wants simple APIs, stable IDs, predictable data model.
- **Platform/SRE**: wants observable services, safe failure modes, easy rollout.

## MVP = what is *true today* in this repo
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
  - Optionally persists telemetry, dispatches, sessions, heartbeats to Postgres
- **Launcher** (`agent_launcher`): one-process-per-asset for local simulation/bring-up

### MVP APIs (behavioral contract)
- REST (headend):
  - `GET /assets` — list assets
  - `GET /agents` — connected agents (best-effort, current session view)
  - `GET /telemetry/{id}` — latest telemetry snapshot (in-memory)
  - `GET /telemetry/{id}/history` — history from DB (if configured)
  - `POST /dispatch` — create dispatch; headend pushes setpoint to agent if online, or queues as pending if offline
  - `GET /dispatch/history` — dispatch log (DB-backed if configured)
  - `GET /heartbeat/{id}` and `/history` — last heartbeat / history (DB-backed if configured)
- gRPC (agent-initiated):
  - stream Register → Telemetry/Heartbeat
  - headend pushes Setpoint messages on the same stream

## Non-goals (explicitly NOT MVP)
- Multi-tenant isolation (org/project/workspace)
- AuthN/AuthZ (mTLS, OAuth/JWT, RBAC)
- Durable event bus / guaranteed delivery semantics
- Exactly-once ingestion / replay protection
- Fleet management (remote upgrade, config rollout, device provisioning)
- Advanced data model (quality flags, alarms/events, time sync validation)
- High availability / geo-redundancy claims

## MVP success criteria (ship/market readiness bar)
- 1 headend + N agents can run continuously for 7 days with:
  - No manual restarts required after transient network drops
  - Telemetry available “near-real-time” (≤10s end-to-end typical)
  - Dispatch reaches agent and is applied within ≤2s typical when connected
- A new asset can be added via YAML + restart, and shows up with stable ID
- Basic audit trail exists when Postgres is enabled (telemetry + dispatch + sessions)

## “MVP-plus” (allowed if it doesn’t explode scope)
- Better docs + diagrams + runbooks
- Structured logs, metrics endpoints, and a minimal healthcheck
- Clean separation of “simulation” vs “real DER adapter” inside the agent