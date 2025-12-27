#!/usr/bin/env python3
import argparse
import json
import tarfile
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_SSOT_PATH = ROOT / "ignition_ssot.json"

DEFAULT_OPCUA_MAP_DIR = ROOT / "der_headend"
DEFAULT_ASSETS_TEST_OUTPUT = ROOT / "der_headend" / "assets_test.yaml"
DEFAULT_IGNITION_TAGS_OUTPUT = ROOT / "ignition_tags.json"
DEFAULT_TELEMETRY_SCHEMA = [
    {"name": "soc_pct", "type": "f64", "source": "opcua"},
    {"name": "soc_mwhr", "type": "f64", "source": "opcua"},
    {"name": "current_mw", "type": "f64", "source": "opcua"},
    {"name": "setpoint_mw", "type": "f64", "source": "computed"},
    {"name": "status", "type": "string", "source": "opcua"},
    {"name": "voltage_v", "type": "f64", "source": "opcua"},
    {"name": "current_a", "type": "f64", "source": "opcua"},
    {"name": "dc_bus_v", "type": "f64", "source": "opcua"},
    {"name": "dc_bus_a", "type": "f64", "source": "opcua"},
    {"name": "temperature_cell_f", "type": "f64", "source": "opcua"},
    {"name": "temperature_module_f", "type": "f64", "source": "opcua"},
    {"name": "temperature_ambient_f", "type": "f64", "source": "opcua"},
    {"name": "soh_pct", "type": "f64", "source": "opcua"},
    {"name": "cycle_count", "type": "u64", "source": "opcua"},
    {"name": "energy_in_mwh", "type": "f64", "source": "opcua"},
    {"name": "energy_out_mwh", "type": "f64", "source": "opcua"},
    {"name": "available_charge_kw", "type": "f64", "source": "opcua"},
    {"name": "available_discharge_kw", "type": "f64", "source": "opcua"},
]


def load_ssot(path):
    with path.open("r", encoding="utf-8") as f:
        return json.load(f)

def validate_ssot(ssot, strict=True):
    sites = ssot.get("sites", [])
    assets = ssot.get("assets", [])
    errors = []
    site_ids = [s.get("id") for s in sites]
    if len(site_ids) != len(set(site_ids)):
        errors.append("duplicate site id found in SSOT")
    site_id_set = set(site_ids)
    for asset in assets:
        site_id = asset.get("site_id")
        if site_id not in site_id_set:
            errors.append(
                "asset %s references unknown site_id %s" % (asset.get("id"), site_id)
            )
    for site in sites:
        opcua = site.get("opcua")
        if not opcua:
            continue
        missing = [k for k in (
            "endpoint",
            "default_setpoint_provider",
            "setpoint_provider",
            "telemetry_provider",
            "telemetry_interval_s",
            "telemetry_write_sim",
        ) if k not in opcua]
        if missing:
            errors.append(
                "site %s opcua config missing keys: %s"
                % (site.get("id"), ", ".join(missing))
            )
    telemetry_schema = ssot.get("telemetry_schema") or []
    if telemetry_schema:
        seen = set()
        for entry in telemetry_schema:
            name = entry.get("name")
            field_type = entry.get("type")
            if not name or not field_type:
                errors.append("telemetry_schema entries require name and type")
                continue
            if name in seen:
                errors.append("telemetry_schema has duplicate field %s" % name)
            seen.add(name)
            source = entry.get("source", "opcua")
            if source not in ("opcua", "computed"):
                errors.append(
                    "telemetry_schema field %s has invalid source %s"
                    % (name, source)
                )
    if errors:
        msg = "validation failed:\\n- " + "\\n- ".join(errors)
        if strict:
            raise ValueError(msg)
        print("WARNING:", msg)

def telemetry_schema_from_ssot(ssot):
    schema = ssot.get("telemetry_schema")
    if not schema:
        return DEFAULT_TELEMETRY_SCHEMA
    fields = []
    for entry in schema:
        name = entry.get("name")
        field_type = entry.get("type")
        source = entry.get("source", "opcua")
        if not name or not field_type:
            raise ValueError("telemetry_schema entries require name and type")
        fields.append({"name": name, "type": field_type, "source": source})
    return fields

