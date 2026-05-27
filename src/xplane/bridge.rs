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
    #[allow(dead_code)]
    req_id: Option<u64>,
    data: Option<serde_json::Value>,
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
            // Track positional DataRefs separately for high-speed polling
            if dr.name.contains("head") || dr.name.contains("position/psi") { 
                pos_ids.push(dr.id); 
            } else { 
                switch_ids.push(dr.id); 
            }
        }
    }

    let (ws_stream, _) = connect_async(XPLANE_WEB_WS).await?;
    let (mut write, mut read) = ws_stream.split();

    let my_req_id = REQ_ID_COUNTER.fetch_add(1, Ordering::SeqCst);
    let sub_params: Vec<_> = pos_ids.iter().map(|id| serde_json::json!({"id": id})).collect();
    let sub_req = serde_json::json!({"req_id": my_req_id, "type": "dataref_subscribe_values", "params": {"datarefs": sub_params}});
    write.send(Message::Text(sub_req.to_string().into())).await?;

    // --- INITIAL SYNC ---
    println!("Fetching initial state...");
    for (&id, &enum_id) in &id_to_enum {
        let val_url = format!("{}/datarefs/{}/value", XPLANE_WEB_REST, id);
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
    let pos_ids_poll = pos_ids.clone();
    let client_poll = client.clone();
    let id_to_enum_poll = id_to_enum.clone();
    
    // HYBRID POLLING: WebSocket for events, high-speed REST for coordinates
    tokio::spawn(async move {
        let mut pos_ticker = interval(Duration::from_millis(50)); // 20Hz for Smooth Spatial
        let mut sw_ticker = interval(Duration::from_millis(200)); // 5Hz for Switches
        loop {
            tokio::select! {
                _ = pos_ticker.tick() => {
                    for &id in &pos_ids_poll {
                        let url = format!("{}/datarefs/{}/value", XPLANE_WEB_REST, id);
                        if let Ok(Ok(resp)) = timeout(Duration::from_millis(30), client_poll.get(url).send()).await {
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
                _ = sw_ticker.tick() => {
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
            }
        }
    });

    while let Some(msg_result) = read.next().await {
        if let Ok(Message::Text(text)) = msg_result {
            if let Ok(update_resp) = serde_json::from_str::<WsResponse>(&text) {
                if update_resp.msg_type == "dataref_update_values" || update_resp.msg_type == "dataref_values" {
                    if let Some(data) = update_resp.data {
                        if let Some(updates) = data.as_array() {
                            let mut s = state.lock().unwrap();
                            for update in updates {
                                if let (Some(id), Some(val)) = (update["id"].as_u64(), update.get("value")) {
                                    if let Some(enum_id) = id_to_enum.get(&id) {
                                        s.update_from_dataref(*enum_id, val);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}
