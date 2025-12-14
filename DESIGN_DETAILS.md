# Design Details (MVP Snapshot)

This document captures the current MVP architecture of the distributed DER simulator and how it differs from a traditional Central SCADA + PI stack. It also notes near-term scalability considerations.

## Core Components (MVP)

- **Headend (der_headend)**  
  - REST API: `/assets`, `/telemetry/{id}`, `/telemetry/{id}/history` (empty array if no DB), `/dispatch` (JSON ack), `/agents` (connected agent list).  
  - gRPC (AgentLink::Stream): agent-initiated bidirectional stream; agents send Register + Telemetry; headend pushes Setpoint.  
  - Persistence (optional): Postgres for telemetry (`telemetry` table) and agent sessions (`agent_sessions` with asset/site names, peer, connected/disconnected timestamps).  
  - Logging: request/response tracing for HTTP; agent connect/disconnect with asset/site/peer; dispatch failures include MW bounds and connection state.  
  - Tick loop: still present; disable if you want agent-only telemetry.

- **Edge Agent (edge_agent)**  
  - Outbound-only gRPC to headend; receives setpoints, pushes telemetry every 4s, reconnects with backoff.  
  - No inbound listener; safer for internet-facing deployments.  
  - Config via env vars (`ASSET_ID`, `SITE_ID`, `SITE_NAME`, `ASSET_NAME`, `ASSET_LOCATION`, `CAPACITY_MWHR`, `MAX_MW`, `MIN_MW`, `EFFICIENCY`, `RAMP_RATE_MW_PER_MIN`, `HEADEND_GRPC`).

- **Launcher (agent_launcher)**  
  - Reads `assets.yaml` (override `ASSETS_PATH`) and spawns one `edge_agent` per asset with the correct env vars.  
  - Uses `HEADEND_GRPC` to point all agents at the headend gRPC endpoint.

- **Asset Config (assets.yaml)**  
  - Defines sites and assets (id/name/site_id, capacity, max/min MW, ramp rate, efficiency).  
  - Scales to large fleets (e.g., 100 assets); agents can be launched per asset via the launcher.

## Integration Notes (Central SCADA + PI)

- **Today’s simulator is standalone**: It does not yet integrate with existing Central SCADA or PI servers.  
- **Observed pain in legacy stacks** (central SCADA + PI connectors): inbound connectivity to sites, brittle onboarding, stale/missing PI points (connector/network hiccups, polling limits), and large fault domains when SCADA/PI is down.  
- **Path to integration** (future work):  
  - Expose a SCADA/PI-facing adapter (e.g., PI AF/SDK writer or CSV/OPC exports) fed by headend telemetry or an event bus.  
  - Keep agents outbound-only; do not require inbound SCADA paths.  
  - Treat SCADA/PI as a consumer of telemetry/dispatch intent, not the primary orchestrator.

## Scalability Considerations

- **Agent scaling**: Launcher can spawn one process per asset; for larger fleets, consider containerizing agents and running under an orchestrator (K8s/Nomad), or batching multiple assets per agent when appropriate.  
  - Keep gRPC outbound-only; add MTLS and retry/backoff for resilience.  
  - Use `HEADEND_GRPC_ADDR` to move the headend off default ports that may be occupied (e.g., 50051).

- **Headend scaling**:  
  - Currently single instance combining API + gRPC + DB writes. Next step is to separate concerns (API/gateway vs. control/dispatch vs. data ingest) and introduce an event bus for telemetry and setpoints.  
  - Persist agent state and sessions in Postgres; add health/metrics endpoints for observability.  
  - Disable the headend tick loop in agent-only mode to reduce DB writes and avoid duplicate telemetry.

- **Data fidelity**:  
  - Agents buffer/retry on gRPC reconnects; headend writes telemetry and agent sessions to Postgres.  
  - For better fidelity, add durable queues and backpressure handling between headend and DB, and consider timeseries-specific storage later (Timescale/OpenTSDB/etc.).

## MVP vs. Future Control Plane

- **MVP**: Single headend service + outbound agents + launcher + Postgres. Focused on basic dispatch, telemetry ingest, and agent session tracking.  
- **Future**: Service-oriented control plane (API/gateway, orchestration, data/event layer), outbound-only agents, event bus for telemetry/dispatch, auth/tenant boundaries, UI separated from headend, and SCADA/PI adapters as consumers—not controllers.  

## Crate-by-Crate Details

- **sim_core**  
  - Shared models: `Asset`, `BessState`, `Telemetry`, `Dispatch`, `DispatchRequest`.  
  - Tick logic: `tick_asset(asset, state, dt_secs)` applies ramp limits, updates SOC, and emits telemetry.  
  - No HTTP/DB deps; used by headend and agents.

- **der_headend**  
  - HTTP endpoints:  
    - `GET /assets` – list assets (from `assets.yaml`).  
      - Example: `curl http://127.0.0.1:3001/assets`  
    - `GET /telemetry/{id}` – latest telemetry (agent-pushed if present, else synthesized).  
      - Example: `curl http://127.0.0.1:3001/telemetry/<ASSET_ID>`  
    - `GET /telemetry/{id}/history` – last 100 rows from DB (empty array if no DB).  
      - Example: `curl http://127.0.0.1:3001/telemetry/<ASSET_ID>/history`  
    - `POST /dispatch` – set setpoint (JSON ack; logs asset/site, limits, connection state).  
      - Example: `curl -X POST http://127.0.0.1:3001/dispatch -H 'content-type: application/json' -d '{"asset_id":"<ASSET_ID>","mw":10.0}'`  
    - `GET /agents` – current connected agents (asset_id, asset_name, site_name, peer).  
      - Example: `curl http://127.0.0.1:3001/agents`  
  - gRPC: `AgentLink::Stream` (agent-initiated). Agents send Register + Telemetry; headend pushes Setpoint.  
  - Persistence (optional, Postgres): tables `assets`, `telemetry`, `agent_sessions` (asset_id, peer, asset/site names, connect/disconnect timestamps).  
  - Agent registry: in-memory map of active streams with peer + asset/site names; sessions persisted on connect/disconnect.  
  - Tick loop: can be disabled for agent-only telemetry to avoid duplicate data.  

- **edge_agent**  
  - Outbound-only gRPC to headend; reconnects with backoff.  
  - Receives setpoints and updates local `BessState`; ticks every 4s and sends telemetry.  
  - Env config: `ASSET_ID`, `SITE_ID`, `SITE_NAME`, `ASSET_NAME`, `ASSET_LOCATION`, `CAPACITY_MWHR`, `MAX_MW`, `MIN_MW`, `EFFICIENCY`, `RAMP_RATE_MW_PER_MIN`, `HEADEND_GRPC` (host:port or URL).

- **agent_launcher**  
  - Reads `assets.yaml` (override `ASSETS_PATH`) and spawns `cargo run -p edge_agent` per asset with proper env vars.  
  - Sets `HEADEND_GRPC` for all agents.

- **Other crates (stubs)**  
  - `der_control_plane`, `telemetry_event_layer`: not yet implemented; intended for future separation of control and data/event concerns.  
