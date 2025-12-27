use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sim_core::{Asset, BessEvent, Dispatch, Telemetry};
use sqlx::{PgPool, postgres::PgPoolOptions, types::Json};
use tracing::warn;
use uuid::Uuid;

pub async fn maybe_connect_db() -> Result<Option<PgPool>> {
    // DATABASE_URL is optional; if not set we run in-memory only.
    let url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => return Ok(None),
    };
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .context("connecting to DATABASE_URL")?;
    init_db(&pool).await?;
    // Optional: wipe all tables on startup for development convenience.
    // Usage: RESET_DB=1 cargo run -p der_headend
    if env_truthy("RESET_DB") {
        warn!("RESET_DB is set; truncating database tables");
        reset_db(&pool).await?;
    }
    Ok(Some(pool))
}

pub async fn reset_db(pool: &PgPool) -> Result<()> {
    // TRUNCATE is fast and keeps table definitions intact.
    // NOTE: there are no foreign keys today, but CASCADE keeps this resilient if you add them later.
    sqlx::query(
        r#"
        TRUNCATE TABLE
            telemetry,
            agent_sessions,
            dispatches,
            heartbeats,
            events,
            assets
        CASCADE
        "#,
    )
    .execute(pool)
    .await
    .context("resetting database tables")?;
    Ok(())
}

