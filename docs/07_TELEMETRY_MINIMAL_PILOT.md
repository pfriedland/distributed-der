## Purpose

This is intentionally not a “collect every tag” spec.

## Related documents
- `README.md` — project entry point and doc index
- `docs/01_ARCHITECTURE_CURRENT.md` — current system architecture and flows
- `docs/03_ROADMAP_V1.md` — V1 roadmap (durability, security, UI, adapters)
- `docs/04_RFC_EDGE_GATEWAY_MODE.md` — gateway mode semantics and routing rules
- `docs/05_TEST_PLAN_GATEWAY_MODE.md` — gateway mode test plan
- `docs/06_CUTSHEET.md` — product positioning and narrative

# Minimal Pilot Telemetry Spec (Wind + PV) — V1-friendly

## Purpose
Define a **small, high-value telemetry + events surface** for pilots (e.g., 20 turbines + 20 inverters) that:
- supports realistic demos (simulated assets),
- supports real site integration via **OPC UA at the edge**, and
- scales by using **store-by-exception** for most signals.

This is intentionally not a “collect every tag” spec.

## Related documents
- `README.md` — project entry point and doc index
- `docs/01_ARCHITECTURE_CURRENT.md` — current system architecture and flows
- `docs/03_ROADMAP_V1.md` — V1 roadmap (durability, security, UI, adapters)
- `docs/04_RFC_EDGE_GATEWAY_MODE.md` — gateway mode semantics and routing rules
- `docs/05_TEST_PLAN_GATEWAY_MODE.md` — gateway mode test plan
- `docs/06_CUTSHEET.md` — product positioning and narrative

## Scope
- Asset types: **Wind Turbine**, **PV Inverter**, and **Site Aggregate**.
- Transport: normalized telemetry from `edge_agent` → headend over gRPC.
- Edge integration: OPC UA client subscription (site SCADA/PLC/RTU as the server).
- Optional: `edge_agent` OPC UA server surface for local SCADA/HMI (Ignition) interoperability.

## Design principles
- **Few fast signals.** Only what operators need at 1–5s cadence.
- **Most signals by exception.** Use deadbands, state-change triggers, and event windows.
- **Stable identity.** Internally identify signals with stable keys even if OPC UA node paths change.
- **Type-aware but compatible.** A small common core + type-specific details.

## Common core schema (all assets)

### Required fields
- `timestamp` (UTC)
- `asset_id` (UUID)
- `site_id` (UUID)
- `asset_type` (enum: `wind_turbine`, `pv_inverter`, `site`)
- `online` (bool)
- `status` (string/enum)

### Recommended fields
- `mw` (f64, signed; generation positive)
- `mw_setpoint` (f64, optional)
- `quality` (string/enum; derived from OPC UA StatusCode or simulator)

### Type-specific fields
All other fields are carried in a type-specific `details` payload (or a flattened representation in storage), keyed by a normalized signal name (see below).

## Minimal signal sets

### Wind turbine (per turbine)

**Fast (sample)**
- `mw`
- `grid_frequency_hz` (optional)

**Normal (sample or exception with heartbeat)**
- `wind_speed_ms`
- `rotor_speed_rpm`
- `pitch_deg` (aggregate)
- `grid_voltage_v` (one representative measurement)
- `inverter_or_converter_temp_c` (optional)

**Exception (state-change / deadband / event-driven)**
- `status`
- `online`
- `fault_code` (string/int)
- `curtailment_active` (bool)
- `curtailment_reason` (string)

### PV inverter (per inverter)

**Fast (sample)**
- `mw`

**Normal (sample or exception with heartbeat)**
- `dc_voltage_v`
- `dc_current_a`
- `ac_voltage_v`
- `ac_current_a`
- `inverter_temp_c`

**Exception (state-change / deadband / event-driven)**
- `status`
- `online`
- `fault_code`
- `curtailment_active`
- `curtailment_reason`

### Site aggregate (one per site)

**Fast (sample; often the only truly “fast” stream)**
- `site_mw` (sum or POI meter)

**Normal (sample)**
- `site_online_assets` (count)
- `site_alarm_count` (count)

**Optional**
- `site_mvar`
- `poi_mw` (if distinct from `site_mw`)

## Sampling & exception policy

### Classes
- **fast:** 1–5 seconds (only a handful of signals)
- **normal:** 4–8 seconds (or slower if acceptable)
- **exception:** publish only when a value changes beyond deadband or state changes
- **event:** publish on alarms/trips with optional “window capture”

### Heartbeat
For exception-driven signals, publish a **heartbeat sample** every `heartbeat_s` (suggested 120–300s) so trends remain interpretable.

### Deadbands (recommended defaults)
- Power (`mw`): absolute deadband 0.02–0.10 MW (pilot dependent)
- Voltage/current: percent deadband 0.5–1.0%
- Temperature: absolute deadband 0.5–1.0 °C
- Speeds: percent deadband ~1%