def ignition_type_for_schema(field_type):
    field_type = str(field_type).lower()
    if field_type in ("f64", "float", "double", "number"):
        return "Float4", 0
    if field_type in ("i64", "int64", "integer", "int"):
        return "Int8", 0
    if field_type in ("u64", "uint64", "uint"):
        return "Int8", 0
    if field_type in ("bool", "boolean"):
        return "Boolean", False
    if field_type in ("string", "str"):
        return "String", ""
    return "Float4", 0

def asset_folder(asset_name, telemetry_schema, read_only=False):
    telemetry_tags = []
    for field in telemetry_schema:
        data_type, default_value = ignition_type_for_schema(field["type"])
        telemetry_tags.append(
            atomic_tag(field["name"], data_type, default_value, read_only)
        )
    control_tags = [
        atomic_tag("setpoint_mw", "Float4", 0, read_only),
        atomic_tag("duration_s", "Int4", 0, read_only),
        atomic_tag("dispatch_id", "String", "", read_only),
    ]
    event_tags = [
        atomic_tag("last_event", "String", "", read_only),
    ]
    return folder_tag(
        asset_name,
        [
            folder_tag("telemetry", telemetry_tags),
            folder_tag("control", control_tags),
            folder_tag("events", event_tags),
        ],
    )


def folder_tag(name, tags):
    return {
        "name": name,
        "tagType": "Folder",
        "tags": tags,
    }


def atomic_tag(name, data_type, value, read_only):
    return {
        "name": name,
        "tagType": "AtomicTag",
        "valueSource": "memory",
        "dataType": data_type,
        "value": value,
        "readOnly": bool(read_only),
    }


def write_ignition_tags(assets, telemetry_schema, output_path):
    root = folder_tag(
        "Assets",
        [asset_folder(a["name"], telemetry_schema, read_only=False) for a in assets],
    )
    payload = {
        "meta": {
            "generated_by": "tools/generate_ignition_files.py",
            "purpose": "Ignition tag definitions for telemetry/control values under [default]/Assets",
            "notes": "These tags are runtime values updated by simulators or real assets.",
        },
        "tags": [root],
    }
    with output_path.open("w", encoding="utf-8") as f:
        json.dump(payload, f, indent=2)
        f.write("\n")


def write_assets_test(sites, assets, output_path):
    lines = []
    lines.append("# Generated by tools/generate_ignition_files.py")
    lines.append("# Static asset/site configuration (non-telemetry).")
    lines.append("sites:")
    for site in sites:
        lines.append("  - id: \"%s\"" % site["id"])
        lines.append("    name: \"%s\"" % site["name"])
        lines.append("    location: \"%s\"" % site["location"])
    lines.append("")
    lines.append("assets:")
    for asset in assets:
        lines.append("  - id: \"%s\"" % asset["id"])
        lines.append("    name: \"%s\"" % asset["name"])
        lines.append("    site_id: \"%s\"" % asset["site_id"])
        lines.append("    capacity_mwhr: %s" % asset["capacity_mwhr"])
        lines.append("    max_mw: %s" % asset["max_mw"])
        lines.append("    min_mw: %s" % asset["min_mw"])
        lines.append("    min_soc_pct: %s" % asset["min_soc_pct"])
        lines.append("    max_soc_pct: %s" % asset["max_soc_pct"])
        lines.append("    efficiency: %s" % asset["efficiency"])
        lines.append("    ramp_rate_mw_per_min: %s" % asset["ramp_rate_mw_per_min"])
        lines.append("")
    if lines and lines[-1] == "":
        lines.pop()
    output_path.write_text("\n".join(lines) + "\n", encoding="utf-8")


def tag_path(provider, tag_root, asset_name, suffix):
    base = "[%s]" % provider
    if tag_root:
        base = "%s/%s/%s" % (base, tag_root, asset_name)
    else:
        base = "%s/%s" % (base, asset_name)
    return "ns=2;s=%s/%s" % (base, suffix)


