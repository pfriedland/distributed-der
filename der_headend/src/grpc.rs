use std::{collections::HashMap, pin::Pin};

use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use sqlx::PgPool;
use tokio::sync::mpsc;
use tokio_stream::{StreamExt as TokioStreamExt, wrappers::ReceiverStream};
use tonic::{Request, Response as GrpcResponse, Status};
use uuid::Uuid;

use sim_core::{Asset, BessEvent, BessState, Dispatch, Telemetry, TelemetryValue, tick_asset};

use crate::{
    AppState,
    soc_bounds,
    proto::{
        AgentToHeadend, AssetBootstrap, BootstrapRequest, BootstrapResponse, DispatchAck,
        Event as ProtoEvent, HeadendToAgent, Register, Setpoint,
        agent_link_server::AgentLink,
        agent_to_headend, headend_to_agent,
    },
};
use crate::db::{
    persist_event, persist_heartbeat, persist_telemetry, record_agent_connect, record_agent_disconnect,
};
use crate::models::{DispatchSnapshotRow, TelemetrySnapshotRow};

/// Connection info for a live agent stream.
#[derive(Clone)]
pub(crate) struct AgentStream {
    pub(crate) tx: mpsc::Sender<HeadendToAgent>,
    pub(crate) peer: String,
    pub(crate) asset_name: String,
    pub(crate) site_name: String,
    pub(crate) site_id: Uuid,
    pub(crate) connected_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SocState {
    BelowMin,
    InRange,
    AboveMax,
}

#[derive(Clone)]
pub(crate) struct GrpcApi {
    pub(crate) state: AppState,
}

#[tonic::async_trait]
impl AgentLink for GrpcApi {
    type StreamStream =
        Pin<Box<dyn futures_util::Stream<Item = Result<HeadendToAgent, Status>> + Send>>;

    async fn bootstrap(
        &self,
        request: Request<BootstrapRequest>,
    ) -> Result<GrpcResponse<BootstrapResponse>, Status> {
        let req = request.into_inner();
        let mut asset_ids = Vec::new();
        for id in req.asset_ids {
            match Uuid::parse_str(&id) {
                Ok(uuid) => asset_ids.push(uuid),
                Err(_) => {
                    return Err(Status::invalid_argument(format!(
                        "invalid asset_id {}",
                        id
                    )));
                }
            }
        }
        let resp = build_bootstrap_response(&self.state, &asset_ids)
            .await
            .map_err(|e| Status::internal(format!("bootstrap failed: {e}")))?;
        Ok(GrpcResponse::new(resp))
    }

    async fn stream(
        &self,
        request: Request<tonic::Streaming<AgentToHeadend>>,
    ) -> Result<GrpcResponse<Self::StreamStream>, Status> {
        let peer_ip = request
            .remote_addr()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "<unknown>".into());

        let mut inbound = request.into_inner();

        // Each connection gets its own channel for outbound setpoints.
        let (tx, rx) = mpsc::channel::<HeadendToAgent>(32);

