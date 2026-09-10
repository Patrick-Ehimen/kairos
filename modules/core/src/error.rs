use alloy::primitives::Address;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum KairosError {
    #[error("Pool not found: {0}")]
    PoolNotFound(Address),

    #[error("Invalid token pair: {token_in} -> {token_out)")]
    InvalidTokenPair {
        token_in: Address,
        token_out: Address,
    },

    #[error("Insufficient liquidity in pool {0}")]
    InsufficientLiquidity(Address),

    #[error("No arbitrage opportunity found")]
    NoOpportunity,

    #[error("Profit below minimum threshold: {profit_wei} < {min_profit_wei}")]
    ProfitBelowThreshold {
        profit_wei: String,
        min_profit_wei: String,
    },

    #[error("Simulation failed: {0}")]
    SimulationFailed(String),

    #[error("EVM revert: {0}")]
    EvmRevert(String),

    #[error("Bundle submission failed: {0}")]
    BundleSubmissionFailed(String),

    #[error("Nonce mismatch: expected {expected}, got {actual}")]
    NonceMismatch { expected: u64, actual: u64 },

    #[error("Node connection failed: {0}")]
    NodeConnectionFailed(String),

    #[error("All nodes unhealthy")]
    AllNodesUnhealthy,

    #[error("Circuit breaker triggered: {0}")]
    CircuitBreakerTriggered(String),

    #[error("Position limit exceeded: {0}")]
    PositionLimitExceeded(String),

    #[error("Configuration error: {0}")]
    ConfigError(String),

    #[error("gRPC error: {0}")]
    GrpcError(String),

    #[error("Internal error: {0}")]
    Internal(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}