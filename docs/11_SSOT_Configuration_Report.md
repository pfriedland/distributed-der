# SSOT Configuration Report

This report summarizes the single source of truth (SSOT) configuration and its generated outputs. It is derived from the normalized DuckDB database produced by the generator script.

## How To Update

1. Edit the SSOT JSON to add sites, assets, or OPC UA settings.
2. Run the generator to produce `assets_test.yaml`, `opcua_map_*.yaml`, `ignition_tags.json`, and `ssot.duckdb`.
3. Deploy the generated files to the runtime environment (edge agents, headend, and Ignition).
4. Restart or reload services to pick up changes (edge agents, headend, and Ignition tag imports).
5. Regenerate this report to verify the results.

## Sites Summary

Each row represents a site with rollups of the assets assigned to it. Use this to confirm asset counts and capacity/limits per site.

| site_id | site_name | asset_count | capacity_mwhr_total | max_mw_total | min_mw_total |
| --- | --- | --- | --- | --- | --- |
| 6ba7b812-9dad-11d1-80b4-00c04fd430c8 | Campus-Test | 2 | 160.0 | 80.0 | -80.0 |
| 6ba7b810-9dad-11d1-80b4-00c04fd430c8 | Gateway-Test | 3 | 200.0 | 100.0 | -100.0 |

## Assets by Site

Static asset inventory grouped by site. This data feeds the headend and edge agent configuration.

| site_name | asset_name | asset_id | capacity_mwhr | max_mw | min_mw | min_soc_pct | max_soc_pct | efficiency | ramp_rate_mw_per_min |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Campus-Test | CT-BESS-A | 7ba7b812-9dad-11d1-80b4-00c04fd430c8 | 100.0 | 50.0 | -50.0 | 15.0 | 95.0 | 0.93 | 1000.0 |
| Campus-Test | CT-BESS-B | 8ba7b812-9dad-11d1-80b4-00c04fd430c8 | 60.0 | 30.0 | -30.0 | 10.0 | 90.0 | 0.92 | 1000.0 |
| Gateway-Test | GT-BESS-A | 7ba7b810-9dad-11d1-80b4-00c04fd430c8 | 120.0 | 60.0 | -60.0 | 10.0 | 90.0 | 0.92 | 1000.0 |
| Gateway-Test | GT-BESS-B | 8ba7b810-9dad-11d1-80b4-00c04fd430c8 | 40.0 | 20.0 | -20.0 | 10.0 | 90.0 | 0.91 | 1000.0 |
| Gateway-Test | GT-BESS-C | 9ba7b810-9dad-11d1-80b4-00c04fd430c8 | 40.0 | 20.0 | -20.0 | 10.0 | 90.0 | 0.9 | 1000.0 |

## OPC UA Maps

Per-site OPC UA connection and mapping configuration used by edge agents to read telemetry and write setpoints.

| map_name | site_id | endpoint | tag_root | setpoint_provider | telemetry_provider | telemetry_interval_s | telemetry_write_sim |
| --- | --- | --- | --- | --- | --- | --- | --- |
| campus | 6ba7b812-9dad-11d1-80b4-00c04fd430c8 | opc.tcp://host.containers.internal:62541 | Assets | default | default | 10 | True |
| gateway | 6ba7b810-9dad-11d1-80b4-00c04fd430c8 | opc.tcp://host.containers.internal:62542 |  | edge | edge | 10 | True |

## OPC UA Setpoints

Resolved setpoint node IDs per asset and map. These nodes are written when dispatching.

| map_name | site_name | asset_name | asset_id | node_id |
| --- | --- | --- | --- | --- |
| campus | Campus-Test | CT-BESS-A | 7ba7b812-9dad-11d1-80b4-00c04fd430c8 | ns=2;s=[default]/Assets/CT-BESS-A/control/setpoint_mw |
| campus | Campus-Test | CT-BESS-B | 8ba7b812-9dad-11d1-80b4-00c04fd430c8 | ns=2;s=[default]/Assets/CT-BESS-B/control/setpoint_mw |
| gateway | Gateway-Test | GT-BESS-A | 7ba7b810-9dad-11d1-80b4-00c04fd430c8 | ns=2;s=[edge]/GT-BESS-A/control/setpoint_mw |
| gateway | Gateway-Test | GT-BESS-B | 8ba7b810-9dad-11d1-80b4-00c04fd430c8 | ns=2;s=[edge]/GT-BESS-B/control/setpoint_mw |
| gateway | Gateway-Test | GT-BESS-C | 9ba7b810-9dad-11d1-80b4-00c04fd430c8 | ns=2;s=[edge]/GT-BESS-C/control/setpoint_mw |

## OPC UA Telemetry

Resolved telemetry node IDs per asset and field. These nodes are read to ingest telemetry.

