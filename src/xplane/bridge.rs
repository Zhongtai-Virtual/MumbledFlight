//! X-Plane WebAPI hybrid bridge with timeout protection.

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, atomic::{AtomicU64, Ordering}};
use tokio::time::{sleep, interval, timeout};
use std::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use crate::state::{CockpitState, DataRefId};

const XPLANE_WEB_REST: &str = "http://localhost:8086/api/v3";
const XPLANE_WEB_WS: &str = "ws://localhost:8086/api/v3";

static REQ_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Deserialize, Debug)]
struct WsResponse {
    #[serde(rename = "type")]
    msg_type: String,
    req_id: Option<u64>,
    data: Option<serde_json::Value>,
    error: Option<String>,
}

#[derive(Deserialize, Debug)]
struct RestDataRef {
    id: u64,
    name: String,
}

#[derive(Deserialize, Debug)]
struct RestResponse {
    data: Vec<RestDataRef>,
}

pub async fn run_bridge_forever(state: Arc<Mutex<CockpitState>>) -> Result<()> {
    loop {
        if let Err(e) = run_bridge(Arc::clone(&state)).await {
            eprintln!("X-Plane Bridge error: {}. Retrying in 5s...", e);
            sleep(Duration::from_secs(5)).await;
        }
    }
}

async fn run_bridge(state: Arc<Mutex<CockpitState>>) -> Result<()> {
    println!("Discovering DataRefs...");
    let client = reqwest::Client::new();
    let rest_resp: RestResponse = client.get(format!("{}/datarefs", XPLANE_WEB_REST)).send().await?.json().await?;

    let mut id_to_enum = HashMap::new();
    let mut switch_ids = Vec::new();
    let mut pos_ids = Vec::new();

    for dr in rest_resp.data {
        if let Some(enum_id) = DataRefId::from_name(&dr.name) {
            id_to_enum.insert(dr.id, enum_id);
            if dr.name.contains("head") { pos_ids.push(dr.id); } else { switch_ids.push(dr.id); }
        }
    }

    let (ws_stream, _) = connect_async(XPLANE_WEB_WS).await?;
    let (mut write, mut read) = ws_stream.split();

    let my_req_id = REQ_ID_COUNTER.fetch_add(1, Ordering::SeqCst);
    let sub_params: Vec<_> = pos_ids.iter().map(|id| serde_json::json!({"id": id})).collect();
    let sub_req = serde_json::json!({"req_id": my_req_id, "type": "dataref_subscribe_values", "params": {"datarefs": sub_params}});
    write.send(Message::Text(sub_req.to_string().into())).await?;

    // --- NON-BLOCKING INITIAL SYNC ---
    println!("Fetching initial state (with timeout)...");
    for (&id, &enum_id) in &id_to_enum {
        let val_url = format!("{}/datarefs/{}/value", XPLANE_WEB_REST, id);
        // Use timeout to prevent hanging the bridge
        if let Ok(Ok(resp)) = timeout(Duration::from_millis(500), client.get(val_url).send()).await {
            if let Ok(val_json) = resp.json::<serde_json::Value>().await {
                if let Some(val) = val_json.get("data") {
                    let mut s = state.lock().unwrap();
                    s.update_from_dataref(enum_id, val);
                }
            }
        }
    }

    let state_poll = Arc::clone(&state);
    let switch_ids_poll = switch_ids.clone();
    let client_poll = client.clone();
    let id_to_enum_poll = id_to_enum.clone();
    
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_millis(200));
        loop {
            ticker.tick().await;
            for &id in &switch_ids_poll {
                let url = format!("{}/datarefs/{}/value", XPLANE_WEB_REST, id);
                if let Ok(Ok(resp)) = timeout(Duration::from_millis(100), client_poll.get(url).send()).await {
                    if let Ok(val_json) = resp.json::<serde_json::Value>().await {
                        if let Some(val) = val_json.get("data") {
                            if let Some(enum_id) = id_to_enum_poll.get(&id) {
                                let mut s = state_poll.lock().unwrap();
                                s.update_from_dataref(*enum_id, val);
                            }
                        }
                    }
                }
            }
        }
    });

    while let Some(msg_result) = read.next().await {
        let msg = match msg_result { Ok(Message::Text(t)) => t, _ => continue };
        let resp: WsResponse = match serde_json::from_str(&msg) { Ok(r) => r, Err(_) => continue };
        if resp.msg_type != "dataref_values" && resp.msg_type != "dataref_update_values" { continue; }
        let updates = match resp.data.and_then(|d| d.as_array().cloned()) { Some(u) => u, None => continue };

        let mut s = state.lock().unwrap();
        for update in updates {
            let id = update["id"].as_u64().unwrap();
            let val = &update["value"];
            if let Some(enum_id) = id_to_enum.get(&id) {
                s.update_from_dataref(*enum_id, val);
            }
        }
    }
    Ok(())
}
