
# integrated_system/docs/02_CAPABILITY_MATRIX.md

# Capability Matrix (Now vs Next vs Later)

| Capability | MVP (Now) | V1 (Next) | Later |
|---|---|---|---|
| Asset identity & config | YAML-driven stable UUIDs | API-driven onboarding + validation | Full CMDB / inventory integration |
| Agent connectivity | Outbound gRPC stream w/ reconnect | mTLS + cert rotation + device identity | Multi-region routing / brokers |
| Telemetry ingest | gRPC telemetry + in-memory latest | Durable queue + backpressure | Exactly-once / replay-safe ingest |
| Telemetry storage | Optional Postgres tables | Timeseries strategy + retention policies | Tiered storage + lakehouse export |
| Telemetry query | Latest + basic history | Rich query (range, downsample) | Analytics/feature store integrations |
| Dispatch | REST `/dispatch` → Setpoint push | Acks, states, expiry, safety rules | Closed-loop control workflows |
| Offline handling | Pending setpoint delivered on reconnect | Durable command queue + TTL | Multi-level fallback strategies |
| Agent health | Heartbeats persisted (optional) | SLOs, alerting, dashboards | Predictive health / anomaly detection |
| Observability | Tracing/logs (basic) | Metrics + traces + structured events | Full ops suite + audit trails |
| Multi-tenancy | Not supported | Tenants/projects + RBAC | Cross-tenant governance/segmentation |
| AuthN/Z | None | mTLS + JWT/OIDC + RBAC | Fine-grained policy + approval flows |
| Deployment | Local run + Postgres optional | Containerize + K8s manifests | HA, autoscaling, multi-region |
| Compliance story | “Reduce scope” narrative only | Concrete controls + evidence artifacts | Formal mapping + customer templates |
| Extensibility | Sim agent + shared core | Plugin/adapter model for real DERs | Marketplace/adapters ecosystem |

Notes:
- Anything not in “MVP (Now)” should not be marketed as “shipping today.”
- V1 should focus on the smallest set of features that enable *real customer trials*.