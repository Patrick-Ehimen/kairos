//! The task that keeps pool state in sync with the chain.
//!
//! # RPC usage
//!
//! Designed for a hosted RPC billed per request and per byte of
//! subscription traffic:
//!
//! - One `eth_subscribe("logs")`, filtered to the tracked pools and the
//!   state-changing events in [`TOPICS`]. No `newHeads` or pending-transaction
//!   streams. Traffic scales with activity on the tracked pools only.
//! - Startup and every reload: `eth_blockNumber` plus three multicall
//!   `eth_call`s, whatever the number of pools (see [`PoolLoader`]).
//! - Every [`SyncSettings::fee_refresh_secs`]: one multicall `eth_call`, only
//!   if a tracked pool has a dynamic fee.
//! - Every [`SyncSettings::audit_secs`]: `eth_blockNumber` and one
//!   `eth_getLogs` per [`SyncSettings::max_log_range`] blocks.
//!
//! # Consistency
//!
//! The subscription is opened *before* the head block `N` is read and the
//! pools are loaded at `N`. Every log after `N` therefore arrives on the
//! subscription, and earlier ones are already in the loaded state, so no
//! backfill is needed. The [`Tracker`] applies the ordering, reorg and gap
//! rules. If the subscription closes or falls behind, every pool is marked
//! as syncing and the whole session restarts with backoff.
//!
//! [`SyncSettings::fee_refresh_secs`]: crate::SyncSettings::fee_refresh_secs
//! [`SyncSettings::audit_secs`]: crate::SyncSettings::audit_secs
//! [`SyncSettings::max_log_range`]: crate::SyncSettings::max_log_range

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::Address;
use alloy::providers::Provider;
use alloy::rpc::types::{Filter, Log};
use futures::future::BoxFuture;
use futures::stream::{FuturesUnordered, StreamExt};
use kairos_core::{KairosError, Result};
use kairos_dex::AnyPool;
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::sync::watch;
use tokio::time::{Instant, MissedTickBehavior, interval_at, sleep};
use tracing::{debug, info, warn};

use crate::config::{PoolConfig, StateConfig};
use crate::events::{PoolLog, TOPICS};
use crate::loader::PoolLoader;
use crate::snapshot::StateSnapshot;
use crate::tracker::Tracker;

/// Logs buffered between the WebSocket reader and the sync task. Overflow
/// drops logs, which triggers a full resync, so this is sized for bursts.
const SUBSCRIPTION_BUFFER: usize = 4096;
/// Delay before the first session retry; doubles up to [`MAX_BACKOFF`].
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// A session that lasted this long resets the backoff.
const HEALTHY_SESSION: Duration = Duration::from_secs(60);
/// Delay before retrying a failed pool reload.
const RELOAD_RETRY: Duration = Duration::from_secs(5);

/// Keeps a [`StateSnapshot`] current from a log subscription.
///
/// ```no_run
/// # async fn demo(provider: alloy::providers::DynProvider, multicall: alloy::primitives::Address)
/// #     -> kairos_core::Result<()> {
/// use kairos_state::{StateConfig, StateSync};
///
/// let config = StateConfig::load("config/pools/base.toml")?;
/// let (sync, mut snapshots) = StateSync::new(provider, config, multicall);
/// tokio::spawn(sync.run());
/// while snapshots.changed().await.is_ok() {
///     let snap = snapshots.borrow_and_update().clone();
///     // price the pools in snap.changed_since(last_seq) …
/// }
/// # Ok(()) }
/// ```
pub struct StateSync<P> {
    provider: P,
    loader: PoolLoader<P>,
    config: StateConfig,
    tx: watch::Sender<Arc<StateSnapshot>>,
    seq: u64,
}

enum Job {
    Reload(Vec<PoolConfig>, Result<(u64, Vec<AnyPool>)>),
    ReloadRetryReady,
    Fees(Result<Vec<(Address, u32)>>),
    Audit(Result<(u64, Vec<Log>)>),
}