        let state = self.state.clone();
        tokio::spawn(async move {
            let mut asset_ids: Vec<(Uuid, String, String)> = Vec::new(); // (asset_id, asset_name, site_name)
            while let Some(msg) = TokioStreamExt::next(&mut inbound).await {
                match msg {
                    Ok(AgentToHeadend {
                        msg:
                            Some(agent_to_headend::Msg::Register(Register {
                                asset_id: id,
                                site_id,
                                asset_name,
                                site_name,
                                assets,
                                ..
                            })),
                    }) => {
                        asset_ids.clear();

                        // Prefer multi-asset registrations; fall back to legacy single-asset fields.
                        let mut registered_any = false;
                        for desc in assets {
                            if let Ok(uuid) = Uuid::parse_str(&desc.asset_id) {
                                let asset_name = if desc.asset_name.is_empty() {
                                    asset_name.clone()
                                } else {
                                    desc.asset_name.clone()
                                };
                                let site_name = if desc.site_name.is_empty() {
                                    site_name.clone()
                                } else {
                                    desc.site_name.clone()
                                };
                                let site_uuid = if !desc.site_id.is_empty() {
                                    Uuid::parse_str(&desc.site_id).unwrap_or_else(|_| Uuid::nil())
                                } else {
                                    Uuid::parse_str(&site_id).unwrap_or_else(|_| Uuid::nil())
                                };

                                let connected_at = Utc::now();
                                state.agent_streams.write().await.insert(
                                    uuid,
                                    AgentStream {
                                        tx: tx.clone(),
                                        peer: peer_ip.clone(),
                                        asset_name: asset_name.clone(),
                                        site_name: site_name.clone(),
                                        site_id: site_uuid,
                                        connected_at,
                                    },
                                );
                                asset_ids.push((uuid, asset_name.clone(), site_name.clone()));
                                registered_any = true;

                                tracing::info!(
                                    "agent connected asset_id={} asset_name={} site_name={} peer={}",
                                    uuid,
                                    asset_name,
                                    site_name,
                                    peer_ip,
                                );

                                if let Some(db) = state.db.as_ref() {
                                    if let Err(err) = record_agent_connect(
                                        db,
                                        uuid,
                                        &peer_ip,
                                        &asset_name,
                                        &site_name,
                                        connected_at,
                                    )
                                    .await
                                    {
                                        tracing::warn!("failed to record agent connect: {err}");
                                    }
                                }

                                if let Some(pending) =
                                    state.pending_setpoints.write().await.remove(&uuid)
                                {
                                    let _ = tx
                                        .send(HeadendToAgent {
                                            msg: Some(headend_to_agent::Msg::Setpoint(Setpoint {
                                                asset_id: pending.asset_id.to_string(),
                                                mw: pending.mw,
                                                duration_s: pending.duration_s.map(|d| d as u64),
                                                site_id: Some(site_uuid.to_string()),
                                                group_id: None,
                                                dispatch_id: Some(pending.id.to_string()),
                                            })),
                                        })
                                        .await;
                                }
                            }
                        }

                        // Legacy single-asset registration.
                        if !registered_any {
                            if let Ok(uuid) = Uuid::parse_str(&id) {
                                let site_uuid =
                                    Uuid::parse_str(&site_id).unwrap_or_else(|_| Uuid::nil());
                                let connected_at = Utc::now();
                                state.agent_streams.write().await.insert(
                                    uuid,
                                    AgentStream {
                                        tx: tx.clone(),
                                        peer: peer_ip.clone(),
                                        asset_name: asset_name.clone(),
                                        site_name: site_name.clone(),
                                        site_id: site_uuid,
                                        connected_at,
                                    },
                                );
                                asset_ids.push((uuid, asset_name.clone(), site_name.clone()));

                                tracing::info!(
                                    "agent connected asset_id={} asset_name={} site_name={} peer={}",
                                    uuid,
                                    asset_name,
                                    site_name,
                                    peer_ip,
                                );

                                if let Some(db) = state.db.as_ref() {
                                    if let Err(err) = record_agent_connect(
                                        db,
                                        uuid,
                                        &peer_ip,
                                        &asset_name,
                                        &site_name,
                                        connected_at,
                                    )
                                    .await
                                    {
                                        tracing::warn!("failed to record agent connect: {err}");
                                    }
                                }

                                if let Some(pending) =
                                    state.pending_setpoints.write().await.remove(&uuid)
                                {
                                    let _ = tx
                                        .send(HeadendToAgent {
                                            msg: Some(headend_to_agent::Msg::Setpoint(Setpoint {
                                                asset_id: pending.asset_id.to_string(),
                                                mw: pending.mw,
                                                duration_s: pending.duration_s.map(|d| d as u64),
                                                site_id: Some(site_uuid.to_string()),
                                                group_id: None,
                                                dispatch_id: Some(pending.id.to_string()),
                                            })),
                                        })
                                        .await;
                                }
                            }
                        }
                    }
                    Ok(AgentToHeadend {
                        msg: Some(agent_to_headend::Msg::Telemetry(t)),
                    }) => {
                        if let Err(err) = handle_agent_telemetry(&state, t).await {
                            tracing::warn!("telemetry ingest failed: {err}");
                        }
                    }
                    Ok(AgentToHeadend {
                        msg: Some(agent_to_headend::Msg::Heartbeat(hb)),
                    }) => {
                        if let Err(err) = handle_agent_heartbeat(&state, hb).await {
                            tracing::warn!("heartbeat handling failed: {err}");
                        }
                    }
                    Ok(AgentToHeadend {
                        msg: Some(agent_to_headend::Msg::DispatchAck(ack)),
                    }) => {
                        if let Err(err) = handle_dispatch_ack(&state, ack).await {
                            tracing::warn!("dispatch ack handling failed: {err}");
                        }
                    }
                    Ok(AgentToHeadend {
                        msg: Some(agent_to_headend::Msg::Event(evt)),
                    }) => {
                        if let Err(err) = handle_agent_event(&state, evt).await {
                            tracing::warn!("event handling failed: {err}");
                        }
                    }
                    Ok(_) => {}
                    Err(err) => {
                        let primary = asset_ids.get(0).map(|(id, _, _)| *id);
                        tracing::info!(
                            "agent stream closed/errored (asset={:?}, peer={}): {err}",
                            primary,
                            peer_ip
                        );
                        break;
                    }
                }
            }

            // Clean up on disconnect.
            if !asset_ids.is_empty() {
                for (id, _asset_name, _site_name) in asset_ids {
                    if let Some(agent) = state.agent_streams.write().await.remove(&id) {
                        tracing::info!(
                            "agent disconnected asset_id={} asset_name={} site_name={} peer={}",
                            id,
                            agent.asset_name,
                            agent.site_name,
                            peer_ip
                        );
                    } else {
                        tracing::info!("agent disconnected asset_id={} peer={}", id, peer_ip);
                    }
                    if let Some(db) = state.db.as_ref() {
                        if let Err(err) = record_agent_disconnect(db, id).await {
                            tracing::warn!("failed to record agent disconnect: {err}");
                        }
                    }
                }
            }
        });

