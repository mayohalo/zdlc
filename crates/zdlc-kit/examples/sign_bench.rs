//! Times a hedge contract's setup (offer, accept, sign, finish) offline, on one thread,
//! at several payout roundings: what the hedge desk's agents do before funding.
use std::time::Instant;

use zdlc::{
    contract::{Party, PartyInfo},
    events::MemoryOracle,
    keys, params,
    templates::Template,
    tx::{Coin, Spend},
    wallet::Wallet,
};

fn party(w: &Wallet, i: u32, coin: u8) -> Party {
    let pk = keys::pubkey(&w.coin_key(0));
    let inputs = vec![Coin { txid: [coin; 32], index: 0, value: 1_000_000, spend: Spend::P2pkh(pk) }];
    Party {
        funding_sk: w.funding_key(i),
        input_keys: vec![w.coin_key(0)],
        close_fvk: w.close_fvk(i),
        roll_key: None,
        ironwood: None,
        info: PartyInfo {
            funding_pubkey: keys::pubkey(&w.funding_key(i)),
            inputs,
            change: w.coin_address(0),
            payout: w.payout_address(i),
            close: w.close_address(i),
            ironwood: false,
        },
    }
}

fn main() {
    let p = params::testnet();
    let tip = 4_547_000;
    let oracles = [MemoryOracle::new([1; 32]), MemoryOracle::new([2; 32])];
    for (n, bps) in [(1i64, 200u64), (2, 500), (3, 1000)] {
        let t = Template::StablePayment {
            usd_cents: 124,
            collateral_zat: 100_000,
            premium_zat: 500,
            maturity_blocks: 2,
            refund_blocks: 24,
            price_minute: Some(1_793_000_000 + n * 60),
            rounding_bps: Some(bps),
        };
        let spec = t.event().unwrap();
        let anns: Vec<_> = oracles.iter().map(|o| o.announce(&format!("bench-{n}"), &spec)).collect();
        let terms = t.terms(&anns, tip + 1).unwrap();
        let (taker, maker) = (Wallet::from_seed([10 + n as u8; 32]), Wallet::from_seed([20 + n as u8; 32]));
        let (tp, mp) = (party(&taker, n as u32, 1), party(&maker, n as u32, 2));
        let t0 = Instant::now();
        let offer = tp.offer(terms);
        let (accept, txs) = mp.accept(&p, tip, &offer).unwrap();
        let t1 = Instant::now();
        let (sign, _) = tp.sign(&p, tip, &offer, &accept).unwrap();
        let t2 = Instant::now();
        let built = zdlc::contract::build_txs(&p, &offer.terms, &offer.offer, &accept.accept).unwrap();
        mp.finish(&offer, built, &sign).unwrap();
        let t3 = Instant::now();
        println!(
            "rounding {bps:>4} bps: {:>4} CETs | maker accept {:>5.1}s | taker sign {:>5.1}s | maker finish {:>5.1}s | total {:>5.1}s",
            txs.cets.len(),
            (t1 - t0).as_secs_f64(),
            (t2 - t1).as_secs_f64(),
            (t3 - t2).as_secs_f64(),
            (t3 - t0).as_secs_f64()
        );
    }
}
