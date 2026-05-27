//! X-Plane WebAPI hybrid bridge with timeout protection.

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::Value;
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
    data: Option<Value>,
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

/// Fetches the current value of a single DataRef via REST. Returns None on timeout or parse error.
async fn fetch_dataref(client: &reqwest::Client, id: u64, timeout_ms: u64) -> Option<Value> {
    let url = format!("{}/datarefs/{}/value", XPLANE_WEB_REST, id);
    let resp = timeout(Duration::from_millis(timeout_ms), client.get(&url).send())
        .await.ok()?.ok()?;
    resp.json::<Value>().await.ok()?.get("data").cloned()
}

/// Polls a list of DataRef IDs and applies any values that changed to state.
async fn poll_datarefs(
    ids: &[u64],
    timeout_ms: u64,
    client: &reqwest::Client,
    id_to_enum: &HashMap<u64, DataRefId>,
    state: &Arc<Mutex<CockpitState>>,
) {
    for &id in ids {
        let Some(val) = fetch_dataref(client, id, timeout_ms).await else { continue };
        let Some(&enum_id) = id_to_enum.get(&id) else { continue };
        state.lock().unwrap().update_from_dataref(enum_id, &val);
    }
}

/// Parses a WebSocket text frame into a list of (dataref_id, value) update pairs.
fn parse_ws_updates(text: &str) -> Option<Vec<(u64, Value)>> {
    let resp: WsResponse = serde_json::from_str(text).ok()?;
    if resp.msg_type != "dataref_update_values" && resp.msg_type != "dataref_values" {
        return None;
    }
    let updates = resp.data?.as_array()?
        .iter()
        .filter_map(|u| Some((u["id"].as_u64()?, u.get("value")?.clone())))
        .collect();
    Some(updates)
}

async fn run_bridge(state: Arc<Mutex<CockpitState>>) -> Result<()> {
    println!("Discovering DataRefs...");
    let client = reqwest::Client::new();
    let rest_resp: RestResponse = client
        .get(format!("{}/datarefs", XPLANE_WEB_REST))
        .send().await?.json().await?;

    let mut id_to_enum: HashMap<u64, DataRefId> = HashMap::new();
    let mut pos_ids = Vec::new();
    let mut switch_ids = Vec::new();

    for dr in rest_resp.data {
        let Some(enum_id) = DataRefId::from_name(&dr.name) else { continue };
        id_to_enum.insert(dr.id, enum_id);
        if dr.name.contains("head") || dr.name.contains("position/psi") {
            pos_ids.push(dr.id);
        } else {
            switch_ids.push(dr.id);
        }
    }

    let (ws_stream, _) = connect_async(XPLANE_WEB_WS).await?;
    let (mut write, mut read) = ws_stream.split();

    let req_id = REQ_ID_COUNTER.fetch_add(1, Ordering::SeqCst);
    let sub_params: Vec<_> = pos_ids.iter().map(|id| serde_json::json!({"id": id})).collect();
    let sub_req = serde_json::json!({
        "req_id": req_id,
        "type": "dataref_subscribe_values",
        "params": {"datarefs": sub_params}
    });
    write.send(Message::Text(sub_req.to_string().into())).await?;

    // Initial sync: fetch all known DataRefs once before streaming begins.
    println!("Fetching initial state...");
    let all_ids: Vec<u64> = id_to_enum.keys().copied().collect();
    poll_datarefs(&all_ids, 500, &client, &id_to_enum, &state).await;

    // Hybrid polling: REST at 20 Hz for smooth positional data, 5 Hz for switches.
    let state_poll = Arc::clone(&state);
    let client_poll = client.clone();
    let id_to_enum_poll = id_to_enum.clone();
    let (pos_ids_poll, switch_ids_poll) = (pos_ids.clone(), switch_ids.clone());
    tokio::spawn(async move {
        let mut pos_ticker = interval(Duration::from_millis(50));
        let mut sw_ticker  = interval(Duration::from_millis(200));
        loop {
            tokio::select! {
                _ = pos_ticker.tick() => {
                    poll_datarefs(&pos_ids_poll, 30, &client_poll, &id_to_enum_poll, &state_poll).await;
                }
                _ = sw_ticker.tick() => {
                    poll_datarefs(&switch_ids_poll, 100, &client_poll, &id_to_enum_poll, &state_poll).await;
                }
            }
        }
    });

    // WebSocket events (subscribed position DataRefs, pushed by X-Plane).
    while let Some(Ok(Message::Text(text))) = read.next().await {
        let Some(updates) = parse_ws_updates(&text) else { continue };
        let mut s = state.lock().unwrap();
        for (id, val) in updates {
            let Some(&enum_id) = id_to_enum.get(&id) else { continue };
            s.update_from_dataref(enum_id, &val);
        }
    }
    Ok(())
}