impl<P: Provider + Clone + 'static> StateSync<P> {
    /// Creates the sync task and the receiver its snapshots are published
    /// on. The receiver starts with an empty snapshot (`seq() == 0`).
    ///
    /// `provider` must support subscriptions (WebSocket or IPC).
    /// `multicall` is the chain's Multicall3 deployment.
    pub fn new(
        provider: P,
        config: StateConfig,
        multicall: Address,
    ) -> (Self, watch::Receiver<Arc<StateSnapshot>>) {
        let (tx, rx) = watch::channel(Arc::new(StateSnapshot::default()));
        let loader = PoolLoader::new(provider.clone(), multicall, config.sync.tick_window_words);
        (
            Self {
                provider,
                loader,
                config,
                tx,
                seq: 0,
            },
            rx,
        )
    }

    /// Runs until a permanent error. Connection failures are retried with
    /// backoff.
    ///
    /// # Errors
    ///
    /// [`KairosError::ConfigError`] if no pools are configured or a pool is
    /// not what its config says (for example an Aerodrome stable pool).
    pub async fn run(mut self) -> Result<Infallible> {
        if self.config.pools.is_empty() {
            return Err(KairosError::ConfigError("no pools to track".into()));
        }
        let mut backoff = MIN_BACKOFF;
        loop {
            let started = Instant::now();
            let Err(err) = self.session().await;
            if matches!(err, KairosError::ConfigError(_)) {
                return Err(err);
            }
            if started.elapsed() >= HEALTHY_SESSION {
                backoff = MIN_BACKOFF;
            }
            warn!(error = %err, retry_in = ?backoff, "state sync session ended; resyncing");
            self.publish_all_syncing();
            sleep(backoff).await;
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }

    fn publish(&mut self, tracker: &mut Tracker) {
        if let Some(snapshot) = tracker.publish() {
            self.seq = snapshot.seq();
            self.tx.send_replace(Arc::new(snapshot));
        }
    }

    fn publish_all_syncing(&mut self) {
        if self.seq == 0 {
            return;
        }
        self.seq += 1;
        let snapshot = self.tx.borrow().all_syncing(self.seq);
        self.tx.send_replace(Arc::new(snapshot));
    }

    fn filter(&self) -> Filter {
        let addresses: Vec<Address> = self.config.pools.iter().map(|p| p.address).collect();
        Filter::new()
            .address(addresses)
            .event_signature(TOPICS.to_vec())
    }

    async fn session(&mut self) -> Result<Infallible> {
        let filter = self.filter();
        let mut sub = self
            .provider
            .subscribe_logs(&filter)
            .channel_size(SUBSCRIPTION_BUFFER)
            .await
            .map_err(|e| KairosError::NodeConnectionFailed(format!("eth_subscribe logs: {e}")))?;

        let head = self.loader.block_number().await?;
        let pools = self.loader.load(&self.config.pools, head).await?;
        let mut tracker = Tracker::new(self.seq);
        for (config, pool) in self.config.pools.iter().zip(pools) {
            tracker.install(config, pool, head);
        }
        self.publish(&mut tracker);
        info!(
            block = head,
            pools = self.config.pools.len(),
            "pool state loaded"
        );

        let settings = &self.config.sync;
        let audit_range = AuditRange {
            lag: settings.audit_lag_blocks,
            max_range: settings.max_log_range,
        };
        let fee_every = Duration::from_secs(settings.fee_refresh_secs);
        let audit_every = Duration::from_secs(settings.audit_secs);
        let mut fee_tick = interval_at(Instant::now() + fee_every, fee_every);
        let mut audit_tick = interval_at(Instant::now() + audit_every, audit_every);
        fee_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        audit_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let mut jobs: FuturesUnordered<BoxFuture<'static, Job>> = FuturesUnordered::new();
        let (mut reloading, mut fees_busy, mut audit_busy) = (false, false, false);

        loop {
            if !reloading {
                let configs = tracker.take_reload_requests();
                if !configs.is_empty() {
                    debug!(pools = configs.len(), "reloading pools");
                    reloading = true;
                    let loader = self.loader.clone();
                    jobs.push(Box::pin(async move {
                        let result = async {
                            let block = loader.block_number().await?;
                            Ok((block, loader.load(&configs, block).await?))
                        }
                        .await;
                        Job::Reload(configs, result)
                    }));
                }
            }

            tokio::select! {
                msg = sub.recv() => {
                    let log = msg.map_err(|e| subscription_error(&e))?;
                    handle_log(&mut tracker, &log);
                    loop {
                        match sub.try_recv() {
                            Ok(log) => handle_log(&mut tracker, &log),
                            Err(TryRecvError::Empty) => break,
                            Err(TryRecvError::Lagged(n)) => {
                                return Err(subscription_error(&RecvError::Lagged(n)));
                            }
                            Err(TryRecvError::Closed) => {
                                return Err(subscription_error(&RecvError::Closed));
                            }
                        }
                    }
                    self.publish(&mut tracker);
                }

                Some(job) = jobs.next(), if !jobs.is_empty() => match job {
                    Job::Reload(configs, Ok((block, pools))) => {
                        reloading = false;
                        for (config, pool) in configs.iter().zip(pools) {
                            tracker.install(config, pool, block);
                        }
                        self.publish(&mut tracker);
                        debug!(block, pools = configs.len(), "pools reloaded");
                    }
                    Job::Reload(configs, Err(err)) => {
                        if matches!(err, KairosError::ConfigError(_)) {
                            return Err(err);
                        }
                        warn!(error = %err, "pool reload failed; retrying");
                        tracker.reload_failed(&configs);
                        jobs.push(Box::pin(async {
                            sleep(RELOAD_RETRY).await;
                            Job::ReloadRetryReady
                        }));
                    }
                    Job::ReloadRetryReady => reloading = false,
                    Job::Fees(result) => {
                        fees_busy = false;
                        match result {
                            Ok(fees) => {
                                for (pool, fee) in fees {
                                    tracker.set_fee(&pool, fee);
                                }
                                self.publish(&mut tracker);
                            }
                            Err(err) => warn!(error = %err, "fee refresh failed"),
                        }
                    }
                    Job::Audit(result) => {
                        audit_busy = false;
                        match result {
                            Ok((to_block, logs)) => {
                                let decoded: Vec<PoolLog> =
                                    logs.iter().filter_map(decode).collect();
                                tracker.audit(&decoded, to_block);
                                self.publish(&mut tracker);
                            }
                            Err(err) => warn!(error = %err, "log audit failed"),
                        }
                    }
                },

                _ = fee_tick.tick() => {
                    let pools = tracker.dynamic_fee_pools();
                    if !fees_busy && !pools.is_empty() {
                        fees_busy = true;
                        let loader = self.loader.clone();
                        jobs.push(Box::pin(async move { Job::Fees(loader.fees(&pools).await) }));
                    }
                }

                _ = audit_tick.tick() => {
                    if !audit_busy {
                        audit_busy = true;
                        let provider = self.provider.clone();
                        let filter = filter.clone();
                        let from = tracker.audited_to() + 1;
                        jobs.push(Box::pin(async move {
                            Job::Audit(fetch_audit_logs(provider, filter, from, audit_range).await)
                        }));
                    }
                }
            }
        }
    }
}

