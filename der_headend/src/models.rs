use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

#[derive(FromRow)]
pub struct TelemetrySeed {
    pub asset_id: Uuid,
    pub soc_mwhr: f64,
    pub current_mw: f64,
    pub setpoint_mw: f64,
}

#[derive(Debug, Deserialize)]
pub struct AssetsFile {
    pub sites: Vec<SiteCfg>,
    pub assets: Vec<AssetCfg>,
}

#[derive(Debug, Deserialize)]
pub struct SiteCfg {
    pub id: Uuid,
    pub name: String,
    pub location: String,
}

#[derive(Debug, Deserialize)]
pub struct AssetCfg {
    pub id: Uuid,
    pub site_id: Uuid,
    pub name: String,
    pub capacity_mwhr: f64,
    pub max_mw: f64,
    pub min_mw: f64,
    #[serde(default = "default_min_soc_pct")]
    pub min_soc_pct: f64,
    #[serde(default = "default_max_soc_pct")]
    pub max_soc_pct: f64,
    pub efficiency: f64,
    pub ramp_rate_mw_per_min: f64,
}

pub fn default_min_soc_pct() -> f64 {
    0.0
}

pub fn default_max_soc_pct() -> f64 {
    100.0
}

#[derive(Serialize, FromRow)]
pub struct TelemetryRow {
    pub ts: DateTime<Utc>,
    pub asset_name: String,
    pub site_name: String,
    pub soc_mwhr: f64,
    pub soc_pct: f64,
    pub capacity_mwhr: f64,
    pub current_mw: f64,
    pub setpoint_mw: f64,
    pub max_mw: f64,
    pub min_mw: f64,
    pub status: String,
    pub voltage_v: f64,
    pub current_a: f64,
    pub dc_bus_v: f64,
    pub dc_bus_a: f64,
    pub temperature_cell_f: f64,
    pub temperature_module_f: f64,
    pub temperature_ambient_f: f64,
    pub soh_pct: f64,
    pub cycle_count: i64,
    pub energy_in_mwh: f64,
    pub energy_out_mwh: f64,
    pub available_charge_kw: f64,
    pub available_discharge_kw: f64,
    pub asset_id: Uuid,
    pub site_id: Uuid,
}

#[derive(FromRow)]
pub struct TelemetrySnapshotRow {
    pub asset_id: Uuid,
    pub site_id: Uuid,
    pub site_name: String,
    pub ts: DateTime<Utc>,
    pub soc_mwhr: f64,
    pub soc_pct: f64,
    pub capacity_mwhr: f64,
    pub current_mw: f64,
    pub setpoint_mw: f64,
    pub max_mw: f64,
    pub min_mw: f64,
    pub status: String,
    pub voltage_v: f64,
    pub current_a: f64,
    pub dc_bus_v: f64,
    pub dc_bus_a: f64,
    pub temperature_cell_f: f64,
    pub temperature_module_f: f64,
    pub temperature_ambient_f: f64,
    pub soh_pct: f64,
    pub cycle_count: i64,
    pub energy_in_mwh: f64,
    pub energy_out_mwh: f64,
    pub available_charge_kw: f64,
    pub available_discharge_kw: f64,
}

