use anyhow::{anyhow, Result};
use opcua::client::prelude::*;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

const DEFAULT_ENDPOINT: &str = "opc.tcp://127.0.0.1:62542";
const DEFAULT_NODE: &str = "ns=2;s=[edge]/Assets/CT-BESS-A/control/setpoint_mw";

fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let endpoint = std::env::var("OPCUA_ENDPOINT").unwrap_or_else(|_| DEFAULT_ENDPOINT.to_string());
    let node_str = std::env::var("OPCUA_NODE").unwrap_or_else(|_| DEFAULT_NODE.to_string());
    let write_value: Option<f64> = std::env::var("OPCUA_WRITE")
        .ok()
        .and_then(|s| s.parse::<f64>().ok());
    let subscribe = std::env::var("OPCUA_SUBSCRIBE")
        .map(|s| {
            let s = s.to_ascii_lowercase();
            !(s == "0" || s == "false" || s == "no")
        })
        .unwrap_or(false);

    let mut client = ClientBuilder::new()
        .application_name("opc_test_client")
        .application_uri("urn:opc_test_client")
        .trust_server_certs(true)
        .create_sample_keypair(true)
        .session_retry_limit(3)
        .client()
        .ok_or_else(|| anyhow!("failed to build opc ua client"))?;

    // Build an anonymous / None-security endpoint description.
    let endpoint_desc: EndpointDescription = (
        endpoint.as_str(),
        SecurityPolicy::None.to_str(),
        MessageSecurityMode::None,
        UserTokenPolicy::anonymous(),
    )
        .into();

    // Connect and activate a session.
    let session = client
        .connect_to_endpoint(endpoint_desc, IdentityToken::Anonymous)
        .map_err(|e| anyhow!("session connect error: {e:?}"))?;

    // One-shot read (and optional write / subscribe) of the requested node.
    let node_id: NodeId = node_str.parse().map_err(|e| anyhow!("bad node id: {e:?}"))?;

    {
        let session = session.write();
        let read_id = ReadValueId {
            node_id: node_id.clone(),
            attribute_id: AttributeId::Value as u32,
            index_range: UAString::null(),
            data_encoding: QualifiedName::null(),
        };

        if let Some(v) = write_value {
            let write = WriteValue {
                node_id: read_id.node_id.clone(),
                attribute_id: AttributeId::Value as u32,
                index_range: UAString::null(),
                value: DataValue {
                    value: Some(Variant::Double(v)),
                    status: None,
                    source_timestamp: None,
                    source_picoseconds: None,
                    server_timestamp: None,
                    server_picoseconds: None,
                },
            };
            match session.write(&[write]) {
                Ok(statuses) => println!("write statuses: {:?}", statuses),
                Err(err) => println!("write failed: {:?}", err),
            }
        }

        match session.read(&[read_id.clone()], TimestampsToReturn::Neither, 5_000.0) {
            Ok(values) => {
                for dv in values {
                    println!("value: {:?}", dv.value);
                }
            }
            Err(err) => {
                println!("read failed: {:?}", err);
            }
        }

        if subscribe {
            let sub_id = session
                .create_subscription(
                    1_000.0, // ms
                    10,
                    30,
                    0,
                    0,
                    true,
                    DataChangeCallback::new(|items| {
                        for item in items {
                            let dv = item.last_value();
                            println!(
                                "sub update {} => {:?}",
                                item.item_to_monitor().node_id,
                                dv.value
                            );
                        }
                    }),
                )
                .map_err(|e| anyhow!("subscription create failed: {e:?}"))?;

            let mon = MonitoredItemCreateRequest {
                item_to_monitor: read_id,
                monitoring_mode: MonitoringMode::Reporting,
                requested_parameters: MonitoringParameters {
                    client_handle: 42,
                    sampling_interval: 1_000.0,
                    filter: ExtensionObject::null(),
                    queue_size: 10,
                    discard_oldest: true,
                },
            };

            session
                .create_monitored_items(sub_id, TimestampsToReturn::Both, &[mon])
                .map_err(|e| anyhow!("monitored item create failed: {e:?}"))?;

            println!("subscription active; press Ctrl+C to stop");
        } else {
            let _ = session.disconnect();
        }
    }

    if subscribe {
        // Block and pump publish requests until killed.
        let _ = Session::run(session);
    }

    Ok(())
}
