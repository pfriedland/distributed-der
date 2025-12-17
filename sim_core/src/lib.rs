//! Shared simulation types and logic for BESS assets.
//! Keep this crate free of HTTP/SQL deps so both headend and agents can reuse it.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Asset {
    pub id: Uuid,
    pub site_id: Uuid,
    pub site_name: String,
    pub name: String,
    pub location: String,
    pub capacity_mwhr: f64,
    pub max_mw: f64,
    pub min_mw: f64,
    pub min_soc_pct: f64,
    pub max_soc_pct: f64,
    pub efficiency: f64,
    pub ramp_rate_mw_per_min: f64,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct BessState {
    pub soc_mwhr: f64,
    pub current_mw: f64,
    pub setpoint_mw: f64,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Telemetry {
    pub asset_id: Uuid,
    pub site_id: Uuid,
    pub site_name: String,
    pub timestamp: DateTime<Utc>,
    pub soc_mwhr: f64,
    pub soc_pct: f64,
    pub capacity_mwhr: f64,
    pub current_mw: f64,
    pub setpoint_mw: f64,
    pub max_mw: f64,
    pub min_mw: f64,
    pub status: String,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Dispatch {
    pub id: Uuid,
    pub asset_id: Uuid,
    pub mw: f64, // positive = discharge, negative = charge
    pub duration_s: Option<u64>,
    pub status: String,
    pub reason: Option<String>,
    pub submitted_at: DateTime<Utc>,
    pub clamped: bool,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct DispatchRequest {
    pub asset_id: Uuid,
    pub mw: f64, // positive = discharge, negative = charge
    pub duration_s: Option<u64>,
    pub site_id: Option<Uuid>,
    pub clamped: bool,
}

/// Advance a single asset state by dt_secs. Returns a telemetry snapshot.
pub fn tick_asset(asset: &Asset, state: &mut BessState, dt_secs: f64) -> Telemetry {
    // Ramp toward setpoint (respecting ramp rate and min/max).
    let ramp_per_sec = asset.ramp_rate_mw_per_min / 60.0;
    let target = state.setpoint_mw;
    let delta = target - state.current_mw;
    let max_delta = ramp_per_sec * dt_secs;
    let applied_delta = delta.clamp(-max_delta, max_delta);
    state.current_mw = (state.current_mw + applied_delta).clamp(asset.min_mw, asset.max_mw);

    // Update SOC with a simple linear model.
    // Positive MW means discharge (SOC decreases); negative MW charge (SOC increases).
    let energy_delta_mwh = state.current_mw * dt_secs / 3600.0;
    let eff = asset.efficiency;
    let adjusted = if energy_delta_mwh >= 0.0 {
        energy_delta_mwh / eff
    } else {
        energy_delta_mwh * eff
    };
    let (min_soc_mwhr, max_soc_mwhr) = soc_bounds(asset);
    state.soc_mwhr = (state.soc_mwhr - adjusted).clamp(min_soc_mwhr, max_soc_mwhr);

    // If we are at a SOC boundary and still moving in the wrong direction,
    // zero the setpoint/current MW to stop dispatch.
    let eps = 1e-6;
    if state.soc_mwhr <= min_soc_mwhr + eps && state.current_mw > 0.0 {
        state.setpoint_mw = 0.0;
        state.current_mw = 0.0;
    } else if state.soc_mwhr >= max_soc_mwhr - eps && state.current_mw < 0.0 {
        state.setpoint_mw = 0.0;
        state.current_mw = 0.0;
    }

    let status = if state.current_mw > 0.1 {
        "discharging"
    } else if state.current_mw < -0.1 {
        "charging"
    } else {
        "idle"
    };

    Telemetry {
        asset_id: asset.id,
        site_id: asset.site_id,
        site_name: asset.site_name.clone(),
        timestamp: Utc::now(),
        soc_mwhr: state.soc_mwhr,
        soc_pct: if asset.capacity_mwhr > 0.0 {
            (state.soc_mwhr / asset.capacity_mwhr * 100.0).clamp(0.0, 100.0)
        } else {
            0.0
        },
        capacity_mwhr: asset.capacity_mwhr,
        current_mw: state.current_mw,
        setpoint_mw: state.setpoint_mw,
        max_mw: asset.max_mw,
        min_mw: asset.min_mw,
        status: status.to_string(),
    }
}

fn soc_bounds(asset: &Asset) -> (f64, f64) {
    let cap = asset.capacity_mwhr.max(0.0);
    let min_pct = asset.min_soc_pct.clamp(0.0, 100.0);
    let max_pct = asset.max_soc_pct.clamp(0.0, 100.0);
    let min_mwhr = cap * min_pct / 100.0;
    let max_mwhr = cap * max_pct / 100.0;
    if min_mwhr <= max_mwhr {
        (min_mwhr, max_mwhr)
    } else {
        (0.0, cap)
    }
}
