//! Error type shared across all Kairos crates.
//!
//! Every fallible operation in the workspace returns [`KairosError`] (usually
//! via the [`Result`] alias), so errors can flow from ingestion through
//! detection, simulation and execution without per-crate conversion layers.
//!
//! Variants are grouped by the pipeline stage that produces them. Their
//! [`Display`](std::fmt::Display) output is intended for logs and operator
//! dashboards, not for programmatic matching — match on the variant instead.

use alloy::primitives::Address;
use thiserror::Error;

/// Convenience alias for results whose error type is [`KairosError`].
pub type Result<T> = std::result::Result<T, KairosError>;

/// The unified error type for the Kairos arbitrage engine.
#[derive(Error, Debug)]
pub enum KairosError {
    // ---------------------------------------------------------------------
    // Pool state & routing
    // ---------------------------------------------------------------------
    /// No pool with this address is tracked in the pool registry / state store.
    #[error("Pool not found: {0}")]
    PoolNotFound(Address),

    /// The requested swap direction is not supported by the pool, e.g. one of
    /// the tokens is not part of the pair, or `token_in == token_out`.
    #[error("Invalid token pair: {token_in} -> {token_out}")]
    InvalidTokenPair {
        /// Token being sold into the pool.
        token_in: Address,
        /// Token expected out of the pool.
        token_out: Address,
    },

    /// The pool cannot fill the requested amount (reserves or in-range
    /// liquidity are too low to produce a non-zero output).
    #[error("Insufficient liquidity in pool {0}")]
    InsufficientLiquidity(Address),

    /// An on-chain event cannot be applied to the locally tracked pool state
    /// (for example a burn of more liquidity than the pool holds), so the
    /// local copy has drifted from the chain. Reload the pool from chain.
    #[error("State of pool {pool} is out of sync with chain: {reason}")]
    StateDesync { pool: Address, reason: String },

    // ---------------------------------------------------------------------
    // Detection
    // ---------------------------------------------------------------------
    /// The detector scanned the price graph and found no profitable cycle.
    ///
    /// This is the normal, expected outcome for most blocks and should not be
    /// logged at warning level or above.
    #[error("No arbitrage opportunity found")]
    NoOpportunity,

    /// A cycle was found, but its net profit (after gas) does not clear the
    /// configured minimum.
    ///
    /// Values are carried as decimal strings so arbitrarily large `U256`
    /// amounts render exactly in logs.
    #[error("Profit below minimum threshold: {profit_wei} < {min_profit_wei}")]
    ProfitBelowThreshold {
        /// Net profit of the candidate, in wei.
        profit_wei: String,
        /// Configured minimum net profit, in wei.
        min_profit_wei: String,
    },

    /// A [`SwapStep`](crate::types::SwapStep) breaks a rule the on-chain
    /// executor enforces and would revert if submitted.
    #[error("Invalid swap step for pool {pool}: {reason}")]
    InvalidSwapStep {
        /// Pool the offending step targets.
        pool: Address,
        /// Which rule was broken.
        reason: String,
    },

    // ---------------------------------------------------------------------
    // Simulation
    // ---------------------------------------------------------------------
    /// The EVM simulation could not run to completion (fork setup failure,
    /// missing state, database error, …). Distinct from [`Self::EvmRevert`],
    /// where the simulation ran but the transaction reverted.
    #[error("Simulation failed: {0}")]
    SimulationFailed(String),

    /// The simulated transaction reverted. Contains the decoded revert reason
    /// when available, otherwise the raw revert data as hex.
    #[error("EVM revert: {0}")]
    EvmRevert(String),

    // ---------------------------------------------------------------------
    // Execution & submission
    // ---------------------------------------------------------------------
    /// A bundle or transaction was rejected by a relay / builder / RPC.
    #[error("Bundle submission failed: {0}")]
    BundleSubmissionFailed(String),

    /// The locally tracked nonce for the searcher account disagrees with the
    /// chain. Usually means another transaction from the same key landed.
    #[error("Nonce mismatch: expected {expected}, got {actual}")]
    NonceMismatch {
        /// Nonce Kairos expected to use.
        expected: u64,
        /// Nonce reported by the node.
        actual: u64,
    },

    // ---------------------------------------------------------------------
    // Node connectivity
    // ---------------------------------------------------------------------
    /// Connecting to (or a request against) a specific node failed.
    #[error("Node connection failed: {0}")]
    NodeConnectionFailed(String),

    /// A log from the node could not be decoded into a known pool event
    /// (unknown topic, malformed data, or missing block position).
    #[error("Invalid event log: {0}")]
    InvalidLog(String),

    /// Every configured node is unhealthy; the engine cannot observe the chain.
    #[error("All nodes unhealthy")]
    AllNodesUnhealthy,

    // ---------------------------------------------------------------------
    // Risk management
    // ---------------------------------------------------------------------
    /// A circuit breaker tripped and execution is suspended. Contains the
    /// breaker that fired and why.
    #[error("Circuit breaker triggered: {0}")]
    CircuitBreakerTriggered(String),

    /// The trade would exceed a configured exposure limit (per-trade size,
    /// per-token inventory, daily loss, …).
    #[error("Position limit exceeded: {0}")]
    PositionLimitExceeded(String),

    // ---------------------------------------------------------------------
    // Infrastructure
    // ---------------------------------------------------------------------
    /// Configuration is missing, malformed, or internally inconsistent.
    #[error("Configuration error: {0}")]
    ConfigError(String),

    /// A gRPC call to or from the server module failed.
    #[error("gRPC error: {0}")]
    GrpcError(String),

    /// An invariant was violated. Indicates a bug rather than an
    /// environmental failure.
    #[error("Internal error: {0}")]
    Internal(String),

    /// An underlying I/O error, converted automatically via `?`.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::Address;

    #[test]
    fn test_pool_not_found_display() {
        let err = KairosError::PoolNotFound(Address::ZERO);
        let msg = err.to_string();
        assert!(msg.contains("Pool not found"));
        assert!(msg.contains(&Address::ZERO.to_string()));
    }

    #[test]
    fn test_no_opportunity_display() {
        let err = KairosError::NoOpportunity;
        assert_eq!(err.to_string(), "No arbitrage opportunity found");
    }

    #[test]
    fn test_invalid_token_pair_display() {
        let err = KairosError::InvalidTokenPair {
            token_in: Address::ZERO,
            token_out: Address::ZERO,
        };
        let msg = err.to_string();
        assert!(msg.contains("Invalid token pair"));
    }

    #[test]
    fn test_insufficient_liquidity_display() {
        let err = KairosError::InsufficientLiquidity(Address::ZERO);
        assert!(err.to_string().contains("Insufficient liquidity"));
    }

    #[test]
    fn test_state_desync_display() {
        let err = KairosError::StateDesync {
            pool: Address::ZERO,
            reason: "liquidity underflow".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("out of sync"));
        assert!(msg.contains("liquidity underflow"));
    }

    #[test]
    fn test_profit_below_threshold_display() {
        let err = KairosError::ProfitBelowThreshold {
            profit_wei: "100".to_string(),
            min_profit_wei: "1000".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("100"));
        assert!(msg.contains("1000"));
    }
}
