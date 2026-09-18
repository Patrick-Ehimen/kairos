use alloy::primitives::{Address, U256};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum ProtocolType {
    #[default]
    UniswapV2 = 1,
    UniswapV3 = 2,
    SushiSwap = 3,
    Curve = 4,
    BalancerV2 = 5,
    BancorV3 = 6,
}

impl ProtocolType {
    pub fn base_gas(&self) -> u64 {
        match self {
            ProtocolType::UniswapV2 => 60_000,
            ProtocolType::UniswapV3 => 180_000,
            ProtocolType::SushiSwap => 60_000,
            ProtocolType::Curve => 130_000,
            ProtocolType::BalancerV2 => 120_000,
            ProtocolType::BancorV3 => 150_000,
        }
    }
}

pub fn known_token_decimals(addr: &Address) -> Option<u8> {
    use addresses::{DAI, USDC, USDT, WBTC, WETH};
    match *addr {
        WETH => Some(18),
        USDC => Some(6),
        USDT => Some(6),
        DAI => Some(18),
        WBTC => Some(8),
        _ => None,
    }
}

/// Unique pool identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PoolId {
    pub address: Address,
    pub protocol: ProtocolType,
}

/// Token with metadata
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Token {
    pub address: Address,
    pub decimals: u8,
    pub symbol: String,
}