pub async fn init_db(pool: &PgPool) -> Result<()> {
    // Create simple tables if they do not exist.
    // Note: run statements separately to avoid the "cannot insert multiple commands"
    // error some Postgres drivers produce when using prepared statements.
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS assets (
            id uuid PRIMARY KEY,
            site_id uuid NOT NULL,
            name text NOT NULL,
            site_name text NOT NULL,
            location text NOT NULL,
            capacity_mwhr double precision NOT NULL,
            max_mw double precision NOT NULL,
            min_mw double precision NOT NULL,
            efficiency double precision NOT NULL,
            ramp_rate_mw_per_min double precision NOT NULL
        );
    "#,
    )
    .execute(pool)
    .await
    .context("creating assets table")?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS telemetry (
            asset_id uuid NOT NULL,
            site_id uuid NOT NULL,
            ts timestamptz NOT NULL,
            soc_mwhr double precision NOT NULL,
            soc_pct double precision NOT NULL,
            capacity_mwhr double precision NOT NULL,
            current_mw double precision NOT NULL,
            setpoint_mw double precision NOT NULL,
            max_mw double precision NOT NULL,
            min_mw double precision NOT NULL,
            status text NOT NULL,
            voltage_v double precision NOT NULL DEFAULT 0,
            current_a double precision NOT NULL DEFAULT 0,
            dc_bus_v double precision NOT NULL DEFAULT 0,
            dc_bus_a double precision NOT NULL DEFAULT 0,
            temperature_cell_f double precision NOT NULL DEFAULT 0,
            temperature_module_f double precision NOT NULL DEFAULT 0,
            temperature_ambient_f double precision NOT NULL DEFAULT 0,
            soh_pct double precision NOT NULL DEFAULT 0,
            cycle_count bigint NOT NULL DEFAULT 0,
            energy_in_mwh double precision NOT NULL DEFAULT 0,
            energy_out_mwh double precision NOT NULL DEFAULT 0,
            available_charge_kw double precision NOT NULL DEFAULT 0,
            available_discharge_kw double precision NOT NULL DEFAULT 0,
            extras jsonb NOT NULL DEFAULT '{}'::jsonb
        );
    "#,
    )
    .execute(pool)
    .await
    .context("creating telemetry table")?;

    sqlx::query(r#"ALTER TABLE telemetry ADD COLUMN IF NOT EXISTS voltage_v double precision NOT NULL DEFAULT 0;"#)
        .execute(pool)
        .await
        .context("altering telemetry.voltage_v")?;
    sqlx::query(r#"ALTER TABLE telemetry ADD COLUMN IF NOT EXISTS current_a double precision NOT NULL DEFAULT 0;"#)
        .execute(pool)
        .await
        .context("altering telemetry.current_a")?;
    sqlx::query(r#"ALTER TABLE telemetry ADD COLUMN IF NOT EXISTS dc_bus_v double precision NOT NULL DEFAULT 0;"#)
        .execute(pool)
        .await
        .context("altering telemetry.dc_bus_v")?;
    sqlx::query(r#"ALTER TABLE telemetry ADD COLUMN IF NOT EXISTS dc_bus_a double precision NOT NULL DEFAULT 0;"#)
        .execute(pool)
        .await
        .context("altering telemetry.dc_bus_a")?;
    sqlx::query(r#"ALTER TABLE telemetry ADD COLUMN IF NOT EXISTS temperature_cell_f double precision NOT NULL DEFAULT 0;"#)
        .execute(pool)
        .await
        .context("altering telemetry.temperature_cell_f")?;
    sqlx::query(r#"ALTER TABLE telemetry ADD COLUMN IF NOT EXISTS temperature_module_f double precision NOT NULL DEFAULT 0;"#)
        .execute(pool)
        .await
        .context("altering telemetry.temperature_module_f")?;
    sqlx::query(r#"ALTER TABLE telemetry ADD COLUMN IF NOT EXISTS temperature_ambient_f double precision NOT NULL DEFAULT 0;"#)
        .execute(pool)
        .await
        .context("altering telemetry.temperature_ambient_f")?;
    sqlx::query(r#"ALTER TABLE telemetry ADD COLUMN IF NOT EXISTS soh_pct double precision NOT NULL DEFAULT 0;"#)
        .execute(pool)
        .await
        .context("altering telemetry.soh_pct")?;
    sqlx::query(r#"ALTER TABLE telemetry ADD COLUMN IF NOT EXISTS cycle_count bigint NOT NULL DEFAULT 0;"#)
        .execute(pool)
        .await
        .context("altering telemetry.cycle_count")?;
    sqlx::query(r#"ALTER TABLE telemetry ADD COLUMN IF NOT EXISTS energy_in_mwh double precision NOT NULL DEFAULT 0;"#)
        .execute(pool)
        .await
        .context("altering telemetry.energy_in_mwh")?;
    sqlx::query(r#"ALTER TABLE telemetry ADD COLUMN IF NOT EXISTS energy_out_mwh double precision NOT NULL DEFAULT 0;"#)
        .execute(pool)
        .await
        .context("altering telemetry.energy_out_mwh")?;
    sqlx::query(r#"ALTER TABLE telemetry ADD COLUMN IF NOT EXISTS available_charge_kw double precision NOT NULL DEFAULT 0;"#)
        .execute(pool)
        .await
        .context("altering telemetry.available_charge_kw")?;
    sqlx::query(r#"ALTER TABLE telemetry ADD COLUMN IF NOT EXISTS available_discharge_kw double precision NOT NULL DEFAULT 0;"#)
        .execute(pool)
        .await
        .context("altering telemetry.available_discharge_kw")?;
    sqlx::query(r#"ALTER TABLE telemetry ADD COLUMN IF NOT EXISTS extras jsonb NOT NULL DEFAULT '{}'::jsonb;"#)
        .execute(pool)
        .await
        .context("altering telemetry.extras")?;

    try_setup_timescale(pool).await;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS agent_sessions (
            asset_id uuid NOT NULL,
            peer text NOT NULL,
            asset_name text,
            site_name text,
            connected_at timestamptz NOT NULL,
            disconnected_at timestamptz
        );
    "#,
    )
    .execute(pool)
    .await
    .context("creating agent_sessions table")?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS dispatches (
            id uuid PRIMARY KEY,
            asset_id uuid NOT NULL,
            mw double precision NOT NULL,
            duration_s bigint,
            status text NOT NULL,
            reason text,
            submitted_at timestamptz NOT NULL,
            clamped boolean NOT NULL DEFAULT false,
            ack_status text,
            acked_at timestamptz,
            ack_reason text
        );
    "#,
    )
    .execute(pool)
    .await
    .context("creating dispatches table")?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS heartbeats (
            asset_id uuid NOT NULL,
            ts timestamptz NOT NULL
        );
    "#,
    )
    .execute(pool)
    .await
    .context("creating heartbeats table")?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS events (
            id uuid PRIMARY KEY,
            asset_id uuid NOT NULL,
            site_id uuid NOT NULL,
            ts timestamptz NOT NULL,
            event_type text NOT NULL,
            severity text NOT NULL,
            message text NOT NULL
        );
        "#,
    )
    .execute(pool)
    .await
    .context("creating events table")?;
    Ok(())
}

pub async fn persist_telemetry(pool: &PgPool, snaps: &[Telemetry]) -> Result<()> {
    // Write each snapshot into the telemetry table.
    for snap in snaps {
        sqlx::query(
            r#"
            INSERT INTO telemetry (
                asset_id, site_id, ts, soc_mwhr, soc_pct,
                capacity_mwhr, current_mw, setpoint_mw, max_mw, min_mw, status,
                voltage_v, current_a, dc_bus_v, dc_bus_a,
                temperature_cell_f, temperature_module_f, temperature_ambient_f,
                soh_pct, cycle_count, energy_in_mwh, energy_out_mwh,
                available_charge_kw, available_discharge_kw, extras
            ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23,$24,$25)
        "#,
        )
        .bind(snap.asset_id)
        .bind(snap.site_id)
        .bind(snap.timestamp)
        .bind(snap.soc_mwhr)
        .bind(snap.soc_pct)
        .bind(snap.capacity_mwhr)
        .bind(snap.current_mw)
        .bind(snap.setpoint_mw)
        .bind(snap.max_mw)
        .bind(snap.min_mw)
        .bind(&snap.status)
        .bind(snap.voltage_v)
        .bind(snap.current_a)
        .bind(snap.dc_bus_v)
        .bind(snap.dc_bus_a)
        .bind(snap.temperature_cell_f)
        .bind(snap.temperature_module_f)
        .bind(snap.temperature_ambient_f)
        .bind(snap.soh_pct)
        .bind(snap.cycle_count as i64)
        .bind(snap.energy_in_mwh)
        .bind(snap.energy_out_mwh)
        .bind(snap.available_charge_kw)
        .bind(snap.available_discharge_kw)
        .bind(Json(&snap.extras))
        .execute(pool)
        .await
        .context("inserting telemetry row")?;
    }
    Ok(())
}

pub async fn persist_assets(pool: &PgPool, assets: &[Asset]) -> Result<()> {
    // Upsert asset metadata so history queries can join for names.
    for asset in assets {
        sqlx::query(
            r#"
            INSERT INTO assets (
                id, site_id, name, site_name, location, capacity_mwhr,
                max_mw, min_mw, efficiency, ramp_rate_mw_per_min
            ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)
            ON CONFLICT (id) DO UPDATE SET
                site_id = EXCLUDED.site_id,
                name = EXCLUDED.name,
                site_name = EXCLUDED.site_name,
                location = EXCLUDED.location,
                capacity_mwhr = EXCLUDED.capacity_mwhr,
                max_mw = EXCLUDED.max_mw,
                min_mw = EXCLUDED.min_mw,
                efficiency = EXCLUDED.efficiency,
                ramp_rate_mw_per_min = EXCLUDED.ramp_rate_mw_per_min
        "#,
        )
        .bind(asset.id)
        .bind(asset.site_id)
        .bind(&asset.name)
        .bind(&asset.site_name)
        .bind(&asset.location)
        .bind(asset.capacity_mwhr)
        .bind(asset.max_mw)
        .bind(asset.min_mw)
        .bind(asset.efficiency)
        .bind(asset.ramp_rate_mw_per_min)
        .execute(pool)
        .await
        .context("upserting asset row")?;
    }
    Ok(())
}

pub async fn persist_dispatch(pool: &PgPool, d: &Dispatch) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO dispatches (id, asset_id, mw, duration_s, status, reason, submitted_at, clamped, ack_status, acked_at, ack_reason)
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)
    "#,
    )
    .bind(d.id)
    .bind(d.asset_id)
    .bind(d.mw)
    .bind(d.duration_s.map(|v| v as i64))
    .bind(&d.status)
    .bind(&d.reason)
    .bind(d.submitted_at)
    .bind(d.clamped)
    .bind::<Option<String>>(None)
    .bind::<Option<DateTime<Utc>>>(None)
    .bind::<Option<String>>(None)
    .execute(pool)
    .await
    .context("inserting dispatch row")?;
    Ok(())
}

pub async fn persist_event(pool: &PgPool, event: &BessEvent) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO events (id, asset_id, site_id, ts, event_type, severity, message)
        VALUES ($1,$2,$3,$4,$5,$6,$7)
    "#,
    )
    .bind(event.id)
    .bind(event.asset_id)
    .bind(event.site_id)
    .bind(event.timestamp)
    .bind(&event.event_type)
    .bind(&event.severity)
    .bind(&event.message)
    .execute(pool)
    .await
    .context("inserting event row")?;
    Ok(())
}

pub async fn persist_heartbeat(pool: &PgPool, asset_id: Uuid, ts: DateTime<Utc>) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO heartbeats (asset_id, ts)
        VALUES ($1,$2)
    "#,
    )
    .bind(asset_id)
    .bind(ts)
    .execute(pool)
    .await
    .context("inserting heartbeat row")?;
    Ok(())
}

pub async fn record_agent_connect(
    db: &PgPool,
    asset_id: Uuid,
    peer: &str,
    asset_name: &str,
    site_name: &str,
    ts: DateTime<Utc>,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO agent_sessions (asset_id, peer, asset_name, site_name, connected_at)
        VALUES ($1,$2,$3,$4,$5)
    "#,
    )
    .bind(asset_id)
    .bind(peer)
    .bind(asset_name)
    .bind(site_name)
    .bind(ts)
    .execute(db)
    .await
    .context("inserting agent session")?;
    Ok(())
}

pub async fn record_agent_disconnect(db: &PgPool, asset_id: Uuid) -> Result<()> {
    sqlx::query(
        r#"
        UPDATE agent_sessions
        SET disconnected_at = NOW()
        WHERE asset_id = $1 AND disconnected_at IS NULL
    "#,
    )
    .bind(asset_id)
    .execute(db)
    .await
    .context("updating agent session")?;
    Ok(())
}

async fn try_setup_timescale(pool: &PgPool) {
    if let Err(err) = sqlx::query(r#"CREATE EXTENSION IF NOT EXISTS timescaledb;"#)
        .execute(pool)
        .await
    {
        warn!("timescaledb extension unavailable: {err}");
        return;
    }

    if let Err(err) = sqlx::query(
        r#"
        SELECT create_hypertable('telemetry', 'ts', if_not_exists => TRUE);
        "#,
    )
    .execute(pool)
    .await
    {
        warn!("failed to convert telemetry to hypertable: {err}");
        return;
    }

    if let Err(err) = sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS telemetry_asset_ts_idx
        ON telemetry (asset_id, ts DESC);
        "#,
    )
    .execute(pool)
    .await
    {
        warn!("failed to create telemetry index: {err}");
    }
}

fn env_truthy(name: &str) -> bool {
    std::env::var(name)
        .map(|v| {
            let v = v.to_lowercase();
            v == "1" || v == "true" || v == "yes" || v == "on"
        })
        .unwrap_or(false)
}
