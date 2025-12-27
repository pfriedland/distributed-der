#!/usr/bin/env python3
import argparse
from pathlib import Path

def fetch_all(con, sql):
    return con.execute(sql).fetchall(), [d[0] for d in con.description]


def md_table(headers, rows):
    if not rows:
        return "(no rows)\n"
    lines = []
    lines.append("| " + " | ".join(headers) + " |")
    lines.append("|" + "|".join([" --- " for _ in headers]) + "|")
    for row in rows:
        vals = ["" if v is None else str(v) for v in row]
        lines.append("| " + " | ".join(vals) + " |")
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description="Render SSOT DuckDB report as Markdown.")
    parser.add_argument("--db", help="Path to ssot.duckdb", required=True)
    parser.add_argument("--out", help="Markdown output path", required=True)
    args = parser.parse_args()

    try:
        import duckdb  # type: ignore
    except ImportError as exc:
        raise RuntimeError("duckdb module not installed; install with 'pip install duckdb'") from exc

    db_path = Path(args.db).expanduser().resolve()
    out_path = Path(args.out).expanduser().resolve()

    con = duckdb.connect(str(db_path), read_only=True)

    sections = []
    sections.append("# SSOT Configuration Report\n")
    sections.append(
        "This report summarizes the single source of truth (SSOT) configuration and its generated outputs. "
        "It is derived from the normalized DuckDB database produced by the generator script.\n"
    )
    sections.append("## How To Update\n")
    sections.append(
        "1. Edit the SSOT JSON to add sites, assets, or OPC UA settings.\n"
        "2. Run the generator to produce `assets_test.yaml`, `opcua_map_*.yaml`, `ignition_tags.json`, and `ssot.duckdb`.\n"
        "3. Deploy the generated files to the runtime environment (edge agents, headend, and Ignition).\n"
        "4. Restart or reload services to pick up changes (edge agents, headend, and Ignition tag imports).\n"
        "5. Regenerate this report to verify the results.\n"
    )
    sections.append("## Sites Summary\n")
    sections.append(
        "Each row represents a site with rollups of the assets assigned to it. "
        "Use this to confirm asset counts and capacity/limits per site.\n"
    )
    rows, headers = fetch_all(
        con,
        """
        SELECT site_id, site_name, asset_count, capacity_mwhr_total, max_mw_total, min_mw_total
        FROM site_asset_summary
        ORDER BY site_name
        """,
    )
    sections.append(md_table(headers, rows))

    sections.append("## Assets by Site\n")
    sections.append(
        "Static asset inventory grouped by site. This data feeds the headend and edge agent configuration.\n"
    )
    rows, headers = fetch_all(
        con,
        """
        SELECT site_name, asset_name, asset_id, capacity_mwhr, max_mw, min_mw,
               min_soc_pct, max_soc_pct, efficiency, ramp_rate_mw_per_min
        FROM site_assets
        ORDER BY site_name, asset_name
        """,
    )
    sections.append(md_table(headers, rows))

    sections.append("## OPC UA Maps\n")
    sections.append(
        "Per-site OPC UA connection and mapping configuration used by edge agents to read telemetry "
        "and write setpoints.\n"
    )
    rows, headers = fetch_all(
        con,
        """
        SELECT map_name, site_id, endpoint, tag_root, setpoint_provider, telemetry_provider,
               telemetry_interval_s, telemetry_write_sim
        FROM opcua_maps
        ORDER BY map_name
        """,
    )
    sections.append(md_table(headers, rows))

    sections.append("## OPC UA Setpoints\n")
    sections.append(
        "Resolved setpoint node IDs per asset and map. These nodes are written when dispatching.\n"
    )
    rows, headers = fetch_all(
        con,
        """
        SELECT map_name, site_name, asset_name, asset_id, node_id
        FROM opcua_setpoint_map
        ORDER BY map_name, asset_name
        """,
    )
    sections.append(md_table(headers, rows))

    sections.append("## OPC UA Telemetry\n")
    sections.append(
        "Resolved telemetry node IDs per asset and field. These nodes are read to ingest telemetry.\n"
    )
    rows, headers = fetch_all(
        con,
        """
        SELECT map_name, site_name, asset_name, asset_id, field, node_id
        FROM opcua_telemetry_map
        ORDER BY map_name, asset_name, field
        """,
    )
    sections.append(md_table(headers, rows))

    sections.append("## Ignition Tags Summary\n")
    sections.append(
        "Summary counts of Ignition tags generated for the default Assets hierarchy. "
        "These tags represent live telemetry/control values, not static configuration.\n"
    )
    rows, headers = fetch_all(
        con,
        """
        SELECT tag_type, COUNT(*) AS count
        FROM ignition_tags
        GROUP BY tag_type
        ORDER BY tag_type
        """,
    )
    sections.append(md_table(headers, rows))

    sections.append("## Ignition Tags by Asset\n")
    sections.append(
        "Breakdown of tag counts per asset derived from the generated Ignition tag JSON.\n"
    )
    rows, headers = fetch_all(
        con,
        """
        SELECT
            split_part(tag_path, '/', 2) AS asset_name,
            COUNT(*) AS tag_count,
            SUM(CASE WHEN tag_type = 'AtomicTag' THEN 1 ELSE 0 END) AS atomic_count,
            SUM(CASE WHEN tag_type = 'Folder' THEN 1 ELSE 0 END) AS folder_count
        FROM ignition_tags
        WHERE tag_path LIKE 'Assets/%'
          AND tag_path <> 'Assets'
        GROUP BY asset_name
        ORDER BY asset_name
        """,
    )
    sections.append(md_table(headers, rows))

    con.close()

    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text("\n".join(sections), encoding="utf-8")


if __name__ == "__main__":
    main()
