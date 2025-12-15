# integrated_system/docs/03_ROADMAP_V1.md

# Roadmap — From MVP to V1 (customer-trial ready)

## V1 goal (one sentence)
Enable a real customer pilot with secure agents, durable ingestion, and operational visibility — while keeping the outbound-only deployment story.

## Epic 1 — Security baseline
**Deliverables**
- mTLS on gRPC (agent↔headend)
- Device identity model (asset_id ↔ cert)
- Basic RBAC for REST (JWT/OIDC or static tokens to start)

**Definition of done**
- An agent without valid cert cannot connect
- All REST calls require auth and are logged with principal/tenant

## Epic 2 — Durable messaging + backpressure
**Deliverables**
- Introduce an internal event layer (queue/bus) between ingest and storage
- Backpressure strategy (bounded queues, drop policy, or spill-to-disk)
- Retry semantics documented (telemetry, dispatch)

**Definition of done**
- Headend can survive a DB outage without losing process stability
- Clear guarantees documented (“at least once”, “best effort”, etc.)

## Epic 3 — Dispatch lifecycle + safety rails
**Deliverables**
- Dispatch states: submitted → sent → acked → active → complete/expired/failed
- Agent ack on Setpoint apply
- Basic safety checks (limits, duration handling consistency)

**Definition of done**
- You can answer: “Was this setpoint applied? When? For how long? Why did it fail?”

## Epic 4 — Observability & ops
**Deliverables**
- Metrics endpoint + dashboards (telemetry rate, lag, connected agents, dispatch success)
- Health endpoints (liveness/readiness)
- Runbook: deploy, upgrade, common failures

**Definition of done**
- An on-call engineer can diagnose “no data” in <10 minutes without reading code

## Epic 5 — Packaging for pilots
**Deliverables**
- Docker images for headend/agent
- Minimal K8s manifests (or compose) + example configs
- “Pilot guide” doc: install, onboard 1 DER, verify telemetry, issue dispatch

**Definition of done**
- A customer can stand up a pilot using only docs + provided manifests

## What we will NOT do in V1 (guardrail)
- Full multi-region HA claims
- Full-blown rules engine
- Complex market/SCADA protocol termination in cloud