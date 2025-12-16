# Test Plan: Edge Gateway Mode (demo-ready)

This test plan verifies **A) Minimal Edge Gateway Mode (demo-ready)**:
- One **gateway `edge_agent`** maintains **one outbound-initiated bidirectional gRPC stream** to `der_headend`
- That single stream serves **multiple `asset_id`s**
- Headend tracks **distinct latest telemetry per asset**
- **Per-asset dispatch** affects only the targeted asset
- **Target resolution precedence:** if a Setpoint includes both `asset_id` and `site_id`, the agent MUST treat it as a **per-asset** command (asset_id wins) to avoid accidental fanout
- **Site dispatch** is split deterministically by `capacity_mwhr` (Gateway-Test is 3:1:1)

---

## Recommended run order (fast happy-path)
1. Start `der_headend` (Terminal 1)
2. Start **Gateway Mode** `edge_agent` (Terminal 2)
3. Sanity check `/assets` and `/agents`
4. Run A5 (per-asset dispatch) — proves `asset_id` routing
5. Run A6 (site dispatch) — proves capacity-weighted split

---

## Prerequisites

## How many `edge_agent` processes should be running?
This system supports two operational modes:

- **Single-asset mode (today / launcher default):** run **one `edge_agent` per asset** in `assets_test.yaml`. If you use `agent_launcher` (test harness), it will spawn **one agent per asset**.
- **Gateway Mode (one-per-site):** run **one `edge_agent` per site**. A single agent process serves multiple assets by loading `ASSETS_PATH` and selecting assets by `GATEWAY_SITE_ID`.

**Note (what good looks like):**
- `/agents` shows **one row per configured `asset_id`**.
- In Gateway Mode, multiple assets share the **same `peer`** (one agent process / one gRPC stream).
- Each asset should have **exactly one open session** (`disconnected_at: null`). Previous sessions should be closed with a `disconnected_at` timestamp.

---

### Services
- `der_headend` running with REST on `http://127.0.0.1:3001`
- `der_headend` running with gRPC on `127.0.0.1:<grpc-port>` (default is often `50051`; your launcher may default to `50070`)
- One or more `edge_agent` processes connected to the headend (either one-per-asset **or** one-per-site in gateway mode)

### Start commands (local)
Assumption: `assets_test.yaml` is located at `der_headend/assets_test.yaml` from the repo root.

**Terminal 1 — start headend**
```bash
cd /Users/pfriedland/projects/rust/integrated_system
export ASSETS_PATH=der_headend/assets_test.yaml

# Optional (DB-backed history):
# export DATABASE_URL='postgres://user:pass@localhost:5432/db'

# Optional (dev reset): if DATABASE_URL is set and RESET_DB is truthy, tables are truncated on startup.
# export RESET_DB=1

# Pick a gRPC port and use it consistently. If you plan to use agent_launcher,
# setting HEADEND_GRPC_ADDR to 50070 avoids the common 50051 vs 50070 mismatch.
export HEADEND_GRPC_ADDR=127.0.0.1:50070

cargo run -p der_headend
```

**Terminal 2 — start agents (Option 1: launcher test harness, one agent per asset)**
```bash
cd /Users/pfriedland/projects/rust/integrated_system
export ASSETS_PATH=der_headend/assets_test.yaml
export HEADEND_GRPC=127.0.0.1:50070

cargo run -p agent_launcher
```

**Terminal 2 — start agents (Option 2: Gateway Mode, one agent per site)**
This starts a **single** `edge_agent` process that serves all assets under `GATEWAY_SITE_ID`.

```bash
cd /Users/pfriedland/projects/rust/integrated_system
export HEADEND_GRPC=127.0.0.1:50070
export ASSETS_PATH=der_headend/assets_test.yaml
export GATEWAY_SITE_ID=6ba7b810-9dad-11d1-80b4-00c04fd430c8

# Optional: logical gateway identifier; defaults to GATEWAY_SITE_ID.
# export GATEWAY_ID=6ba7b810-9dad-11d1-80b4-00c04fd430c8

cargo run -p edge_agent
```

**Sanity check**
```bash
curl -s http://127.0.0.1:3001/agents | jq .
curl -s http://127.0.0.1:3001/assets | jq .

# In Gateway Mode you should see A/B/C all `connected:true` and typically the same `peer`.
# Sessions should show one open entry per asset (disconnected_at: null).
```

If `/agents` is empty, the most common cause is a **gRPC port mismatch** between `HEADEND_GRPC_ADDR` (headend) and `HEADEND_GRPC` (agents).

### API basics
- REST dispatch example (per-asset):
  ```bash
  curl -X POST http://127.0.0.1:3001/dispatch \
    -H 'content-type: application/json' \
    -d '{"asset_id":"7ba7b810-9dad-11d1-80b4-00c04fd430c8","mw":5}'
  ```

