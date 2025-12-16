# Roadmap — From MVP to V1 (customer‑trial ready)

This roadmap assumes the MVP already proves:
- outbound gRPC connectivity,
- **Edge Gateway Mode** (one agent per site, multi‑asset registration),
- per‑asset dispatch and a deterministic per‑site dispatch split (capacity‑weighted + clamped),
- `/agents` visibility (now includes `site_id` + `site_name`) and DB session de‑dup,
- dev ergonomics like `RESET_DB`.

## V1 goal (one sentence)
Enable a real customer pilot with secure agents, durable telemetry/dispatch semantics, and an operator UI — while keeping the outbound‑only deployment story.

## Delivery model (trajectory)

### Today: managed pilot service (cloud-hosted, typically single-tenant)
- Vendor-operated control plane with outbound-only edge connectivity.
- Single-tenant per customer is common initially (clear security boundaries, faster onboarding).
- Goal: prove reliability, dispatch semantics, and operator workflows with real assets.

### V1: pilot-ready service
- Security baseline (mTLS + API auth), durability improvements (bounded queues + optional store-and-forward), and a thin operator UI.
- Clear runbooks, monitoring, and an upgrade story.

### V2+: multi-tenant SaaS
- Tenant isolation, RBAC, audit trails, quotas/billing hooks, compliance-ready controls.
- Optional regional deployments and data residency as needed.

## Guiding principles
- **Don’t break the MVP demo loop.** Every epic should be deliverable incrementally.
- **Be honest about guarantees.** Document “best effort” vs “at‑least‑once” explicitly.
- **Keep the edge simple.** No inbound rules at customer sites; agent reconnects are expected.

## Epic 0 — MVP hardening (quality + testability)
**Deliverables**
- Gate‑checked test plan execution (Gateway Mode, per‑asset dispatch, per‑site split)
- Regression tests for **target precedence** (`asset_id` wins over `site_id`)
- Config sanity checks (required env vars, friendly errors)

**Definition of done**
- A new dev can run the demo end‑to‑end in <10 minutes using docs
- A CI run can execute smoke tests without manual steps

## Epic 1 — Security baseline
**Deliverables**
- mTLS on gRPC (agent↔headend)
- Device identity model (asset_id ↔ cert; gateway_id ↔ cert policy)
- Basic REST auth (JWT/OIDC later; static tokens acceptable for first pilot)

**Definition of done**
- An agent without a valid cert cannot connect
- All REST calls require auth and are logged with principal (+ tenant if enabled)

## Epic 2 — Durable ingest + backpressure
**Deliverables**
- Internal event layer between ingest and storage (queue/bus)
- Backpressure strategy (bounded queues + explicit drop/spill policy)
- Headend behavior under dependency outages (DB down, slow disk, high load)
- Clear retry semantics documented (telemetry + dispatch)
- Groundwork for **edge store-and-forward** (buffering policy + interfaces; implementation may be Epic 5/7 depending on pilot)

**Definition of done**
- Headend survives a DB outage without crashing or unbounded memory growth
- Guarantees are explicit and testable (e.g., “at‑least‑once ingest”, dispatch lifecycle/acks, bounded-memory behavior)

## Epic 3 — Dispatch lifecycle + safety rails
**Deliverables**
- Dispatch states: submitted → sent → acked → active → complete/expired/failed
- Agent ack on Setpoint apply (with reason codes)
- Safety checks (limits, duration handling consistency, optional ramp limiting)

**Definition of done**
- You can answer: “Was this setpoint applied? When? For how long? Why did it fail?”
- Per‑site dispatch has clear semantics (split policy versioned + logged)

## Epic 4 — Operator UI v1 (thin web console)
**Deliverables**
- Assets page (list + details)
- Agents page (connected state, `peer`, `site_id`, sessions, last‑seen)
- Telemetry page (latest + simple history window)
- Dispatch page (per‑asset + per‑site) with validation, guardrails, and split preview

**Definition of done**
- A pilot user can operate the system without curl
- UI mirrors routing semantics (warn if both `asset_id` and `site_id` are provided; default to per‑asset)

## Epic 5 — Telemetry model v1 (type‑aware, extensible)
**Deliverables**
- Add `asset_type` and a small **common core** telemetry schema
- Add a `details`/extension mechanism for type‑specific fields
- Define starter field sets for: Storage/BESS, PV, Wind, EV charging
- Define a **telemetry adapter interface** (edge side) so multiple data sources can feed the same normalized telemetry model
- Build 1–2 **pilot-grade protocol adapters** (pick based on target customer):
  - **OPC UA** (common in industrial plants)
  - Optional (pilot-fit): expose an **OPC UA server surface** from the `edge_agent` for SCADA/HMI interoperability (e.g., Ignition as OPC UA client: read telemetry + write setpoints)
  - **Modbus TCP** (common for inverters/BESS controllers)
  - Optional alternates for pilot fit: **MQTT** (IoT/pubsub) or **DNP3** (utility/OT)
- Add an adapter configuration model (YAML) describing:
  - endpoint (host/port, security settings)
  - tag/point mapping → normalized fields
  - polling/subscribe interval + timeouts

**Definition of done**
- Headend tolerates missing optional fields (backward compatible)
- API returns a normalized view (core + details) suitable for fleet dashboards
- At least one real protocol adapter can publish normalized telemetry end-to-end (device → edge_agent → headend)
- If the pilot requires it, an OPC UA server surface can mirror normalized telemetry and accept setpoint writes at the site (SCADA-friendly)
- Adapter mappings are testable and versioned (sample configs + smoke tests)

## Epic 6 — Observability & ops
**Deliverables**
- Metrics endpoint + dashboards (telemetry rate, lag, connected assets, dispatch success)
- Health endpoints (liveness/readiness)
- Runbook: deploy, upgrade, common failures (“no data”, “agent flapping”, “dispatch not applied”)

**Definition of done**
- An on‑call engineer can diagnose “no data” in <10 minutes without reading code

## Epic 7 — Packaging for pilots
**Deliverables**
- Docker images for headend/agent
- Minimal K8s manifests (or compose) + example configs
- “Pilot guide” doc: install, onboard 1 site, verify telemetry, issue dispatch

**Definition of done**
- A customer can stand up a pilot using only docs + provided manifests

## What we will NOT do in V1 (guardrails)
- Full multi‑region HA claims
- Full rules engine / optimization engine
- Complex market/SCADA protocol termination in the cloud
- VPP/portfolio optimization beyond foundational group/site control primitives
- "All protocols" claims: V1 targets 1–2 adapters for pilots; broad protocol coverage is later
- “Fully multi-tenant SaaS today” claims: V1 focuses on pilot-ready reliability/security; multi-tenancy is Epic 8 (V2 path)

## Epic 8 — SaaS foundations (V2 path, not required for first pilots)
**Deliverables**
- Tenant model (identity + data isolation boundaries)
- RBAC roles and audit trails (who did what, when)
- Quotas/rate limits and billing hooks
- Compliance posture starter kit (logging, retention controls, evidence exports)

**Definition of done**
- A single headend deployment can safely serve multiple tenants without data bleed
- Operator actions are attributable and exportable for audits