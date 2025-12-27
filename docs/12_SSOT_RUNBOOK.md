# SSOT Runbook

## Overview
This runbook describes the happy‑path workflow for updating the SSOT, regenerating outputs, and verifying the system.

## Prerequisites
- Python 3 available on your PATH
- `duckdb` Python package (optional; required only for `ssot.duckdb` and report generation)

Install DuckDB if needed:
```
pip install duckdb
```

## SSOT Field Reference

### `sites[]`
| Field | Description |
| --- | --- |
| `id` | Stable UUID for the site. Used to join assets and maps. |
| `name` | Human‑readable site name (used for reporting and map filenames). |
| `location` | Free‑form location string (display only). |
| `opcua` | Optional OPC UA mapping settings for this site. If omitted, no `opcua_map_<site>.yaml` is generated. |

### `sites[].opcua`
| Field | Description |
| --- | --- |
| `map_name` | Optional override for the map filename (`opcua_map_<map_name>.yaml`). If omitted, uses `site.name`. |
| `endpoint` | OPC UA endpoint URL for this site (used by edge agents). |
| `tag_root` | Ignition tag folder prefix (e.g., `Assets`). Empty means tags are directly under the provider root. |
| `default_setpoint_provider` | Tag provider for the default setpoint node (typically `default` or `edge`). |
| `setpoint_provider` | Tag provider for per‑asset setpoint nodes. |
| `telemetry_provider` | Tag provider for telemetry nodes. |
| `telemetry_interval_s` | Minimum seconds between telemetry reads/writes per asset. |
| `telemetry_write_sim` | When `true`, simulated telemetry is written into OPC UA tags; when `false`, telemetry is read from OPC UA. |

### `assets[]`
| Field | Description |
| --- | --- |
| `id` | Stable UUID for the asset. Used across headend, edge agent, and OPC UA maps. |
| `name` | Asset name used for Ignition tag paths and UI labels. |
| `site_id` | Site UUID that owns this asset (must match a `sites[].id`). |
| `capacity_mwhr` | Total energy capacity in MWh. |
| `max_mw` | Maximum discharge power (positive MW). |
| `min_mw` | Minimum charge power (negative MW). |
| `min_soc_pct` | Minimum allowable SOC (percent). |
| `max_soc_pct` | Maximum allowable SOC (percent). |
| `efficiency` | Round‑trip efficiency (0‑1). |
| `ramp_rate_mw_per_min` | Max MW change per minute (rate limit). |

## Update + Generate + Verify

### 0) Back up current files
Before generating or promoting anything, make a copy of the files you will replace:
- `ignition_tags.json`
- `der_headend/assets_test.yaml`
- `der_headend/opcua_map_<site>.yaml`

### 1) Update the SSOT
Edit the SSOT file:
- `ignition_ssot.json`

Make your changes to sites, assets, and per‑site OPC UA configuration.

### 2) Generate outputs (safe, staged)
Generate all artifacts into a staging directory so you can review them before copying into the repo or deploying. This keeps production files untouched until you explicitly promote the staged outputs.

Recommended pattern:
- Use a unique staging folder per change (timestamp or ticket id).
- Inspect files directly in the staging folder before promotion.

Example:
```
python3 tools/generate_ignition_files.py --output-dir /tmp/ignition_preview
```

Outputs:
- `/tmp/ignition_preview/ignition_tags.json`
- `/tmp/ignition_preview/der_headend/assets_test.yaml`
- `/tmp/ignition_preview/der_headend/opcua_map_<site>.yaml`
- `/tmp/ignition_preview/ssot.duckdb`

### 3) Generate the report
The report is a human‑readable snapshot of the SSOT and generated mappings. It is useful for catching mismatches (site/asset counts, tag roots, or OPC UA node paths) before deployment.
```
python3 tools/render_ssot_report.py \
  --db /tmp/ignition_preview/ssot.duckdb \
  --out /tmp/ignition_preview/ssot_report.md
```
Open the report in your editor and review:
- `ssot_report.md`

### 4) Promote outputs
When the staged outputs look correct, copy them into the repo (or your deployment target). This is the only step that changes production configuration files.

Minimum files to promote:
- `ignition_tags.json`
- `der_headend/assets_test.yaml`
- `der_headend/opcua_map_<site>.yaml`

### 5) Reload services
The services read these files at startup, so you must reload them for changes to take effect.

- Restart `der_headend` (loads `assets_test.yaml` on startup).
- Restart `edge_agent_*` (loads `assets_test.yaml` + `opcua_map_<site>.yaml` on startup).
- Import `ignition_tags.json` into Ignition if tag definitions changed (Tag Browser → Import).

Podman examples (replace container IDs):
```
podman exec -it <edge_agent_container_id> /usr/local/ignition/ignition.sh restart
podman exec -it <der_headend_container_id> /usr/local/ignition/ignition.sh restart
```

## Validation Modes
Validation prevents SSOT mistakes from silently propagating into runtime configuration files.
By default, the generator runs in strict validation mode and fails on errors.

- Strict (default):
```
python3 tools/generate_ignition_files.py
```

- Warn and continue:
```
python3 tools/generate_ignition_files.py --warn
```

## Troubleshooting
- Missing assets for a site: validation fails because assets reference unknown `site_id` values.
- OPC UA write errors: confirm `opcua_map_<site>.yaml` endpoint, tag_root, and providers; verify Ignition tag paths.
- Tag path mismatch: ensure `tag_root` matches the Ignition tag folder (e.g., `Assets`).

## Add a New Site (Happy Path)
1) Add the site to `ignition_ssot.json` under `sites` with a unique `id`, `name`, and `location`.
2) Add an `opcua` block that defines how edge agents connect to this site’s Ignition gateway:
   - `endpoint`
   - `tag_root`
   - `default_setpoint_provider`
   - `setpoint_provider`
   - `telemetry_provider`
   - `telemetry_interval_s`
   - `telemetry_write_sim`
3) Add assets for the site under `assets` (use the same `site_id`).
4) Regenerate outputs and report (see Happy Path above).
5) Deploy the new `opcua_map_<site>.yaml` and restart edge agents.

## Add a New Asset
1) Add the asset to `ignition_ssot.json` under `assets` with the correct `site_id`.
2) Regenerate outputs and report.
3) Deploy `assets_test.yaml` and `opcua_map_<site>.yaml`.
4) Restart `der_headend` and edge agents.

## Add or Change Telemetry Fields
Telemetry fields are metadata-driven via `telemetry_schema` in `ignition_ssot.json`.
To add or remove a field:
1) Update `telemetry_schema` entries with `name`, `type`, and optional `source` (`opcua` or `computed`).
2) Regenerate outputs and report.
3) Import updated `ignition_tags.json` into Ignition.
4) Deploy updated `opcua_map_<site>.yaml` and restart edge agents.

## Ignition Tag Import
1) Open Ignition Designer.
2) In Tag Browser, right‑click the provider (e.g., `[default]`) → Import.
3) Select `ignition_tags.json` and import into the desired folder (typically root).

## Notes on Providers and Tag Root
- `tag_root` should match the top‑level Ignition folder that contains assets (e.g., `Assets`).
- Providers should match the Ignition tag provider for that gateway (`default` or `edge`).

## Common Validation Errors
- `asset <id> references unknown site_id`: asset site_id doesn’t exist in `sites`.
- `duplicate site id`: you have two sites with the same id.
- `opcua config missing keys`: add required fields under the site’s `opcua` block.