### Helpful tooling (optional)
- `jq` for pretty printing JSON:
  ```bash
  brew install jq
  ```

---

## Reference asset IDs for this run

Example (Gateway-Test / current `assets_test.yaml`):
- ASSET_A = `7ba7b810-9dad-11d1-80b4-00c04fd430c8` (GT-BESS-A)
- ASSET_B = `8ba7b810-9dad-11d1-80b4-00c04fd430c8` (GT-BESS-B)
- ASSET_C = `9ba7b810-9dad-11d1-80b4-00c04fd430c8` (GT-BESS-C)

For convenience:
```bash
export HEADEND=http://127.0.0.1:3001
export SITE_ID=6ba7b810-9dad-11d1-80b4-00c04fd430c8
export ASSET_A=7ba7b810-9dad-11d1-80b4-00c04fd430c8
export ASSET_B=8ba7b810-9dad-11d1-80b4-00c04fd430c8
export ASSET_C=9ba7b810-9dad-11d1-80b4-00c04fd430c8
```

---

## A1–A2: Registration + headend routing map

### Goal
Headend receives a Register message indicating the gateway serves `[ASSET_A, ASSET_B, ...]`, and can route setpoints by `asset_id` to that stream.

### Actions
1. Start `der_headend`
2. Start gateway-mode `edge_agent`
3. Check connected agent list (best-effort presence):
   ```bash
   curl -s $HEADEND/agents | jq .
   ```
4. Confirm assets are known to headend:
   ```bash
   curl -s $HEADEND/assets | jq .
   ```

### Expected results
- `/agents` shows **one row per configured asset** (even if disconnected), and shows `connected:true` for assets currently served by an agent.
  - With `agent_launcher`: expect **one entry per asset** (different `peer` ports).
  - With Gateway Mode: expect **multiple assets** sharing the **same `peer`**.
  - In Gateway Mode, all assets for the site should typically share the same `peer` (single agent process).
- `/assets` includes `ASSET_A`, `ASSET_B`, and `ASSET_C` (matches `assets_test.yaml`).
- Headend logs (recommended) show something like:
  - `Register ... asset_id=... site_id=...` (single-asset mode)
  - or `Register ... assets=[ASSET_A, ASSET_B, ...]` (gateway registration)

### Common failures
- `/agents` empty: agent not connected / gRPC port mismatch / TLS mismatch
- `/assets` missing IDs: assets.yaml not loaded or headend uses a different config file

---

## A3–A4: Telemetry multiplexing (distinct telemetry per asset)

### Goal
Each `asset_id` has its **own** latest telemetry record.

### Actions
1. Fetch latest telemetry for each asset:
   ```bash
   curl -s $HEADEND/telemetry/$ASSET_A | jq .
   curl -s $HEADEND/telemetry/$ASSET_B | jq .
   curl -s $HEADEND/telemetry/$ASSET_C | jq .
   ```
2. Wait ~5–10 seconds, then fetch again:
   ```bash
   sleep 6
   curl -s $HEADEND/telemetry/$ASSET_A | jq .
   curl -s $HEADEND/telemetry/$ASSET_B | jq .
   curl -s $HEADEND/telemetry/$ASSET_C | jq .
   ```

### Expected results
- Both endpoints return JSON successfully (not 404)
- Timestamps (or last-updated indicators) advance over time
- Values are **not accidentally identical across all assets** unless your simulator intentionally makes them identical

### Red flags (classic gateway bugs)
- Both assets show identical values and identical timestamps forever
  - Likely overwriting a single “latest telemetry” slot instead of keying by `asset_id`
- One asset updates, the other returns 404
  - Likely telemetry missing `asset_id` or headend routing map incomplete

---

## A5: Per-asset dispatch (must only affect the targeted asset)

### Goal
Dispatch to `ASSET_A` changes only `ASSET_A` behavior/telemetry.

### Actions
1. Record baseline telemetry:
   ```bash
   echo "Baseline A:"
   curl -s $HEADEND/telemetry/$ASSET_A | jq .

   echo "Baseline B:"
   curl -s $HEADEND/telemetry/$ASSET_B | jq .

   echo "Baseline C:"
   curl -s $HEADEND/telemetry/$ASSET_C | jq .
   ```

2. Dispatch to **ASSET_A**:
   ```bash
   curl -s -X POST $HEADEND/dispatch \
     -H 'content-type: application/json' \
     -d "{\"asset_id\":\"$ASSET_A\",\"mw\":5}" | jq .
   ```