| map_name | site_name | asset_name | asset_id | field | node_id |
| --- | --- | --- | --- | --- | --- |
| campus | Campus-Test | CT-BESS-A | 7ba7b812-9dad-11d1-80b4-00c04fd430c8 | current_mw | ns=2;s=[default]/Assets/CT-BESS-A/telemetry/current_mw |
| campus | Campus-Test | CT-BESS-A | 7ba7b812-9dad-11d1-80b4-00c04fd430c8 | soc_mwhr | ns=2;s=[default]/Assets/CT-BESS-A/telemetry/soc_mwhr |
| campus | Campus-Test | CT-BESS-A | 7ba7b812-9dad-11d1-80b4-00c04fd430c8 | soc_pct | ns=2;s=[default]/Assets/CT-BESS-A/telemetry/soc_pct |
| campus | Campus-Test | CT-BESS-A | 7ba7b812-9dad-11d1-80b4-00c04fd430c8 | status | ns=2;s=[default]/Assets/CT-BESS-A/telemetry/status |
| campus | Campus-Test | CT-BESS-B | 8ba7b812-9dad-11d1-80b4-00c04fd430c8 | current_mw | ns=2;s=[default]/Assets/CT-BESS-B/telemetry/current_mw |
| campus | Campus-Test | CT-BESS-B | 8ba7b812-9dad-11d1-80b4-00c04fd430c8 | soc_mwhr | ns=2;s=[default]/Assets/CT-BESS-B/telemetry/soc_mwhr |
| campus | Campus-Test | CT-BESS-B | 8ba7b812-9dad-11d1-80b4-00c04fd430c8 | soc_pct | ns=2;s=[default]/Assets/CT-BESS-B/telemetry/soc_pct |
| campus | Campus-Test | CT-BESS-B | 8ba7b812-9dad-11d1-80b4-00c04fd430c8 | status | ns=2;s=[default]/Assets/CT-BESS-B/telemetry/status |
| gateway | Gateway-Test | GT-BESS-A | 7ba7b810-9dad-11d1-80b4-00c04fd430c8 | current_mw | ns=2;s=[edge]/GT-BESS-A/telemetry/current_mw |
| gateway | Gateway-Test | GT-BESS-A | 7ba7b810-9dad-11d1-80b4-00c04fd430c8 | soc_mwhr | ns=2;s=[edge]/GT-BESS-A/telemetry/soc_mwhr |
| gateway | Gateway-Test | GT-BESS-A | 7ba7b810-9dad-11d1-80b4-00c04fd430c8 | soc_pct | ns=2;s=[edge]/GT-BESS-A/telemetry/soc_pct |
| gateway | Gateway-Test | GT-BESS-A | 7ba7b810-9dad-11d1-80b4-00c04fd430c8 | status | ns=2;s=[edge]/GT-BESS-A/telemetry/status |
| gateway | Gateway-Test | GT-BESS-B | 8ba7b810-9dad-11d1-80b4-00c04fd430c8 | current_mw | ns=2;s=[edge]/GT-BESS-B/telemetry/current_mw |
| gateway | Gateway-Test | GT-BESS-B | 8ba7b810-9dad-11d1-80b4-00c04fd430c8 | soc_mwhr | ns=2;s=[edge]/GT-BESS-B/telemetry/soc_mwhr |
| gateway | Gateway-Test | GT-BESS-B | 8ba7b810-9dad-11d1-80b4-00c04fd430c8 | soc_pct | ns=2;s=[edge]/GT-BESS-B/telemetry/soc_pct |
| gateway | Gateway-Test | GT-BESS-B | 8ba7b810-9dad-11d1-80b4-00c04fd430c8 | status | ns=2;s=[edge]/GT-BESS-B/telemetry/status |
| gateway | Gateway-Test | GT-BESS-C | 9ba7b810-9dad-11d1-80b4-00c04fd430c8 | current_mw | ns=2;s=[edge]/GT-BESS-C/telemetry/current_mw |
| gateway | Gateway-Test | GT-BESS-C | 9ba7b810-9dad-11d1-80b4-00c04fd430c8 | soc_mwhr | ns=2;s=[edge]/GT-BESS-C/telemetry/soc_mwhr |
| gateway | Gateway-Test | GT-BESS-C | 9ba7b810-9dad-11d1-80b4-00c04fd430c8 | soc_pct | ns=2;s=[edge]/GT-BESS-C/telemetry/soc_pct |
| gateway | Gateway-Test | GT-BESS-C | 9ba7b810-9dad-11d1-80b4-00c04fd430c8 | status | ns=2;s=[edge]/GT-BESS-C/telemetry/status |

## Ignition Tags Summary

Summary counts of Ignition tags generated for the default Assets hierarchy. These tags represent live telemetry/control values, not static configuration.

| tag_type | count |
| --- | --- |
| AtomicTag | 45 |
| Folder | 21 |

## Ignition Tags by Asset

Breakdown of tag counts per asset derived from the generated Ignition tag JSON.

| asset_name | tag_count | atomic_count | folder_count |
| --- | --- | --- | --- |
| CT-BESS-A | 13 | 9 | 4 |
| CT-BESS-B | 13 | 9 | 4 |
| GT-BESS-A | 13 | 9 | 4 |
| GT-BESS-B | 13 | 9 | 4 |
| GT-BESS-C | 13 | 9 | 4 |
