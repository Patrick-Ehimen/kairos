//! Runs state sync and detection against a live node and prints every
//! opportunity found. Never submits anything.
//!
//! ```sh
//! KAIROS_WS_URL=wss://… cargo run -p kairos-detector --example live_detect -- [seconds] [chain]
//! ```

use std::path::PathBuf;
use std::time::Duration;

use alloy::providers::{Provider, ProviderBuilder, WsConnect};
use kairos_core::{ChainProfile, KairosError, Result};
use kairos_detector::{Detector, DetectorConfig};
use kairos_state::{StateConfig, StateSync};
use tokio::time::{Instant, timeout};
use tracing_subscriber::EnvFilter;

/// How often the gas price is re-read.
const GAS_PRICE_REFRESH: Duration = Duration::from_secs(30);

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let mut args = std::env::args().skip(1);
    let seconds: u64 = args.next().map_or(60, |s| s.parse().expect("seconds"));
    let chain = args.next().unwrap_or_else(|| "base".into());
    let url = std::env::var("KAIROS_WS_URL").expect("set KAIROS_WS_URL to a WebSocket RPC URL");
    let rpc =
        |e: alloy::transports::TransportError| KairosError::NodeConnectionFailed(e.to_string());

    let config_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config");
    let profile = ChainProfile::load(config_dir.join(format!("chains/{chain}.toml")))?;
    let config = StateConfig::load(config_dir.join(format!("pools/{chain}.toml")))?;

    let provider = ProviderBuilder::new()
        .connect_ws(WsConnect::new(url))
        .await
        .map_err(rpc)?
        .erased();
    let chain_id = provider.get_chain_id().await.map_err(rpc)?;
    if chain_id != profile.chain_id {
        return Err(KairosError::ConfigError(format!(
            "node is on chain {chain_id}, profile {} expects {}",
            profile.name, profile.chain_id
        )));
    }

    let detector = Detector::new(DetectorConfig::default(), &profile);
    let (sync, mut snapshots) = StateSync::new(provider.clone(), config, profile.multicall3);
    let task = tokio::spawn(sync.run());

    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut gas_price = provider.get_gas_price().await.map_err(rpc)?;
    let mut gas_read_at = Instant::now();
    let (mut last_seq, mut scans, mut found) = (0, 0u64, 0u64);
    while let Ok(Ok(())) = timeout(
        deadline.saturating_duration_since(Instant::now()),
        snapshots.changed(),
    )
    .await
    {
        if gas_read_at.elapsed() >= GAS_PRICE_REFRESH {
            gas_price = provider.get_gas_price().await.map_err(rpc)?;
            gas_read_at = Instant::now();
        }
        let snap = snapshots.borrow_and_update().clone();
        let changed: Vec<_> = snap.changed_since(last_seq).collect();
        last_seq = snap.seq();
        let started = Instant::now();
        let opportunities = detector.detect(&snap, &changed, gas_price);
        scans += 1;
        for o in &opportunities {
            found += 1;
            println!(
                "block {} net {} wei (gross {}, gas {} × {gas_price}) in {:?}: {}",
                o.block_number,
                o.net_profit_wei,
                o.total_profit_wei,
                o.total_gas,
                started.elapsed(),
                o.id
            );
        }
    }
    task.abort();
    println!("{scans} snapshots scanned, {found} opportunities, gas price {gas_price} wei");
    Ok(())
}