def slugify(value):
    out = []
    for ch in value.strip():
        if ch.isalnum() or ch in ("-", "_"):
            out.append(ch.lower())
        elif ch.isspace():
            out.append("_")
    return "".join(out) or "site"


def build_opcua_map_data(site, opcua_cfg, assets, telemetry_schema):
    map_name = opcua_cfg.get("map_name") or site["name"]
    map_slug = slugify(map_name)

    tag_root = opcua_cfg.get("tag_root", "")

    site_assets = [a for a in assets if a["site_id"] == site["id"]]
    if not site_assets:
        raise ValueError("no assets found for site %s" % site["id"])
    site_assets = sorted(site_assets, key=lambda a: a["id"])
    default_asset = site_assets[0]

    return {
        "map_name": map_name,
        "map_slug": map_slug,
        "site_id": site["id"],
        "endpoint": opcua_cfg["endpoint"],
        "tag_root": tag_root,
        "default_setpoint_provider": opcua_cfg["default_setpoint_provider"],
        "setpoint_provider": opcua_cfg["setpoint_provider"],
        "telemetry_provider": opcua_cfg["telemetry_provider"],
        "telemetry_interval_s": opcua_cfg["telemetry_interval_s"],
        "telemetry_write_sim": opcua_cfg["telemetry_write_sim"],
        "default_setpoint_asset": default_asset,
        "site_assets": site_assets,
        "telemetry_fields": [
            field["name"]
            for field in telemetry_schema
            if field.get("source", "opcua") == "opcua"
        ],
    }


def write_opcua_map(map_data, output_dir):
    output_path = output_dir / ("opcua_map_%s.yaml" % map_data["map_slug"])

    lines = []
    lines.append("# Generated by tools/generate_ignition_files.py")
    lines.append("# OPC UA node mapping for telemetry reads and setpoint writes.")
    lines.append("endpoint: %s" % map_data["endpoint"])
    lines.append(
        "# Note: tag provider is usually [default]; if namespace index changes, use nsu=urn:inductiveautomation:ignition:opcua:tags."
    )
    lines.append("# Default fallback if an asset_id is not explicitly mapped")
    lines.append(
        "default_setpoint: %s"
        % tag_path(
            map_data["default_setpoint_provider"],
            map_data["tag_root"],
            map_data["default_setpoint_asset"]["name"],
            "control/setpoint_mw",
        )
    )
    lines.append("setpoints:")
    for asset in map_data["site_assets"]:
        lines.append(
            "  \"%s\": %s"
            % (
                asset["id"],
                tag_path(
                    map_data["setpoint_provider"],
                    map_data["tag_root"],
                    asset["name"],
                    "control/setpoint_mw",
                ),
            )
        )
    lines.append("telemetry_interval_s: %s" % map_data["telemetry_interval_s"])
    lines.append(
        "telemetry_write_sim: %s"
        % str(bool(map_data["telemetry_write_sim"])).lower()
    )
    lines.append("telemetry_assets:")
    for asset in map_data["site_assets"]:
        lines.append("  \"%s\":" % asset["id"])
        for key in map_data["telemetry_fields"]:
            lines.append(
                "    %s: %s"
                % (
                    key,
                    tag_path(
                        map_data["telemetry_provider"],
                        map_data["tag_root"],
                        asset["name"],
                        "telemetry/%s" % key,
                    ),
                )
            )

    output_path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return output_path


def collect_tag_rows(assets, telemetry_schema):
    rows = []
    rows.append(("Assets", "Folder", None, None, None, None))
    for asset in assets:
        asset_root = "Assets/%s" % asset["name"]
        rows.append((asset_root, "Folder", None, None, None, None))
        telemetry_tags = []
        for field in telemetry_schema:
            data_type, default_value = ignition_type_for_schema(field["type"])
            telemetry_tags.append((field["name"], data_type, default_value, False))
        for group, tags in (
            ("telemetry", telemetry_tags),
            (
                "control",
                [
                    ("setpoint_mw", "Float4", 0, False),
                    ("duration_s", "Int4", 0, False),
                    ("dispatch_id", "String", "", False),
                ],
            ),
            ("events", [("last_event", "String", "", False)]),
        ):
            group_path = "%s/%s" % (asset_root, group)
            rows.append((group_path, "Folder", None, None, None, None))
            for name, data_type, value, read_only in tags:
                tag_path_value = "%s/%s" % (group_path, name)
                rows.append(
                    (
                        tag_path_value,
                        "AtomicTag",
                        data_type,
                        "memory",
                        bool(read_only),
                        json.dumps(value),
                    )
                )
    return rows