        let outbound = ReceiverStream::new(rx).map(Ok);
        Ok(GrpcResponse::new(Box::pin(outbound)))
    }
}

async fn handle_agent_telemetry(state: &AppState, t: crate::proto::Telemetry) -> Result<()> {
    let asset_id = Uuid::parse_str(&t.asset_id)?;
    let site_id = Uuid::parse_str(&t.site_id)?;
    let ts = DateTime::parse_from_rfc3339(&t.timestamp)
        .map(|dt| dt.with_timezone(&Utc))
        .context("parsing telemetry timestamp")?;

    let snap = Telemetry {
        asset_id,
        site_id,
        site_name: t.site_name.clone(),
        timestamp: ts,
        soc_mwhr: t.soc_mwhr,
        soc_pct: t.soc_pct,
        capacity_mwhr: t.capacity_mwhr,
        current_mw: t.current_mw,
        setpoint_mw: t.setpoint_mw,
        max_mw: t.max_mw,
        min_mw: t.min_mw,
        status: t.status.clone(),
        voltage_v: t.voltage_v,
        current_a: t.current_a,
        dc_bus_v: t.dc_bus_v,
        dc_bus_a: t.dc_bus_a,
        temperature_cell_f: t.temperature_cell_f,
        temperature_module_f: t.temperature_module_f,
        temperature_ambient_f: t.temperature_ambient_f,
        soh_pct: t.soh_pct,
        cycle_count: t.cycle_count,
        energy_in_mwh: t.energy_in_mwh,
        energy_out_mwh: t.energy_out_mwh,
        available_charge_kw: t.available_charge_kw,
        available_discharge_kw: t.available_discharge_kw,
        extras: proto_extras_to_map(&t.extras),
    };

    {
        let mut latest = state.latest.write().await;
        latest.insert(asset_id, snap.clone());
    }

    if let Some(db) = state.db.as_ref() {
        persist_telemetry(db, &[snap.clone()]).await?;
        maybe_emit_soc_event(state, db, &snap).await?;
    }

    Ok(())
}

