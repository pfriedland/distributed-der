# BESS Simulator (multi-crate workspace)

This workspace hosts a headend + future agents for grid-tied battery assets (BESS) with registration, telemetry, and dispatch flows.

- `der_headend`: loads `assets.yaml`, runs the tick loop using `sim_core`, exposes REST (`/assets`, `/telemetry/{id}`, `/telemetry/{id}/history`, `/dispatch`), and optionally persists telemetry to Postgres.
- `sim_core`: shared models and tick logic (no HTTP/SQL deps).
- `edge_agent`, `der_control_plane`, `telemetry_event_layer`: currently stubs; will be filled in next.

See `DESIGN.md` and `DECOMPOSE_PLAN.md` for the design and split-plan notes.

## Quick start (headend)

```bash
cd der_headend
DATABASE_URL=postgres://bess:besspassword@localhost:5432/bess cargo run
```

If you omit `DATABASE_URL`, the headend runs in-memory only and still serves the REST API. The tick loop runs every 4s and the `/dispatch` endpoint accepts `{asset_id, mw, duration_s?}` (sign = charge/discharge).
