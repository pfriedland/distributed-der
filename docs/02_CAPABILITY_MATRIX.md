# Capability Matrix (Now vs Next vs Later)

| Capability | MVP (Now) | V1 (Next) | Later |
|---|---|---|---|
| Asset identity & config | YAML-driven stable UUIDs (`ASSETS_PATH`) | API-driven onboarding + validation | Full CMDB / inventory integration |
| Agent connectivity | Outbound gRPC stream w/ reconnect; **Gateway Mode** (one agent per site) supported | mTLS + cert rotation + device identity | Multi-region routing / brokers |
| Telemetry ingest | gRPC telemetry; per-asset mux over one stream (Gateway Mode) | Durable queue + backpressure | Exactly-once / replay-safe ingest |
| Telemetry storage | Optional Postgres tables (history/sessions); runs without DB | Timeseries strategy + retention policies | Tiered storage + lakehouse export |
| Telemetry query | Latest per-asset (`GET /telemetry/:asset_id`); basic history when DB enabled | Rich query (range, downsample) | Analytics/feature store integrations |
| Dispatch | REST `/dispatch` → gRPC `Setpoint`; per-asset + per-site (`site_id`) | Acks, states, expiry, safety rules | Closed-loop control workflows |
| Site dispatch split | **Capacity-weighted split** by `capacity_mwhr` with clamp + logs | Pluggable split policies (priority, constraints, availability) | Portfolio / VPP optimization |
| Target resolution | **Precedence rule:** `asset_id` wins over `site_id` (prevents accidental fanout) | Explicit addressing schema + validation errors | Policy engine + approval flows |
| Offline handling | Best-effort reconnect; no durable command queue | Durable command queue + TTL | Multi-level fallback strategies |
| Agent health | Heartbeats (and sessions) visible; DB-backed when enabled | SLOs, alerting, dashboards | Predictive health / anomaly detection |
| Observability | Tracing/logs; `/agents` shows per-asset connection + recent sessions; includes `site_id` + `site_name` | Metrics + traces + structured events | Full ops suite + audit trails |
| User interface | No UI (API-first); demo via curl/CLI; JSON is source of truth | Web UI v1: assets/agents dashboard, per-asset telemetry view, dispatch form (asset + site) w/ validation | Operator console: fleet views, filtering, alarms, audit trail, role-based views, workflows |
| Multi-tenancy | Not supported | Tenants/projects + RBAC | Cross-tenant governance/segmentation |
| AuthN/Z | None | mTLS + JWT/OIDC + RBAC | Fine-grained policy + approval flows |
| Deployment | Local run; Postgres optional; `RESET_DB` dev reset | Containerize + K8s manifests | HA, autoscaling, multi-region |
| Compliance story | “Reduce scope” narrative only | Concrete controls + evidence artifacts | Formal mapping + customer templates |
| Extensibility | Sim agent + shared core; Gateway Mode test harness | Plugin/adapter model for real DERs | Marketplace/adapters ecosystem |

## UI evolution (operator experience)

### MVP (Now): API-first demo surface
- Primary interface is REST + gRPC; the UI is effectively `curl` + logs.
- Goal: prove routing, gateway behavior, and deterministic split policies.

### V1 (Next): thin web console (fast to build)
- Pages:
  - **Assets**: list + details (limits, type, site)
  - **Agents**: connection state, peer, last-seen, sessions
  - **Telemetry**: latest view + simple history chart (time window)
  - **Dispatch**: setpoint form for `asset_id` or `site_id` with guardrails/validation
- Non-goals: complex workflows; keep it as an operator cockpit.

#### V1 UI acceptance criteria (minimum)
- **Assets page** loads in <2s locally and lists all configured assets with `site_name` and limits (`min_mw`/`max_mw`).
- **Agents page** shows one row per configured asset and includes `site_id`, `site_name`, `peer`, `connected`, and “last seen”.
- **Telemetry page** shows latest values for a selected asset and a simple history view (time window: 15m / 1h / 24h).
- **Dispatch page** supports:
  - per-asset dispatch (`asset_id`) and per-site dispatch (`site_id`),
  - input validation (MW is numeric; required target),
  - guardrails: if both `asset_id` and `site_id` are provided, the UI warns and defaults to per-asset (mirrors agent precedence),
  - a split preview for site dispatch (shows the 3:1:1 style allocations and clamping) before submit.
- All actions show clear success/error results (HTTP status + message) and provide a “copy curl” button for reproducibility.

### Later: production operator console
- Fleet segmentation (regions, portfolios), alerting, audit, RBAC, and workflow-driven operations.
- Integrations for tickets/notifications and customer-specific views.

Notes:
- Anything not in “MVP (Now)” should not be marketed as “shipping today.”
- MVP (Now) **does include** Gateway Mode and a **demo-ready** per-site dispatch split (capacity-weighted by `capacity_mwhr`, clamped to per-asset limits). Today this applies to `site_id` targeting and simulated assets; `group_id`/VPP semantics and real device adapters are V1+.
- V1 should focus on the smallest set of features that enable *real customer trials*.