3. Wait 1–2 ticks (10 seconds is safe), then re-check telemetry:
   ```bash
   sleep 10
   echo "After dispatch A:"
   curl -s $HEADEND/telemetry/$ASSET_A | jq .

   echo "After dispatch B:"
   curl -s $HEADEND/telemetry/$ASSET_B | jq .

   echo "After dispatch C:"
   curl -s $HEADEND/telemetry/$ASSET_C | jq .
   ```

### Expected results
- Telemetry for `ASSET_A` shows a clear change consistent with a 5 MW setpoint (exact field depends on your model)
- Telemetry for `ASSET_B` does **not** show the same change caused by that dispatch
- Telemetry for `ASSET_C` does **not** show the same change caused by that dispatch
- Agent logs show it targeted **one** asset (no fanout). In Gateway Mode a good signature is `num_assets=1` / no allocation table across the site.

### Common failures
- Multiple assets change:
  - Agent treated the command as a site-level dispatch because `site_id` was present. Fix: `asset_id` must win over `site_id` when both are provided.
- Neither changes:
  - Headend didn’t route the setpoint to the stream, or agent didn’t recognize the message
- HTTP 200 but no effect:
  - Dispatch accepted by REST but not delivered/applied (lack of ack is normal in demo-ready; check logs)

---

## A6: Site dispatch split by `site_id` (capacity-weighted)

### Goal
A dispatch targeting a `site_id` is split deterministically into per-asset allocations and applied.

This plan assumes the split weight is **`capacity_mwhr`** from `assets_test.yaml`.

With the recommended `assets_test.yaml` values:
- `ASSET_A capacity_mwhr = 120`
- `ASSET_B capacity_mwhr = 40`
- `ASSET_C capacity_mwhr = 40`

The weight ratio is **3:1:1** (sum = 200). For a 12 MW site dispatch, the split should be:
- `ASSET_A = 7.2 MW`
- `ASSET_B = 2.4 MW`
- `ASSET_C = 2.4 MW`

(Then each allocation is clamped to `[min_mw, max_mw]`.)

### Actions
1. Issue a site dispatch (12 MW makes the split obvious):
   ```bash
   curl -s -X POST $HEADEND/dispatch \
     -H 'content-type: application/json' \
     -d "{\"site_id\":\"$SITE_ID\",\"mw\":12}" | jq .
   ```

2. Check **edge_agent** logs for the computed split (recommended log contents):
   - total requested MW (`mw_total`)
   - list of assets considered
   - each asset’s `capacity_mwhr`
   - computed per-asset allocations (raw + clamped)
   - confirmation the sum matches the request within rounding tolerance

3. Verify with telemetry:
   ```bash
   sleep 10
   echo "After site dispatch (A):"
   curl -s $HEADEND/telemetry/$ASSET_A | jq .

   echo "After site dispatch (B):"
   curl -s $HEADEND/telemetry/$ASSET_B | jq .

   echo "After site dispatch (C):"
   curl -s $HEADEND/telemetry/$ASSET_C | jq .
   ```

### Expected results
- Telemetry shows `ASSET_A`, `ASSET_B`, and `ASSET_C` moving toward their allocated setpoints.
- The split is stable between runs (same input → same allocations).
- The sum of per-asset allocations equals the requested total within rounding tolerance.

---

## Success criteria (demo-ready)
- One gateway agent stays connected (no flapping) for at least 5 minutes
- `/telemetry/{asset_id}` returns distinct latest telemetry for at least 2 assets
- Per-asset dispatch changes only the targeted asset’s telemetry/behavior
- Site dispatch (`site_id`) splits deterministically across A/B/C (3:1:1) and is visible via logs + telemetry

---

## Known-good signatures (quick eyeball)

### Gateway registration
- `/agents` shows A/B/C with the **same `peer`**
- Headend logs show `received Register ... num_assets=3 asset_ids=[A,B,C]`

### Per-asset dispatch
- `POST /dispatch` with `{"asset_id": "ASSET_A"}` changes only ASSET_A
- Agent logs show no fanout (`num_assets=1`)

### Site dispatch
- `POST /dispatch` with `{"site_id": "SITE_ID"}` triggers split
- Agent logs show allocation rows for 3 assets and sum-applied equals total

---

## Troubleshooting checklist

### If `/dispatch` returns success but nothing changes
- Check headend logs: did it route to a connected stream for that `asset_id`?
- Check agent logs: did it receive a Setpoint? Did it match the `asset_id`?
- Confirm the gateway Register included that `asset_id` (assets list)

### If telemetry only shows up for one asset
- Confirm agent is sending telemetry tagged with `asset_id` for each asset
- Confirm headend stores latest telemetry keyed by `asset_id`

### If gRPC connection drops
- Add keepalive/idle timeout tuning (this is part of V1 pilot hardening, not demo-ready)
