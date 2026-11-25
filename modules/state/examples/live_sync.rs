//! Runs [`StateSync`] against a live node, then checks the incrementally
//! maintained state against a fresh load at the same block.
//!
//! ```sh
//! KAIROS_WS_URL=wss://… cargo run -p kairos-state --example live_sync -- [seconds] [chain]
//! ```
//!
//! `chain` names the files in `config/chains/` and `config/pools/`
//! (default `base`). The fee refresh and log audit run on short intervals so
//! a one-minute run exercises them.

use std::path::PathBuf;
use std::time::Duration;

use alloy::primitives::{Address, I256, U256};
use alloy::providers::{Provider, ProviderBuilder, WsConnect};
use kairos_core::{ChainProfile, KairosError, Result};
use kairos_dex::{AnyPool, Pool};
use kairos_state::{PoolLoader, StateConfig, StateSync};
use tokio::time::{Instant, timeout};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,kairos_state=debug".into()),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let seconds: u64 = args.next().map_or(60, |s| s.parse().expect("seconds"));
    let chain = args.next().unwrap_or_else(|| "base".into());
    let url = std::env::var("KAIROS_WS_URL").expect("set KAIROS_WS_URL to a WebSocket RPC URL");

    let config_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config");
    let profile = ChainProfile::load(config_dir.join(format!("chains/{chain}.toml")))?;
    let mut config = StateConfig::load(config_dir.join(format!("pools/{chain}.toml")))?;
    config.sync.fee_refresh_secs = 10;
    config.sync.audit_secs = 20;

    let provider = ProviderBuilder::new()
        .connect_ws(WsConnect::new(url))
        .await
        .map_err(|e| KairosError::NodeConnectionFailed(e.to_string()))?
        .erased();
    let chain_id = provider
        .get_chain_id()
        .await
        .map_err(|e| KairosError::NodeConnectionFailed(e.to_string()))?;
    if chain_id != profile.chain_id {
        return Err(KairosError::ConfigError(format!(
            "node is on chain {chain_id}, profile {} expects {}",
            profile.name, profile.chain_id
        )));
    }

    let (sync, mut snapshots) =
        StateSync::new(provider.clone(), config.clone(), profile.multicall3);
    let task = tokio::spawn(sync.run());

    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut last_seq = 0;
    while let Ok(Ok(())) = timeout(
        deadline.saturating_duration_since(Instant::now()),
        snapshots.changed(),
    )
    .await
    {
        let snap = snapshots.borrow_and_update().clone();
        let changed: Vec<Address> = snap.changed_since(last_seq).collect();
        last_seq = snap.seq();
        for address in &changed {
            match snap.pool(address) {
                Some(pool) => println!(
                    "seq {:>4} block {} {address} {}",
                    snap.seq(),
                    snap.block(),
                    describe(pool)
                ),
                None => println!("seq {:>4} {address} syncing", snap.seq()),
            }
        }
    }
    task.abort();

    // Compare against a fresh load at the snapshot's block.
    let snap = snapshots.borrow().clone();
    let loader = PoolLoader::new(provider, profile.multicall3, config.sync.tick_window_words);
    let fresh = loader.load(&config.pools, snap.block()).await?;
    let mut mismatches = 0;
    for (pool, pool_config) in fresh.iter().zip(&config.pools) {
        let address = pool.address();
        let Some(ours) = snap.pool(&address) else {
            println!("{address}: still syncing, not compared");
            continue;
        };
        // A dynamic fee is polled, so it can lag the chain by up to
        // fee_refresh_secs. Report the lag, then compare the rest of the
        // state with the chain's fee.
        let mut ours = ours.clone();
        if let (AnyPool::ConcentratedLiquidity(o), AnyPool::ConcentratedLiquidity(c)) =
            (&mut ours, pool)
            && pool_config.kind.fee_is_dynamic()
            && o.fee_pips() != c.fee_pips()
        {
            println!(
                "{address}: polled fee {} vs {} at block {} (dynamic fee lag)",
                o.fee_pips(),
                c.fee_pips(),
                snap.block()
            );
            o.set_fee_pips(c.fee_pips())?;
        }
        let ours = &ours;
        let same = describe(ours) == describe(pool) && same_quotes(ours, pool);
        println!(
            "{address}: {}",
            if same { "matches chain" } else { "MISMATCH" }
        );
        if !same {
            mismatches += 1;
            println!("  ours:  {}\n  chain: {}", describe(ours), describe(pool));
        }
    }
    println!(
        "checked {} pools at block {} after {} snapshots: {mismatches} mismatches",
        fresh.len(),
        snap.block(),
        snap.seq()
    );
    if mismatches > 0 {
        std::process::exit(1);
    }
    Ok(())
}

fn describe(pool: &AnyPool) -> String {
    match pool {
        AnyPool::ConstantProduct(p) => {
            let (r0, r1) = p.reserves();
            format!("reserves {r0} / {r1}, fee {} ppm", p.fee_ppm())
        }
        AnyPool::ConcentratedLiquidity(p) => {
            let (sqrt_price, tick, liquidity) = p.slot0();
            format!(
                "tick {tick}, sqrtPriceX96 {sqrt_price}, liquidity {liquidity}, fee {}",
                p.fee_pips()
            )
        }
    }
}

/// Quotes in both directions at sizes from dust to large agree.
fn same_quotes(a: &AnyPool, b: &AnyPool) -> bool {
    let [t0, t1] = [a.tokens()[0], a.tokens()[1]];
    (3..=21).step_by(3).all(|exp| {
        let amount = U256::from(10).pow(U256::from(exp));
        a.get_amount_out(t0, t1, amount) == b.get_amount_out(t0, t1, amount)
            && a.get_amount_out(t1, t0, amount) == b.get_amount_out(t1, t0, amount)
    }) && match (a, b) {
        (AnyPool::ConcentratedLiquidity(x), AnyPool::ConcentratedLiquidity(y)) => {
            let amount = I256::try_from(1_000_000_000u64).unwrap();
            x.swap(true, amount) == y.swap(true, amount)
        }
        _ => true,
    }
}