fn telemetry_to_proto(snap: &Telemetry) -> crate::proto::Telemetry {
    crate::proto::Telemetry {
        asset_id: snap.asset_id.to_string(),
        site_id: snap.site_id.to_string(),
        site_name: snap.site_name.clone(),
        timestamp: snap
            .timestamp
            .to_rfc3339_opts(SecondsFormat::Millis, true),
        soc_mwhr: snap.soc_mwhr,
        soc_pct: snap.soc_pct,
        capacity_mwhr: snap.capacity_mwhr,
        current_mw: snap.current_mw,
        setpoint_mw: snap.setpoint_mw,
        max_mw: snap.max_mw,
        min_mw: snap.min_mw,
        status: snap.status.clone(),
        voltage_v: snap.voltage_v,
        current_a: snap.current_a,
        dc_bus_v: snap.dc_bus_v,
        dc_bus_a: snap.dc_bus_a,
        temperature_cell_f: snap.temperature_cell_f,
        temperature_module_f: snap.temperature_module_f,
        temperature_ambient_f: snap.temperature_ambient_f,
        soh_pct: snap.soh_pct,
        cycle_count: snap.cycle_count,
        energy_in_mwh: snap.energy_in_mwh,
        energy_out_mwh: snap.energy_out_mwh,
        available_charge_kw: snap.available_charge_kw,
        available_discharge_kw: snap.available_discharge_kw,
        extras: snap
            .extras
            .iter()
            .filter_map(|(key, value)| telemetry_value_to_proto(value).map(|v| (key.clone(), v)))
            .collect(),
    }
}

fn setpoint_from_dispatch(dispatch: &Dispatch, asset: &Asset) -> Setpoint {
    Setpoint {
        asset_id: dispatch.asset_id.to_string(),
        mw: dispatch.mw,
        duration_s: dispatch.duration_s.map(|v| v as u64),
        site_id: Some(asset.site_id.to_string()),
        group_id: None,
        dispatch_id: Some(dispatch.id.to_string()),
    }
}

fn setpoint_from_state(asset: &Asset, state: &BessState) -> Option<Setpoint> {
    if state.setpoint_mw.abs() <= f64::EPSILON {
        return None;
    }
    Some(Setpoint {
        asset_id: asset.id.to_string(),
        mw: state.setpoint_mw,
        duration_s: None,
        site_id: Some(asset.site_id.to_string()),
        group_id: None,
        dispatch_id: None,
    })
}

