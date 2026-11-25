//! Cross-module checks on the shipped configuration: every chain with a pool
//! list has a profile, and the profile can price every pool on it.

use std::path::PathBuf;

use kairos_core::ChainProfile;
use kairos_state::StateConfig;

fn config_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config")
}

#[test]
fn every_pool_list_has_a_chain_profile_that_prices_it() {
    let pools_dir = config_dir().join("pools");
    let mut checked = 0;
    for entry in std::fs::read_dir(&pools_dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "toml") {
            continue;
        }
        let chain = path.file_stem().unwrap().to_str().unwrap();
        let profile = ChainProfile::load(config_dir().join(format!("chains/{chain}.toml")))
            .unwrap_or_else(|e| panic!("pool list {chain} has no usable chain profile: {e}"));
        let pools = StateConfig::load(&path).unwrap();
        for pool in &pools.pools {
            let kind = pool.kind.amm_kind();
            assert!(
                profile.gas.hop(kind).is_some(),
                "{chain}: pool {} is {kind:?}, which has no gas estimate in the profile",
                pool.address
            );
        }
        checked += 1;
    }
    assert!(checked > 0, "no pool lists in {}", pools_dir.display());
}