#[derive(FromRow)]
pub struct DispatchSnapshotRow {
    pub id: Uuid,
    pub asset_id: Uuid,
    pub mw: f64,
    pub duration_s: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct TimeRange {
    pub start: Option<String>,
    pub end: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct IncomingDispatch {
    pub asset_id: Option<String>,
    pub site_id: Option<String>,
    pub mw: f64,
    pub duration_s: Option<u64>,
}

#[derive(Serialize, FromRow)]
pub struct HeartbeatRow {
    pub asset_id: Uuid,
    pub ts: DateTime<Utc>,
    pub asset_name: String,
    pub site_name: String,
}

#[derive(Serialize)]
pub struct AssetView {
    pub available_charge_kw: Option<f64>,
    pub available_discharge_kw: Option<f64>,
    pub capacity_mwhr: f64,
    pub current_a: Option<f64>,
    pub current_mw: Option<f64>,
    pub cycle_count: Option<u64>,
    pub dc_bus_a: Option<f64>,
    pub dc_bus_v: Option<f64>,
    pub efficiency: f64,
    pub energy_in_mwh: Option<f64>,
    pub energy_out_mwh: Option<f64>,
    pub id: Uuid,
    pub location: String,
    pub max_mw: f64,
    pub max_soc_pct: f64,
    pub min_mw: f64,
    pub min_soc_pct: f64,
    pub name: String,
    pub ramp_rate_mw_per_min: f64,
    pub setpoint_mw: Option<f64>,
    pub soh_pct: Option<f64>,
    pub soc_mwhr: Option<f64>,
    pub soc_pct: Option<f64>,
    pub site_id: Uuid,
    pub site_name: String,
    pub status: Option<String>,
    pub temperature_ambient_f: Option<f64>,
    pub temperature_cell_f: Option<f64>,
    pub temperature_module_f: Option<f64>,
    pub voltage_v: Option<f64>,
}

#[derive(Serialize)]
pub struct SiteView {
    pub site_id: Uuid,
    pub site_name: String,
    pub location: String,
    pub aggregate: SiteAggregate,
    pub assets: Vec<AssetView>,
}

#[derive(Serialize)]
pub struct SiteAggregate {
    pub asset_count: usize,
    pub capacity_mwhr: f64,
    pub max_mw: f64,
    pub min_mw: f64,
    pub min_soc_pct: f64,
    pub max_soc_pct: f64,
    pub efficiency: f64,
    pub ramp_rate_mw_per_min: f64,
    pub soc_mwhr: Option<f64>,
    pub soc_pct: Option<f64>,
    pub current_mw: Option<f64>,
    pub setpoint_mw: Option<f64>,
}

#[derive(Default)]
pub struct SiteAggregateBuilder {
    asset_count: usize,
    capacity_sum: f64,
    max_mw_sum: f64,
    min_mw_sum: f64,
    weight_sum: f64,
    min_soc_weighted: f64,
    max_soc_weighted: f64,
    efficiency_weighted: f64,
    ramp_weighted: f64,
    min_soc_sum: f64,
    max_soc_sum: f64,
    efficiency_sum: f64,
    ramp_sum: f64,
    soc_mwhr_sum: f64,
    soc_cap_sum: f64,
    has_soc: bool,
    current_mw_sum: f64,
    current_count: usize,
    setpoint_mw_sum: f64,
    setpoint_count: usize,
}

impl SiteAggregateBuilder {
    pub fn add_asset(&mut self, asset: &AssetView) {
        let cap = asset.capacity_mwhr.max(0.0);
        self.asset_count += 1;
        self.capacity_sum += cap;
        self.max_mw_sum += asset.max_mw;
        self.min_mw_sum += asset.min_mw;

        self.min_soc_sum += asset.min_soc_pct;
        self.max_soc_sum += asset.max_soc_pct;
        self.efficiency_sum += asset.efficiency;
        self.ramp_sum += asset.ramp_rate_mw_per_min;

        if cap > 0.0 {
            self.weight_sum += cap;
            self.min_soc_weighted += asset.min_soc_pct * cap;
            self.max_soc_weighted += asset.max_soc_pct * cap;
            self.efficiency_weighted += asset.efficiency * cap;
            self.ramp_weighted += asset.ramp_rate_mw_per_min * cap;
        }

        if let Some(soc_mwhr) = asset.soc_mwhr {
            self.soc_mwhr_sum += soc_mwhr;
            self.soc_cap_sum += cap;
            self.has_soc = true;
        }
        if let Some(current_mw) = asset.current_mw {
            self.current_mw_sum += current_mw;
            self.current_count += 1;
        }
        if let Some(setpoint_mw) = asset.setpoint_mw {
            self.setpoint_mw_sum += setpoint_mw;
            self.setpoint_count += 1;
        }
    }

    pub fn finish(self) -> SiteAggregate {
        let avg_or_weighted = |weighted_sum: f64, plain_sum: f64, weight: f64, count: usize| {
            if weight > 0.0 {
                weighted_sum / weight
            } else if count > 0 {
                plain_sum / count as f64
            } else {
                0.0
            }
        };

        SiteAggregate {
            asset_count: self.asset_count,
            capacity_mwhr: self.capacity_sum,
            max_mw: self.max_mw_sum,
            min_mw: self.min_mw_sum,
            min_soc_pct: avg_or_weighted(
                self.min_soc_weighted,
                self.min_soc_sum,
                self.weight_sum,
                self.asset_count,
            ),
            max_soc_pct: avg_or_weighted(
                self.max_soc_weighted,
                self.max_soc_sum,
                self.weight_sum,
                self.asset_count,
            ),
            efficiency: avg_or_weighted(
                self.efficiency_weighted,
                self.efficiency_sum,
                self.weight_sum,
                self.asset_count,
            ),
            ramp_rate_mw_per_min: avg_or_weighted(
                self.ramp_weighted,
                self.ramp_sum,
                self.weight_sum,
                self.asset_count,
            ),
            soc_mwhr: self.has_soc.then_some(self.soc_mwhr_sum),
            soc_pct: if self.has_soc && self.soc_cap_sum > 0.0 {
                Some(self.soc_mwhr_sum / self.soc_cap_sum * 100.0)
            } else {
                None
            },
            current_mw: (self.current_count > 0).then_some(self.current_mw_sum),
            setpoint_mw: (self.setpoint_count > 0).then_some(self.setpoint_mw_sum),
        }
    }
}

pub struct SiteAccumulator {
    pub site_id: Uuid,
    pub site_name: String,
    pub location: String,
    pub assets: Vec<AssetView>,
    pub aggregate: SiteAggregateBuilder,
}
