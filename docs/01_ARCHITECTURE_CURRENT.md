# integrated_system/docs/01_ARCHITECTURE_CURRENT.md

# Current Architecture (as implemented)

## Components
- **der_headend**: central service
  - REST API (default `127.0.0.1:3001`)
  - gRPC server (default `127.0.0.1:50051`, override with `HEADEND_GRPC_ADDR`)
  - Optional Postgres persistence (`DATABASE_URL`)
- **edge_agent**: per-asset outbound agent
  - Initiates gRPC stream to headend (`HEADEND_GRPC`)
  - Sends Register/Telemetry/Heartbeat; receives Setpoint
- **agent_launcher**: convenience runner
  - Reads `assets.yaml` and spawns one `edge_agent` per asset (MVP scaling helper)
- **sim_core**: shared types + tick logic (used by agent)

## Data flows
- Telemetry: `edge_agent` → (gRPC stream) → `der_headend` → (optional DB + in-memory latest)
- Dispatch: client → (REST `/dispatch`) → `der_headend` → (gRPC Setpoint) → `edge_agent`
- Presence: `edge_agent` → Heartbeat → `der_headend` → (optional DB)

## Mermaid diagram
```mermaid
flowchart LR
  subgraph Field["DER Site / Edge"]
    A1[edge_agent]
  end

  subgraph CloudOrDC["Headend / Control Plane"]
    H1[der_headend<br/>REST :3001<br/>gRPC :50051]
    DB[(Postgres optional)]
  end

  A1 -->|gRPC stream: Register/Telemetry/Heartbeat| H1
  H1 -->|gRPC stream: Setpoint| A1
  H1 -->|persist telemetry/dispatch/sessions/heartbeats| DB

  UI[Ops UI / Integrations] -->|REST /assets /telemetry /dispatch /heartbeat| H1