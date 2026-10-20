#![allow(dead_code)]

use orchard::keys::{FullViewingKey, Scope, SpendingKey};
use pczt::Pczt;
use secp256k1::SecretKey;
use zcash_protocol::local_consensus::LocalNetwork;
use zdlc::{
    adaptor, keys, oracle, script,
    tx::{self, Coin, Pay, Spec, Spend},
};

pub struct Parties {
    pub a: SecretKey,
    pub b: SecretKey,
}

impl Parties {
    pub fn new(tag: &str) -> Self {
        Self {
            a: keys::dev_key(&format!("{tag}/alice")),
            b: keys::dev_key(&format!("{tag}/bob")),
        }
    }

    pub fn redeem(&self) -> zcash_script::script::FromChain {
        script::multisig_2of2(&keys::pubkey(&self.a), &keys::pubkey(&self.b)).unwrap()
    }

    pub fn funding_coin(&self, txid: [u8; 32], index: u32, value: u64) -> Coin {
        Coin {
            txid,
            index,
            value,
            spend: Spend::P2sh(self.redeem()),
        }
    }
}

/// A spec spending `inputs` to transparent `payees` in the given shares (per mille), the
/// ZIP 317 fee taken from the total before splitting; dust-free by construction here.
pub fn split(
    params: &LocalNetwork,
    target_height: u32,
    expiry_height: u32,
    lock_height: Option<u32>,
    inputs: Vec<Coin>,
    payees: &[(zcash_transparent::address::TransparentAddress, u64)],
) -> Spec {
    let mut spec = Spec {
        target_height,
        expiry_height,
        lock_height,
        inputs,
        outputs: payees
            .iter()
            .map(|(a, _)| Pay::Transparent(*a, 1))
            .collect(),
        ironwood_anchor: None,
    };
    let total: u64 = spec.inputs.iter().map(|c| c.value).sum::<u64>() - tx::fee(params, &spec).unwrap();
    let mut left = total;
    for (i, (a, permille)) in payees.iter().enumerate() {
        let v = if i + 1 == payees.len() { left } else { total * permille / 1000 };
        left -= v;
        spec.outputs[i] = Pay::Transparent(*a, v);
    }
    spec
}

/// Pays the whole input to two fresh Ironwood addresses, half each, minus the fee.
pub fn close_to_ironwood(
    params: &LocalNetwork,
    target_height: u32,
    expiry_height: u32,
    inputs: Vec<Coin>,
    anchor: orchard::Anchor,
) -> (Spec, [orchard::Address; 2]) {
    let addr = |seed: u8| {
        let sk = SpendingKey::from_bytes([seed; 32]).unwrap();
        FullViewingKey::from(&sk).address_at(0u32, Scope::External)
    };
    let to = [addr(7), addr(9)];
    let mut spec = Spec {
        target_height,
        expiry_height,
        lock_height: None,
        inputs,
        outputs: vec![Pay::Ironwood(to[0], 1), Pay::Ironwood(to[1], 1)],
        ironwood_anchor: Some(anchor),
    };
    let total = spec.inputs.iter().map(|c| c.value).sum::<u64>() - tx::fee(params, &spec).unwrap();
    spec.outputs = vec![Pay::Ironwood(to[0], total / 2), Pay::Ironwood(to[1], total - total / 2)];
    (spec, to)
}

/// Both parties sign input 0 with their keys.
pub fn cosign(pczt: Pczt, p: &Parties) -> Pczt {
    let pczt = tx::sign(pczt, 0, &p.a).unwrap();
    tx::sign(pczt, 0, &p.b).unwrap()
}

/// One CET per outcome: Bob's adaptor signature on each, encrypted to that outcome's
/// attestation point and checked by Alice, as at setup.
pub struct Cet {
    pub outcome: String,
    pub pczt: Pczt,
    pub bob_adaptor: adaptor::AdaptorSignature,
}

pub fn presign_cets(
    p: &Parties,
    ann: &oracle::Announcement,
    specs: Vec<(String, Spec)>,
    params: &LocalNetwork,
) -> Vec<Cet> {
    specs
        .into_iter()
        .map(|(outcome, spec)| {
            let pczt = tx::build(params, &spec).unwrap();
            let sighash = tx::sighash(&pczt, 0).unwrap();
            let point = oracle::attestation_point(ann, &outcome).unwrap();
            let bob_adaptor = adaptor::encrypt(&p.b, sighash, &point);
            adaptor::verify(&bob_adaptor, sighash, &keys::pubkey(&p.b), &point).unwrap();
            Cet { outcome, pczt, bob_adaptor }
        })
        .collect()
}

/// Alice completes the CET for the attested outcome: decrypts Bob's adaptor signature
/// with `s_m`, adds her own, finalizes. Returns the tx and whether decryption came out
/// high-S (normalized before appending).
pub fn complete_cet(cet: &Cet, p: &Parties, s_m: &secp256k1_zkp::SecretKey) -> (tx::Final, bool) {
    let d = adaptor::decrypt(&cet.bob_adaptor, s_m).unwrap();
    let pczt = tx::append(cet.pczt.clone(), 0, d.signature).unwrap();
    let pczt = tx::sign(pczt, 0, &p.a).unwrap();
    (tx::extract(pczt).unwrap(), d.was_high_s)
}
