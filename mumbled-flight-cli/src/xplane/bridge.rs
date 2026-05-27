//! X-Plane WebAPI bridge — REST polling at 30 Hz.

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::time::{interval, sleep, timeout};
use std::time::Duration;
use mumbled_flight_core::state::{CockpitState, DataRefId};
use log::{info, warn};

const XPLANE_WEB_REST: &str = "http://localhost:8086/api/v3";

#[derive(Deserialize)]
struct RestDataRef {
    id: u64,
    name: String,
}

#[derive(Deserialize)]
struct RestResponse {
    data: Vec<RestDataRef>,
}

pub async fn run_bridge_forever(state: Arc<Mutex<CockpitState>>) -> Result<()> {
    loop {
        if let Err(e) = run_bridge(Arc::clone(&state)).await {
            warn!("X-Plane bridge error: {}. Retrying in 5s...", e);
            sleep(Duration::from_secs(5)).await;
        }
    }
}

async fn fetch_dataref(client: &reqwest::Client, id: u64, timeout_ms: u64) -> Option<Value> {
    let url = format!("{}/datarefs/{}/value", XPLANE_WEB_REST, id);
    let resp = timeout(Duration::from_millis(timeout_ms), client.get(&url).send())
        .await.ok()?.ok()?;
    resp.json::<Value>().await.ok()?.get("data").cloned()
}

async fn run_bridge(state: Arc<Mutex<CockpitState>>) -> Result<()> {
    info!("Discovering DataRefs...");
    let client = reqwest::Client::new();
    let rest_resp: RestResponse = client
        .get(format!("{}/datarefs", XPLANE_WEB_REST))
        .send().await?.json().await?;

    let id_to_enum: HashMap<u64, DataRefId> = rest_resp.data
        .into_iter()
        .filter_map(|dr| Some((dr.id, DataRefId::from_name(&dr.name)?)))
        .collect();

    info!("Polling {} DataRefs at 10 Hz...", id_to_enum.len());
    let mut ticker = interval(Duration::from_millis(100));
    loop {
        ticker.tick().await;
        for (&id, &enum_id) in &id_to_enum {
            let Some(val) = fetch_dataref(&client, id, 30).await else { continue };
            state.lock().unwrap().update_from_dataref(enum_id, &val);
        }
    }
}
