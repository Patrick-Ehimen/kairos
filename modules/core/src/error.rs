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

#[cfg(tests)]
mod tests {
    use super::*;
    use alloy::primitives::Address;

    #[test]
    fn test_pool_not_found_display() {
        let err = KairosError::PoolNotFound(Address::ZERO);
        let msg = err.to_string();
        assert!(msg.contains("pool not found"));
        assert!(msg.contains(&Address::ZERO.to_string()));
    }

    #[test]
    fn test_no_opportunity_display() {
        let err = AetherError::NoOpportunity;
        assert_eq!(err.to_string(), "No arbitrage opportunity found");
    }

    #[test]
    fn test_invalid_token_pair_display() {
        let err = AetherError::InvalidTokenPair {
            token_in: Address::ZERO,
            token_out: Address::ZERO,
        };
        let msg = err.to_string();
        assert!(msg.contains("Invalid token pair"));
    }

    #[test]
    fn test_insufficient_liquidity_display() {
        let err = AetherError::InsufficientLiquidity(Address::ZERO);
        assert!(err.to_string().contains("Insufficient liquidity"));
    }

    #[test]
    fn test_profit_below_threshold_display() {
        let err = AetherError::ProfitBelowThreshold {
            profit_wei: "100".to_string(),
            min_profit_wei: "1000".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("100"));
        assert!(msg.contains("1000"));
    }
}