### Event windows (fault snapshots)
When a fault/trip event fires:
- emit an event record (fault code, severity, message), and
- optionally capture a short pre/post window for key signals (e.g., `mw`, temps, speeds) for 30–120 seconds.

## Normalized signal naming
Use predictable lower_snake_case names with units in the catalog metadata (not in the name). Examples:
- `mw`, `grid_frequency_hz`, `dc_voltage_v`, `inverter_temp_c`, `wind_speed_ms`

Avoid embedding vendor tag names in the normalized signal key.

## OPC UA mapping model (edge)
OPC UA is node-based; the platform is signal-based. The edge maintains a **signal catalog** mapping UA nodes to normalized signals.

### Minimum catalog fields
- `signal_id` (stable internal id)
- `asset_id`
- `signal` (normalized key, e.g., `dc_voltage_v`)
- `ua_node_id` (string)
- `data_type` (f64/i64/bool/string)
- `mode` (fast/normal/exception/event)
- `deadband` (absolute or percent)
- `publish_interval_ms` (for sampled signals)
- `heartbeat_s` (for exception signals)

### Example mapping YAML (illustrative)
```yaml
site:
  site_id: "6ba7b810-9dad-11d1-80b4-00c04fd430c8"
  name: "Pilot-Site"

opcuaclient:
  endpoint: "opc.tcp://192.168.1.50:4840"
  security_policy: "None"   # pilot only

defaults:
  publish_interval_ms: 4000
  heartbeat_s: 180

assets:
  - asset_id: "..."
    asset_type: "wind_turbine"
    name: "WTG-01"
    points:
      - signal: "mw"
        node_id: "ns=2;s=WTG01.Measurements.RealPower"
        data_type: "f64"
        mode: "normal"
        deadband: { kind: "absolute", value: 0.05 }
      - signal: "status"
        node_id: "ns=2;s=WTG01.Status.State"
        data_type: "string"
        mode: "exception"

  - asset_id: "..."
    asset_type: "pv_inverter"
    name: "INV-01"
    points:
      - signal: "mw"
        node_id: "ns=2;s=INV01.Measurements.ACRealPower"
        data_type: "f64"
        mode: "normal"
        deadband: { kind: "absolute", value: 0.02 }
      - signal: "inverter_temp_c"
        node_id: "ns=2;s=INV01.Measurements.TempC"
        data_type: "f64"
        mode: "exception"
        deadband: { kind: "absolute", value: 0.5 }
```

## Simulator requirements (demo)
The simulator should follow the same contract as real adapters:
- produce telemetry with the common core fields,
- emit realistic ramps toward setpoints (`mw_setpoint` → `mw`), and
- generate occasional events (fault/trip + recovery) with optional fault windows.

### Suggested demo scenarios
- **Wind lull + recovery:** wind speed drops, MW declines, then recovers.
- **Inverter trip:** fault event triggers MW → 0, then recovers after a cooldown.
- **Curtailment period:** curtailment active with a site-wide dispatch target.

## Storage guidance (time-series)
This spec is compatible with a point-based time-series model:
- **catalog** table: signal metadata (small)
- **samples** table: `(ts, asset_id, signal_id, value_*, quality)` (large)

For dashboards, retain:
- raw exceptions for a short “hot” window (e.g., 7–30 days), and
- rollups (1m/5m) for long-term trending.

## Open questions (pilot-specific)

### Fast signals (1–5s) for this pilot
- **Voltage** and **frequency** are fast signals for the pilot (in addition to `site_mw`).
  - Recommend: report `grid_voltage_v` and `grid_frequency_hz` at 1–5s **at the site level** (POI/substation) and optionally at the asset level if the plant exposes it cheaply.

### Deadbands must be configurable
- Deadbands must be definable per signal in the mapping config.
  - Support both **absolute** and **percent** deadbands.
  - Allow a per-signal **min_publish_interval_ms** (rate-limit) and **heartbeat_s** (keep charts readable).

### Site SCADA integration is read/write
- The pilot assumes local SCADA/HMI will both:
  - **read** telemetry from the edge, and
  - **write** setpoints (R/W).
- Guardrails (recommended for V1):
  - clamp setpoints to per-asset limits;
  - require a `duration_s` for writes where feasible;
  - preserve the precedence rule: if a command includes both `asset_id` and `site_id`, treat it as **per-asset**.

### Events and alarming are required (TSDB must be event-friendly)
- The pilot requires support for event streams that can drive alarming (not just numeric samples).
  - Minimum event types: **alarms/conditions**, **trips**, **curtailment state changes**, and **communications/online state changes**.
  - Severity should map into a small consistent scale (e.g., `info`, `warning`, `alarm`, `critical`) plus a vendor-specific code/message.