def write_duckdb(output_dir, sites, assets, map_data_list, telemetry_schema):
    try:
        import duckdb  # type: ignore
    except ImportError as exc:
        raise RuntimeError(
            "duckdb module not installed; install with 'pip install duckdb'"
        ) from exc

    db_path = output_dir / "ssot.duckdb"
    con = duckdb.connect(str(db_path))

    con.execute("DROP TABLE IF EXISTS sites")
    con.execute("DROP TABLE IF EXISTS assets")
    con.execute("DROP TABLE IF EXISTS opcua_maps")
    con.execute("DROP TABLE IF EXISTS opcua_setpoints")
    con.execute("DROP TABLE IF EXISTS opcua_telemetry_nodes")
    con.execute("DROP TABLE IF EXISTS ignition_tags")

    con.execute(
        """
        CREATE TABLE sites (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            location TEXT NOT NULL,
            map_name TEXT,
            endpoint TEXT,
            tag_root TEXT,
            default_setpoint_provider TEXT,
            setpoint_provider TEXT,
            telemetry_provider TEXT,
            telemetry_interval_s BIGINT,
            telemetry_write_sim BOOLEAN
        )
        """
    )
    con.execute(
        """
        CREATE TABLE assets (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            site_id TEXT NOT NULL,
            capacity_mwhr DOUBLE,
            max_mw DOUBLE,
            min_mw DOUBLE,
            min_soc_pct DOUBLE,
            max_soc_pct DOUBLE,
            efficiency DOUBLE,
            ramp_rate_mw_per_min DOUBLE
        )
        """
    )
    con.execute(
        """
        CREATE TABLE opcua_maps (
            map_name TEXT PRIMARY KEY,
            site_id TEXT NOT NULL,
            endpoint TEXT NOT NULL,
            tag_root TEXT,
            default_setpoint_provider TEXT,
            setpoint_provider TEXT,
            telemetry_provider TEXT,
            telemetry_interval_s BIGINT,
            telemetry_write_sim BOOLEAN,
            default_setpoint_asset_id TEXT,
            default_setpoint_node TEXT,
            file_path TEXT
        )
        """
    )
    con.execute(
        """
        CREATE TABLE opcua_setpoints (
            map_name TEXT NOT NULL,
            asset_id TEXT NOT NULL,
            node_id TEXT NOT NULL
        )
        """
    )
    con.execute(
        """
        CREATE TABLE opcua_telemetry_nodes (
            map_name TEXT NOT NULL,
            asset_id TEXT NOT NULL,
            field TEXT NOT NULL,
            node_id TEXT NOT NULL
        )
        """
    )
    con.execute(
        """
        CREATE TABLE ignition_tags (
            tag_path TEXT PRIMARY KEY,
            tag_type TEXT NOT NULL,
            data_type TEXT,
            value_source TEXT,
            read_only BOOLEAN,
            value_json TEXT
        )
        """
    )
    con.execute("DROP VIEW IF EXISTS site_assets")
    con.execute("DROP VIEW IF EXISTS site_asset_summary")
    con.execute("DROP VIEW IF EXISTS opcua_setpoint_map")
    con.execute("DROP VIEW IF EXISTS opcua_telemetry_map")
    con.execute(
        """
        CREATE VIEW site_assets AS
        SELECT
            s.id AS site_id,
            s.name AS site_name,
            s.location,
            a.id AS asset_id,
            a.name AS asset_name,
            a.capacity_mwhr,
            a.max_mw,
            a.min_mw,
            a.min_soc_pct,
            a.max_soc_pct,
            a.efficiency,
            a.ramp_rate_mw_per_min
        FROM sites s
        JOIN assets a ON a.site_id = s.id
        """
    )
    con.execute(
        """
        CREATE VIEW site_asset_summary AS
        SELECT
            s.id AS site_id,
            s.name AS site_name,
            COUNT(a.id) AS asset_count,
            SUM(a.capacity_mwhr) AS capacity_mwhr_total,
            SUM(a.max_mw) AS max_mw_total,
            SUM(a.min_mw) AS min_mw_total,
            AVG(a.min_soc_pct) AS min_soc_pct_avg,
            AVG(a.max_soc_pct) AS max_soc_pct_avg,
            AVG(a.efficiency) AS efficiency_avg,
            AVG(a.ramp_rate_mw_per_min) AS ramp_rate_mw_per_min_avg
        FROM sites s
        LEFT JOIN assets a ON a.site_id = s.id
        GROUP BY s.id, s.name
        """
    )
    con.execute(
        """
        CREATE VIEW opcua_setpoint_map AS
        SELECT
            m.map_name,
            m.site_id,
            s.name AS site_name,
            sp.asset_id,
            a.name AS asset_name,
            sp.node_id
        FROM opcua_setpoints sp
        JOIN opcua_maps m ON m.map_name = sp.map_name
        LEFT JOIN sites s ON s.id = m.site_id
        LEFT JOIN assets a ON a.id = sp.asset_id
        """
    )
    con.execute(
        """
        CREATE VIEW opcua_telemetry_map AS
        SELECT
            m.map_name,
            m.site_id,
            s.name AS site_name,
            tn.asset_id,
            a.name AS asset_name,
            tn.field,
            tn.node_id
        FROM opcua_telemetry_nodes tn
        JOIN opcua_maps m ON m.map_name = tn.map_name
        LEFT JOIN sites s ON s.id = m.site_id
        LEFT JOIN assets a ON a.id = tn.asset_id
        """
    )

    site_rows = []
    for site in sites:
        opcua = site.get("opcua") or {}
        site_rows.append(
            (
                site["id"],
                site["name"],
                site["location"],
                opcua.get("map_name"),
                opcua.get("endpoint"),
                opcua.get("tag_root"),
                opcua.get("default_setpoint_provider"),
                opcua.get("setpoint_provider"),
                opcua.get("telemetry_provider"),
                opcua.get("telemetry_interval_s"),
                opcua.get("telemetry_write_sim"),
            )
        )
    con.executemany(
        """
        INSERT INTO sites VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        """,
        site_rows,
    )

    asset_rows = [
        (
            a["id"],
            a["name"],
            a["site_id"],
            a["capacity_mwhr"],
            a["max_mw"],
            a["min_mw"],
            a["min_soc_pct"],
            a["max_soc_pct"],
            a["efficiency"],
            a["ramp_rate_mw_per_min"],
        )
        for a in assets
    ]
    con.executemany(
        """
        INSERT INTO assets VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        """,
        asset_rows,
    )

    for map_data in map_data_list:
        default_node = tag_path(
            map_data["default_setpoint_provider"],
            map_data["tag_root"],
            map_data["default_setpoint_asset"]["name"],
            "control/setpoint_mw",
        )
        con.execute(
            """
            INSERT INTO opcua_maps VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            """,
            (
                map_data["map_name"],
                map_data["site_id"],
                map_data["endpoint"],
                map_data["tag_root"],
                map_data["default_setpoint_provider"],
                map_data["setpoint_provider"],
                map_data["telemetry_provider"],
                map_data["telemetry_interval_s"],
                bool(map_data["telemetry_write_sim"]),
                map_data["default_setpoint_asset"]["id"],
                default_node,
                str(map_data["output_path"]),
            ),
        )

        setpoint_rows = []
        telemetry_rows = []
        for asset in map_data["site_assets"]:
            setpoint_rows.append(
                (
                    map_data["map_name"],
                    asset["id"],
                    tag_path(
                        map_data["setpoint_provider"],
                        map_data["tag_root"],
                        asset["name"],
                        "control/setpoint_mw",
                    ),
                )
            )
            for field in map_data["telemetry_fields"]:
                telemetry_rows.append(
                    (
                        map_data["map_name"],
                        asset["id"],
                        field,
                        tag_path(
                            map_data["telemetry_provider"],
                            map_data["tag_root"],
                            asset["name"],
                            "telemetry/%s" % field,
                        ),
                    )
                )
        con.executemany(
            "INSERT INTO opcua_setpoints VALUES (?, ?, ?)",
            setpoint_rows,
        )
        con.executemany(
            "INSERT INTO opcua_telemetry_nodes VALUES (?, ?, ?, ?)",
            telemetry_rows,
        )

    tag_rows = collect_tag_rows(assets, telemetry_schema)
    con.executemany(
        "INSERT INTO ignition_tags VALUES (?, ?, ?, ?, ?, ?)",
        tag_rows,
    )

    con.close()