/// Range settings an audit job needs, copied so the job is `'static`.
#[derive(Clone, Copy)]
struct AuditRange {
    lag: u64,
    max_range: u64,
}

/// Canonical logs for `from ..= head - lag`, and the block they run to.
async fn fetch_audit_logs<P: Provider>(
    provider: P,
    filter: Filter,
    from: u64,
    range: AuditRange,
) -> Result<(u64, Vec<Log>)> {
    let rpc = |e: alloy::transports::TransportError| {
        KairosError::NodeConnectionFailed(format!("log audit: {e}"))
    };
    let head = provider.get_block_number().await.map_err(rpc)?;
    let to = head.saturating_sub(range.lag);
    let mut logs = Vec::new();
    let mut start = from;
    while start <= to {
        let end = to.min(start + range.max_range - 1);
        let chunk = filter.clone().from_block(start).to_block(end);
        logs.extend(provider.get_logs(&chunk).await.map_err(rpc)?);
        start = end + 1;
    }
    Ok((to.max(from - 1), logs))
}

fn decode(log: &Log) -> Option<PoolLog> {
    PoolLog::decode(log)
        .inspect_err(|e| warn!(error = %e, "skipping undecodable log"))
        .ok()
}

fn handle_log(tracker: &mut Tracker, log: &Log) {
    if let Some(log) = decode(log) {
        tracker.apply(&log);
    }
}

fn subscription_error(e: &RecvError) -> KairosError {
    KairosError::NodeConnectionFailed(match e {
        RecvError::Lagged(n) => format!("log subscription fell behind and dropped {n} logs"),
        RecvError::Closed => "log subscription closed".into(),
    })
}