- Storage guidance (event-friendly TSDB):
  - Store events in a dedicated **events** table (or equivalent) with: `(ts, asset_id, site_id, event_type, severity, code, message, details_json)`.
  - Keep numeric telemetry in the samples table; relate events to telemetry windows by time.
# Minimal Pilot Telemetry Spec (Wind + PV) — V1-friendly

## Purpose
Define a **small, high-value telemetry + events surface** for pilots (e.g., 20 turbines + 20 inverters) that:
- supports realistic demos (simulated assets),
- supports real site integration via **OPC UA at the edge**, and
- scales by using **store-by-exception** for most signals.

This is intentionally not a “collect every tag” spec.

## Related documents
- `README.md` — project entry point and doc index
- `docs/01_ARCHITECTURE_CURRENT.md` — current system architecture and flows
- `docs/03_ROADMAP_V1.md` — V1 roadmap (durability, security, UI, adapters)
- `docs/04_RFC_EDGE_GATEWAY_MODE.md` — gateway mode semantics and routing rules
- `docs/05_TEST_PLAN_GATEWAY_MODE.md` — gateway mode test plan
- `docs/06_CUTSHEET.md` — product positioning and narrative

## Scope
- Asset types: **Wind Turbine**, **PV Inverter**, and **Site Aggregate**.
- Transport: normalized telemetry from `edge_agent` → headend over gRPC.
- Edge integration: OPC UA client subscription (site SCADA/PLC/RTU as the server).
- Optional: `edge_agent` OPC UA server surface for local SCADA/HMI (Ignition) interoperability.

## Design principles
- **Few fast signals.** Only what operators need at 1–5s cadence.
- **Most signals by exception.** Use deadbands, state-change triggers, and event windows.
- **Stable identity.** Internally identify signals with stable keys even if OPC UA node paths change.
- **Type-aware but compatible.** A small common core + type-specific details.

## Common core schema (all assets)

### Required fields
- `timestamp` (UTC)
- `asset_id` (UUID)
- `site_id` (UUID)
- `asset_type` (enum: `wind_turbine`, `pv_inverter`, `site`)
- `online` (bool)
- `status` (string/enum)

### Recommended fields
- `mw` (f64, signed; generation positive)
- `mw_setpoint` (f64, optional)
- `quality` (string/enum; derived from OPC UA StatusCode or simulator)

### Type-specific fields
All other fields are carried in a type-specific `details` payload (or a flattened representation in storage), keyed by a normalized signal name (see below).

## Minimal signal sets

### Wind turbine (per turbine)

**Fast (sample)**
- `mw`
- `grid_frequency_hz` (optional)

**Normal (sample or exception with heartbeat)**
- `wind_speed_ms`
- `rotor_speed_rpm`
- `pitch_deg` (aggregate)
- `grid_voltage_v` (one representative measurement)
- `inverter_or_converter_temp_c` (optional)

**Exception (state-change / deadband / event-driven)**
- `status`
- `online`
- `fault_code` (string/int)
- `curtailment_active` (bool)
- `curtailment_reason` (string)

### PV inverter (per inverter)

**Fast (sample)**
- `mw`

**Normal (sample or exception with heartbeat)**
- `dc_voltage_v`
- `dc_current_a`
- `ac_voltage_v`
- `ac_current_a`
- `inverter_temp_c`

**Exception (state-change / deadband / event-driven)**
- `status`
- `online`
- `fault_code`
- `curtailment_active`
- `curtailment_reason`

### Site aggregate (one per site)

**Fast (sample; often the only truly “fast” stream)**
- `site_mw` (sum or POI meter)

**Normal (sample)**
- `site_online_assets` (count)
- `site_alarm_count` (count)

**Optional**
- `site_mvar`
- `poi_mw` (if distinct from `site_mw`)

## Sampling & exception policy

### Classes
- **fast:** 1–5 seconds (only a handful of signals)
- **normal:** 4–8 seconds (or slower if acceptable)
- **exception:** publish only when a value changes beyond deadband or state changes
- **event:** publish on alarms/trips with optional “window capture”

### Heartbeat
For exception-driven signals, publish a **heartbeat sample** every `heartbeat_s` (suggested 120–300s) so trends remain interpretable.

### Deadbands (recommended defaults)
- Power (`mw`): absolute deadband 0.02–0.10 MW (pilot dependent)
- Voltage/current: percent deadband 0.5–1.0%
- Temperature: absolute deadband 0.5–1.0 °C
- Speeds: percent deadband ~1%

### Event windows (fault snapshots)
When a fault/trip event fires:
- emit an event record (fault code, severity, message), and
- optionally capture a short pre/post window for key signals (e.g., `mw`, temps, speeds) for 30–120 seconds.