async fn build_bootstrap_response(
    state: &AppState,
    asset_ids: &[Uuid],
) -> Result<BootstrapResponse> {
    let mut telemetry_map: HashMap<Uuid, Telemetry> = HashMap::new();
    {
        let latest = state.latest.read().await;
        for id in asset_ids {
            if let Some(snap) = latest.get(id) {
                telemetry_map.insert(*id, snap.clone());
            }
        }
    }

    if let Some(db) = state.db.as_ref() {
        let missing: Vec<Uuid> = asset_ids
            .iter()
            .filter(|id| !telemetry_map.contains_key(id))
            .cloned()
            .collect();
        if !missing.is_empty() {
            let rows = sqlx::query_as::<_, TelemetrySnapshotRow>(
                r#"
                SELECT DISTINCT ON (asset_id)
                    asset_id,
                    site_id,
                    site_name,
                    ts,
                    soc_mwhr,
                    soc_pct,
                    capacity_mwhr,
                    current_mw,
                    setpoint_mw,
                    max_mw,
                    min_mw,
                    status,
                    voltage_v,
                    current_a,
                    dc_bus_v,
                    dc_bus_a,
                    temperature_cell_f,
                    temperature_module_f,
                    temperature_ambient_f,
                    soh_pct,
                    cycle_count,
                    energy_in_mwh,
                    energy_out_mwh,
                    available_charge_kw,
                    available_discharge_kw,
                    extras
                FROM telemetry
                WHERE asset_id = ANY($1)
                ORDER BY asset_id, ts DESC
                "#,
            )
            .bind(&missing)
            .fetch_all(db)
            .await
            .context("querying latest telemetry for bootstrap")?;
            for row in rows {
                telemetry_map.insert(
                    row.asset_id,
                    Telemetry {
                        asset_id: row.asset_id,
                        site_id: row.site_id,
                        site_name: row.site_name,
                        timestamp: row.ts,
                        soc_mwhr: row.soc_mwhr,
                        soc_pct: row.soc_pct,
                        capacity_mwhr: row.capacity_mwhr,
                        current_mw: row.current_mw,
                        setpoint_mw: row.setpoint_mw,
                        max_mw: row.max_mw,
                        min_mw: row.min_mw,
                        status: row.status,
                        voltage_v: row.voltage_v,
                        current_a: row.current_a,
                        dc_bus_v: row.dc_bus_v,
                        dc_bus_a: row.dc_bus_a,
                        temperature_cell_f: row.temperature_cell_f,
                        temperature_module_f: row.temperature_module_f,
                        temperature_ambient_f: row.temperature_ambient_f,
                        soh_pct: row.soh_pct,
                        cycle_count: row.cycle_count as u64,
                        energy_in_mwh: row.energy_in_mwh,
                        energy_out_mwh: row.energy_out_mwh,
                        available_charge_kw: row.available_charge_kw,
                        available_discharge_kw: row.available_discharge_kw,
                        extras: row.extras.0,
                    },
                );
            }
        }
    }

    let mut setpoint_map: HashMap<Uuid, Setpoint> = HashMap::new();
    let sim = state.sim.read().await;
    for id in asset_ids {
        if let Some(dispatch) = sim.dispatches.get(id) {
            if let Some(asset) = sim.assets.get(id) {
                setpoint_map.insert(*id, setpoint_from_dispatch(dispatch, asset));
            }
        }
    }

    if let Some(db) = state.db.as_ref() {
        let missing: Vec<Uuid> = asset_ids
            .iter()
            .filter(|id| !setpoint_map.contains_key(id))
            .cloned()
            .collect();
        if !missing.is_empty() {
            let rows = sqlx::query_as::<_, DispatchSnapshotRow>(
                r#"
                SELECT DISTINCT ON (asset_id)
                    id,
                    asset_id,
                    mw,
                    duration_s
                FROM dispatches
                WHERE asset_id = ANY($1)
                ORDER BY asset_id, submitted_at DESC
                "#,
            )
            .bind(&missing)
            .fetch_all(db)
            .await
            .context("querying latest dispatches for bootstrap")?;
            for row in rows {
                if let Some(asset) = sim.assets.get(&row.asset_id) {
                    let duration_s = row.duration_s.and_then(|v| u64::try_from(v).ok());
                    setpoint_map.insert(
                        row.asset_id,
                        Setpoint {
                            asset_id: row.asset_id.to_string(),
                            mw: row.mw,
                            duration_s,
                            site_id: Some(asset.site_id.to_string()),
                            group_id: None,
                            dispatch_id: Some(row.id.to_string()),
                        },
                    );
                }
            }
        }
    }

    for id in asset_ids {
        if setpoint_map.contains_key(id) {
            continue;
        }
        if let (Some(asset), Some(state)) = (sim.assets.get(id), sim.state.get(id)) {
            if let Some(sp) = setpoint_from_state(asset, state) {
                setpoint_map.insert(*id, sp);
            }
        }
    }

    for id in asset_ids {
        if telemetry_map.contains_key(id) {
            continue;
        }
        if let (Some(asset), Some(st)) = (sim.assets.get(id), sim.state.get(id)) {
            let mut tmp_state = st.clone();
            let snap = tick_asset(asset, &mut tmp_state, 0.0);
            telemetry_map.insert(*id, snap);
        }
    }

    let mut assets = Vec::new();
    for id in asset_ids {
        assets.push(AssetBootstrap {
            asset_id: id.to_string(),
            telemetry: telemetry_map.get(id).map(telemetry_to_proto),
            setpoint: setpoint_map.get(id).cloned(),
        });
    }

    Ok(BootstrapResponse { assets })
}

fn telemetry_value_to_proto(value: &TelemetryValue) -> Option<crate::proto::TelemetryValue> {
    let value = match value {
        TelemetryValue::F64(v) => crate::proto::telemetry_value::Value::F64(*v),
        TelemetryValue::I64(v) => crate::proto::telemetry_value::Value::I64(*v),
        TelemetryValue::U64(v) => crate::proto::telemetry_value::Value::U64(*v),
        TelemetryValue::Bool(v) => crate::proto::telemetry_value::Value::Bool(*v),
        TelemetryValue::String(v) => crate::proto::telemetry_value::Value::String(v.clone()),
    };
    Some(crate::proto::TelemetryValue {
        value: Some(value),
    })
}

