//! Consensus parameters for the zdlc regtest chain (`infra/zakura/regtest.toml`).

use zcash_protocol::consensus::{BlockHeight, BranchId};
use zcash_protocol::local_consensus::LocalNetwork;

/// NU7 activation height on the zdlc regtest chain. Everything up to NU6.3 (Ironwood)
/// activates at height 1, so heights 1..NU7_AT sign under NU6.3's branch ID and
/// NU7_AT onward under NU7's.
pub const NU7_AT: u32 = 200;

/// NU6.3 "Ironwood" branch ID.
pub const BRANCH_NU6_3: u32 = 0x37A5_165B;
/// NU7 branch ID.
pub const BRANCH_NU7: u32 = 0x7719_0AD9;

/// Largest collateral per side the testnet runner accepts (0.05 TAZ). Mainnet gets its
/// own, smaller cap when it is enabled.
pub const TESTNET_CAP: u64 = 500_0000;

/// Zcash testnet, as a `LocalNetwork` so every builder takes one parameter type. The
/// heights are the pinned zcash_protocol's (NU6.3 at 4,134,000, NU7 at 4,465,026).
pub fn testnet() -> LocalNetwork {
    use zcash_protocol::consensus::{NetworkUpgrade as U, Parameters, TEST_NETWORK};
    let h = |u| TEST_NETWORK.activation_height(u);
    LocalNetwork {
        overwinter: h(U::Overwinter),
        sapling: h(U::Sapling),
        blossom: h(U::Blossom),
        heartwood: h(U::Heartwood),
        canopy: h(U::Canopy),
        nu5: h(U::Nu5),
        nu6: h(U::Nu6),
        nu6_1: h(U::Nu6_1),
        nu6_2: h(U::Nu6_2),
        nu6_3: h(U::Nu6_3),
        nu7: h(U::Nu7),
    }
}

pub fn regtest(nu7: Option<u32>) -> LocalNetwork {
    let one = Some(BlockHeight::from_u32(1));
    LocalNetwork {
        overwinter: one,
        sapling: one,
        blossom: one,
        heartwood: one,
        canopy: one,
        nu5: one,
        nu6: one,
        nu6_1: one,
        nu6_2: one,
        nu6_3: one,
        nu7: nu7.map(BlockHeight::from_u32),
    }
}

/// The branch ID a transaction targeting `height` signs under.
pub fn branch_at(params: &LocalNetwork, height: u32) -> u32 {
    u32::from(BranchId::for_height(params, BlockHeight::from_u32(height)))
}

/// Whether `[from, to]` crosses the next network upgrade after `from`: the rule that a
/// contract's maturity and refund must fall before the next activation height.
pub fn spans_upgrade(params: &LocalNetwork, from: u32, to: u32) -> bool {
    BranchId::for_height(params, BlockHeight::from_u32(from))
        .height_bounds(params)
        .and_then(|(_, next)| next)
        .is_some_and(|next| to >= u32::from(next))
}

/// The activation height of the next network upgrade after `height`, if scheduled.
pub fn next_activation(params: &LocalNetwork, height: u32) -> Option<u32> {
    BranchId::for_height(params, BlockHeight::from_u32(height))
        .height_bounds(params)
        .and_then(|(_, next)| next)
        .map(u32::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zcash_protocol::consensus::Parameters;

    #[test]
    fn branches_and_upgrade_guard() {
        let p = regtest(Some(NU7_AT));
        assert_eq!(branch_at(&p, 10), BRANCH_NU6_3);
        assert_eq!(branch_at(&p, NU7_AT), BRANCH_NU7);
        assert!(spans_upgrade(&p, 150, NU7_AT));
        assert!(!spans_upgrade(&p, 150, NU7_AT - 1));
        assert!(!spans_upgrade(&p, NU7_AT, 10_000));
        let t = testnet();
        assert_eq!(branch_at(&t, 4_521_962), BRANCH_NU7);
        assert_eq!(branch_at(&t, 4_465_025), BRANCH_NU6_3);
        let _ = p.network_type();
    }
}