def parse_args():
    parser = argparse.ArgumentParser(
        description="Generate ignition_tags.json, assets_test.yaml, and opcua_map_*.yaml from SSOT."
    )
    parser.add_argument(
        "--output-dir",
        help="Directory to write generated files (opcua_map_*.yaml, assets_test.yaml, ignition_tags.json).",
    )
    parser.add_argument(
        "--ssot",
        help="Path to SSOT JSON file.",
    )
    parser.add_argument(
        "--skip-duckdb",
        action="store_true",
        help="Skip writing ssot.duckdb.",
    )
    parser.add_argument(
        "--strict",
        action="store_true",
        help="Fail on SSOT validation errors (default).",
    )
    parser.add_argument(
        "--warn",
        action="store_true",
        help="Warn on SSOT validation errors and continue.",
    )
    return parser.parse_args()


def main():
    args = parse_args()
    ssot_path = (
        Path(args.ssot).expanduser().resolve()
        if args.ssot
        else DEFAULT_SSOT_PATH
    )
    ssot = load_ssot(ssot_path)
    strict = True
    if args.warn:
        strict = False
    if args.strict:
        strict = True
    validate_ssot(ssot, strict=strict)
    sites = ssot["sites"]
    assets = ssot["assets"]
    telemetry_schema = telemetry_schema_from_ssot(ssot)

    if args.output_dir:
        output_dir = Path(args.output_dir).expanduser().resolve()
    else:
        output_dir = ROOT

    output_dir.mkdir(parents=True, exist_ok=True)
    opcua_output_dir = output_dir / "der_headend"
    opcua_output_dir.mkdir(parents=True, exist_ok=True)

    ignition_tags_output = output_dir / DEFAULT_IGNITION_TAGS_OUTPUT.name
    assets_test_output = opcua_output_dir / DEFAULT_ASSETS_TEST_OUTPUT.name

    write_ignition_tags(assets, telemetry_schema, ignition_tags_output)
    write_assets_test(sites, assets, assets_test_output)

    map_data_list = []
    for site in sites:
        opcua_cfg = site.get("opcua")
        if not opcua_cfg:
            continue
        map_data = build_opcua_map_data(site, opcua_cfg, assets, telemetry_schema)
        output_path = write_opcua_map(map_data, opcua_output_dir)
        map_data["output_path"] = output_path
        map_data_list.append(map_data)

    timestamp = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
    tar_name = "ignition_outputs_%s.tar.gz" % timestamp
    tar_path = output_dir / tar_name
    with tarfile.open(tar_path, "w:gz") as tar:
        if ignition_tags_output.exists():
            tar.add(ignition_tags_output, arcname="ignition_tags.json")
        if assets_test_output.exists():
            tar.add(assets_test_output, arcname="der_headend/assets_test.yaml")
        for path in sorted(opcua_output_dir.glob("opcua_map_*.yaml")):
            tar.add(path, arcname="der_headend/%s" % path.name)

    if not args.skip_duckdb:
        write_duckdb(output_dir, sites, assets, map_data_list, telemetry_schema)


if __name__ == "__main__":
    main()