fn proto_extras_to_map(
    extras: &std::collections::HashMap<String, crate::proto::TelemetryValue>,
) -> std::collections::HashMap<String, TelemetryValue> {
    let mut map = std::collections::HashMap::new();
    for (key, value) in extras {
        let Some(kind) = value.value.as_ref() else { continue; };
        let converted = match kind {
            crate::proto::telemetry_value::Value::F64(v) => TelemetryValue::F64(*v),
            crate::proto::telemetry_value::Value::I64(v) => TelemetryValue::I64(*v),
            crate::proto::telemetry_value::Value::U64(v) => TelemetryValue::U64(*v),
            crate::proto::telemetry_value::Value::Bool(v) => TelemetryValue::Bool(*v),
            crate::proto::telemetry_value::Value::String(v) => {
                TelemetryValue::String(v.clone())
            }
        };
        map.insert(key.clone(), converted);
    }
    map
}

async fn handle_agent_heartbeat(state: &AppState, hb: crate::proto::Heartbeat) -> Result<()> {
    let asset_id = Uuid::parse_str(&hb.asset_id)?;
    let ts = DateTime::parse_from_rfc3339(&hb.timestamp)
        .map(|dt| dt.with_timezone(&Utc))
        .context("parsing heartbeat timestamp")?;

    if let Some(db) = state.db.as_ref() {
        persist_heartbeat(db, asset_id, ts).await?;
    }
    Ok(())
}

async fn handle_dispatch_ack(state: &AppState, ack: DispatchAck) -> Result<()> {
    let dispatch_id = Uuid::parse_str(&ack.dispatch_id)?;
    let ts = DateTime::parse_from_rfc3339(&ack.timestamp)
        .map(|dt| dt.with_timezone(&Utc))
        .context("parsing dispatch ack timestamp")?;

    if let Some(db) = state.db.as_ref() {
        sqlx::query(
            r#"
            UPDATE dispatches
            SET ack_status = $1,
                acked_at = $2,
                ack_reason = $3
            WHERE id = $4
            "#,
        )
        .bind(&ack.status)
        .bind(ts)
        .bind(&ack.reason)
        .bind(dispatch_id)
        .execute(db)
        .await
        .context("updating dispatch ack")?;
    }
    Ok(())
}

async fn maybe_emit_soc_event(state: &AppState, db: &PgPool, snap: &Telemetry) -> Result<()> {
    let asset = {
        let sim = state.sim.read().await;
        sim.assets.get(&snap.asset_id).cloned()
    };
    let Some(asset) = asset else {
        return Ok(());
    };
    let (min_soc, max_soc) = soc_bounds(&asset);
    let eps = 1e-6;
    let next_state = if snap.soc_mwhr <= min_soc + eps {
        SocState::BelowMin
    } else if snap.soc_mwhr >= max_soc - eps {
        SocState::AboveMax
    } else {
        SocState::InRange
    };

    let mut states = state.soc_state.write().await;
    let prev = states.insert(snap.asset_id, next_state);
    if prev == Some(next_state) {
        return Ok(());
    }

    let (event_type, message) = match next_state {
        SocState::BelowMin => ("MIN_SOC_REACHED", "Min SOC reached"),
        SocState::AboveMax => ("MAX_SOC_REACHED", "Max SOC reached"),
        SocState::InRange => return Ok(()),
    };

    let event = BessEvent {
        id: Uuid::new_v4(),
        asset_id: snap.asset_id,
        site_id: asset.site_id,
        timestamp: snap.timestamp,
        event_type: event_type.to_string(),
        severity: "warning".to_string(),
        message: message.to_string(),
    };
    persist_event(db, &event).await?;
    Ok(())
}

async fn handle_agent_event(state: &AppState, evt: ProtoEvent) -> Result<()> {
    let asset_id = Uuid::parse_str(&evt.asset_id)?;
    let site_id = Uuid::parse_str(&evt.site_id)?;
    let ts = DateTime::parse_from_rfc3339(&evt.timestamp)
        .map(|dt| dt.with_timezone(&Utc))
        .context("parsing event timestamp")?;

    let event = BessEvent {
        id: Uuid::parse_str(&evt.id).unwrap_or_else(|_| Uuid::new_v4()),
        asset_id,
        site_id,
        timestamp: ts,
        event_type: evt.event_type,
        severity: evt.severity,
        message: evt.message,
    };

    if let Some(db) = state.db.as_ref() {
        persist_event(db, &event).await?;
    }
    Ok(())
}