## Normalized signal naming
Use predictable lower_snake_case names with units in the catalog metadata (not in the name). Examples:
- `mw`, `grid_frequency_hz`, `dc_voltage_v`, `inverter_temp_c`, `wind_speed_ms`

Avoid embedding vendor tag names in the normalized signal key.

## OPC UA mapping model (edge)
OPC UA is node-based; the platform is signal-based. The edge maintains a **signal catalog** mapping UA nodes to normalized signals.

### Minimum catalog fields
- `signal_id` (stable internal id)
- `asset_id`
- `signal` (normalized key, e.g., `dc_voltage_v`)
- `ua_node_id` (string)
- `data_type` (f64/i64/bool/string)
- `mode` (fast/normal/exception/event)
- `deadband` (absolute or percent)
- `publish_interval_ms` (for sampled signals)
- `heartbeat_s` (for exception signals)

### Example mapping YAML (illustrative)
```yaml
site:
  site_id: "6ba7b810-9dad-11d1-80b4-00c04fd430c8"
  name: "Pilot-Site"

opcuaclient:
  endpoint: "opc.tcp://192.168.1.50:4840"
  security_policy: "None"   # pilot only

defaults:
  publish_interval_ms: 4000
  heartbeat_s: 180

assets:
  - asset_id: "..."
    asset_type: "wind_turbine"
    name: "WTG-01"
    points:
      - signal: "mw"
        node_id: "ns=2;s=WTG01.Measurements.RealPower"
        data_type: "f64"
        mode: "normal"
        deadband: { kind: "absolute", value: 0.05 }
      - signal: "status"
        node_id: "ns=2;s=WTG01.Status.State"
        data_type: "string"
        mode: "exception"

  - asset_id: "..."
    asset_type: "pv_inverter"
    name: "INV-01"
    points:
      - signal: "mw"
        node_id: "ns=2;s=INV01.Measurements.ACRealPower"
        data_type: "f64"
        mode: "normal"
        deadband: { kind: "absolute", value: 0.02 }
      - signal: "inverter_temp_c"
        node_id: "ns=2;s=INV01.Measurements.TempC"
        data_type: "f64"
        mode: "exception"
        deadband: { kind: "absolute", value: 0.5 }
```

## Simulator requirements (demo)
The simulator should follow the same contract as real adapters:
- produce telemetry with the common core fields,
- emit realistic ramps toward setpoints (`mw_setpoint` → `mw`), and
- generate occasional events (fault/trip + recovery) with optional fault windows.

### Suggested demo scenarios
- **Wind lull + recovery:** wind speed drops, MW declines, then recovers.
- **Inverter trip:** fault event triggers MW → 0, then recovers after a cooldown.
- **Curtailment period:** curtailment active with a site-wide dispatch target.

## Storage guidance (time-series)
This spec is compatible with a point-based time-series model:
- **catalog** table: signal metadata (small)
- **samples** table: `(ts, asset_id, signal_id, value_*, quality)` (large)

For dashboards, retain:
- raw exceptions for a short “hot” window (e.g., 7–30 days), and
- rollups (1m/5m) for long-term trending.

## Open questions (pilot-specific)

### Fast signals (1–5s) for this pilot
- **Voltage** and **frequency** are fast signals for the pilot (in addition to `site_mw`).
  - Recommend: report `grid_voltage_v` and `grid_frequency_hz` at 1–5s **at the site level** (POI/substation) and optionally at the asset level if the plant exposes it cheaply.

### Deadbands must be configurable
- Deadbands must be definable per signal in the mapping config.
  - Support both **absolute** and **percent** deadbands.
  - Allow a per-signal **min_publish_interval_ms** (rate-limit) and **heartbeat_s** (keep charts readable).

### Site SCADA integration is read/write
- The pilot assumes local SCADA/HMI will both:
  - **read** telemetry from the edge, and
  - **write** setpoints (R/W).
- Guardrails (recommended for V1):
  - clamp setpoints to per-asset limits;
  - require a `duration_s` for writes where feasible;
  - preserve the precedence rule: if a command includes both `asset_id` and `site_id`, treat it as **per-asset**.

### Events and alarming are required (TSDB must be event-friendly)
- The pilot requires support for event streams that can drive alarming (not just numeric samples).
  - Minimum event types: **alarms/conditions**, **trips**, **curtailment state changes**, and **communications/online state changes**.
  - Severity should map into a small consistent scale (e.g., `info`, `warning`, `alarm`, `critical`) plus a vendor-specific code/message.
- Storage guidance (event-friendly TSDB):
  - Store events in a dedicated **events** table (or equivalent) with: `(ts, asset_id, site_id, event_type, severity, code, message, details_json)`.
  - Keep numeric telemetry in the samples table; relate events to telemetry windows by time.