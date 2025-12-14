import uuid
from pathlib import Path

sites = [
      {"id": "6ba7b810-9dad-11d1-80b4-00c04fd430c8", "name": "Jersey", "location": "Region-A"},
      {"id": "6ba7b811-9dad-11d1-80b4-00c04fd430c8", "name": "Site-2", "location": "Region-B"},
      {"id": "6ba7b812-9dad-11d1-80b4-00c04fd430c8", "name": "Site-3", "location": "Region-C"},
      {"id": "6ba7b813-9dad-11d1-80b4-00c04fd430c8", "name": "Site-4", "location": "Region-D"},
      {"id": "6ba7b814-9dad-11d1-80b4-00c04fd430c8", "name": "Site-5", "location": "Region-E"},
  ]

assets = [
      {"id": "7ba7b810-9dad-11d1-80b4-00c04fd430c8", "name": "BESS-1", "site_id": sites[0]["id"], "capacity_mwhr": 120, "max_mw": 60, "min_mw": -60, "efficiency": 0.92, "ramp_rate_mw_per_min": 1000},
      {"id": "8ba7b810-9dad-11d1-80b4-00c04fd430c8", "name": "BESS-1B", "site_id": sites[0]["id"], "capacity_mwhr": 90, "max_mw": 45, "min_mw": -45, "efficiency": 0.91, "ramp_rate_mw_per_min": 1000},
      {"id": "7ba7b811-9dad-11d1-80b4-00c04fd430c8", "name": "BESS-2", "site_id": sites[1]["id"], "capacity_mwhr": 80, "max_mw": 40, "min_mw": -40, "efficiency": 0.9, "ramp_rate_mw_per_min": 1000},
      {"id": "7ba7b812-9dad-11d1-80b4-00c04fd430c8", "name": "BESS-3", "site_id": sites[1]["id"], "capacity_mwhr": 150, "max_mw": 75, "min_mw": -75, "efficiency": 0.93, "ramp_rate_mw_per_min": 1000},
      {"id": "7ba7b813-9dad-11d1-80b4-00c04fd430c8", "name": "BESS-4", "site_id": sites[2]["id"], "capacity_mwhr": 110, "max_mw": 55, "min_mw": -55, "efficiency": 0.91, "ramp_rate_mw_per_min": 1000},
      {"id": "7ba7b814-9dad-11d1-80b4-00c04fd430c8", "name": "BESS-5", "site_id": sites[2]["id"], "capacity_mwhr": 51, "max_mw": 45, "min_mw": -45, "efficiency": 0.9, "ramp_rate_mw_per_min": 1000},
  ]

namespace = uuid.UUID("12345678-1234-5678-1234-567812345678")
site_cycle = [s["id"] for s in sites]
start = len(assets) + 1
for idx in range(start, 101):
    name = f"BESS-{idx:03d}"
    site_id = site_cycle[(idx - 1) % len(site_cycle)]
    asset_id = str(uuid.uuid5(namespace, name))
    capacity = 80 + (idx % 10) * 5
    max_mw = 40 + (idx % 10) * 2
    min_mw = -max_mw
    efficiency = 0.9 + ((idx % 5) * 0.01)
    assets.append({
          "id": asset_id,
          "name": name,
          "site_id": site_id,
          "capacity_mwhr": float(capacity),
          "max_mw": float(max_mw),
          "min_mw": float(min_mw),
          "efficiency": float(efficiency),
          "ramp_rate_mw_per_min": 1000.0,
      })

lines = []
lines.append("sites:")
for s in sites:
    lines.append(f"  - id: \"{s['id']}\"")
    lines.append(f"    name: \"{s['name']}\"")
    lines.append(f"    location: \"{s['location']}\"")
lines.append("\nassets:")
for a in assets:
    lines.append(f"  - id: \"{a['id']}\"")
    lines.append(f"    name: \"{a['name']}\"")
    lines.append(f"    site_id: \"{a['site_id']}\"")
    lines.append(f"    capacity_mwhr: {a['capacity_mwhr']}")
    lines.append(f"    max_mw: {a['max_mw']}")
    lines.append(f"    min_mw: {a['min_mw']}")
    lines.append(f"    efficiency: {a['efficiency']}")
    lines.append(f"    ramp_rate_mw_per_min: {a['ramp_rate_mw_per_min']}")
    lines.append("")
Path("assets.yaml").write_text("\n".join(lines))
print("wrote assets.yaml with", len(assets), "assets")
  
