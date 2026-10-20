//! Phase 0 gate on Zakura regtest. Needs a fresh node (heights matter: NU7 is at 200):
//!
//! ```sh
//! infra/zakura/scripts/down.sh; infra/zakura/scripts/up.sh
//! cargo test -p zdlc --release --test regtest -- --ignored --nocapture
//! ```
//!
//! Gate: a refund confirms after its lock height and not before, and an
//! adaptor-completed CET confirms. Also: the cooperative close pays Ironwood (before and
//! after NU7), and a CET signed for one branch ID is only valid under that branch.

mod common;

use common::*;
use serde_json::json;
use zdlc::{
    keys,
    node::Node,
    oracle,
    params::{self, BRANCH_NU6_3, BRANCH_NU7, NU7_AT},
    tx::{self, Coin, Pay, Spec, Spend},
    txid_from_hex, txid_hex,
};

const COINBASE_MATURITY: u32 = 100;
const FUNDING: u64 = 2_0000_0000;

struct Chain {
    node: Node,
    p: zcash_protocol::local_consensus::LocalNetwork,
    next_coinbase: u32,
    log: Vec<serde_json::Value>,
}

impl Chain {
    async fn tip(&self) -> u32 {
        self.node.height().await.unwrap()
    }

    fn note(&mut self, step: &str, v: serde_json::Value) {
        println!("[{step}] {v}");
        self.log.push(json!({"step": step, "result": v}));
    }

    /// The next mature coinbase output (vout 0 pays the miner key).
    async fn coinbase(&mut self) -> Coin {
        let h = self.next_coinbase;
        self.next_coinbase += 1;
        assert!(self.tip().await + 1 >= h + COINBASE_MATURITY, "coinbase {h} immature");
        let b = self.node.call("getblock", json!([h.to_string(), 2])).await.unwrap();
        let cb = &b["tx"][0];
        let value = (cb["vout"][0]["valueZat"].as_u64())
            .unwrap_or_else(|| (cb["vout"][0]["value"].as_f64().unwrap() * 1e8).round() as u64);
        Coin {
            txid: txid_from_hex(cb["txid"].as_str().unwrap()).unwrap(),
            index: 0,
            value,
            spend: Spend::P2pkh(keys::pubkey(&keys::miner())),
        }
    }

    /// Builds and signs (does not send) a funding tx: coinbase -> 2-of-2 + change.
    async fn funding(&mut self, parties: &Parties) -> (tx::Final, Coin) {
        let cb = self.coinbase().await;
        let tip = self.tip().await;
        let miner = keys::p2pkh(&keys::pubkey(&keys::miner()));
        let p2sh = zdlc::script::p2sh(&parties.redeem());
        let mut spec = Spec {
            target_height: tip + 1,
            expiry_height: tip + 20,
            lock_height: None,
            inputs: vec![cb.clone()],
            outputs: vec![Pay::Transparent(p2sh, FUNDING), Pay::Transparent(miner, 1)],
            ironwood_anchor: None,
        };
        let fee = tx::fee(&self.p, &spec).unwrap();
        spec.outputs[1] = Pay::Transparent(miner, cb.value - FUNDING - fee);
        let f = tx::extract(tx::sign(tx::build(&self.p, &spec).unwrap(), 0, &keys::miner()).unwrap()).unwrap();
        let coin = parties.funding_coin(f.txid, 0, FUNDING);
        (f, coin)
    }

    async fn send(&self, f: &tx::Final) -> Result<String, String> {
        self.node.send(&f.raw).await.map_err(|e| e.to_string())
    }

    async fn confirm(&self, f: &tx::Final) -> u32 {
        self.node.generate(1).await.unwrap();
        self.node
            .mined_height(&txid_hex(&f.txid))
            .await
            .unwrap()
            .expect("mined")
    }
}

#[tokio::test]
#[ignore = "needs infra/zakura (fresh chain)"]
async fn phase0_gate() {
    let url = std::env::var("ZDLC_NODE_URL").unwrap_or("http://127.0.0.1:18252/".into());
    let mut c = Chain {
        node: Node::new(url),
        p: params::regtest(Some(NU7_AT)),
        next_coinbase: 1,
        log: vec![],
    };
    let info = c.node.blockchain_info().await.expect("node up: infra/zakura/scripts/up.sh");
    assert_eq!(info["upgrades"][format!("{BRANCH_NU7:08x}")]["activationheight"], NU7_AT);
    assert!(c.tip().await < 100, "needs a fresh chain: down.sh && up.sh");
    c.node.mine_to(110).await.unwrap();
    let miner = keys::p2pkh(&keys::pubkey(&keys::miner()));

    // ---- 1. Refund: confirms after its lock height, not before ------------------------
    let parties = Parties::new("rt-refund");
    let (fund, coin) = c.funding(&parties).await;
    let lock = c.tip().await + 6;
    assert!(!params::spans_upgrade(&c.p, c.tip().await, lock));
    let a = keys::p2pkh(&keys::pubkey(&parties.a));
    let b = keys::p2pkh(&keys::pubkey(&parties.b));
    let refund_spec = split(&c.p, c.tip().await + 1, 0, Some(lock), vec![coin], &[(a, 500), (b, 500)]);
    let refund = tx::extract(cosign(tx::build(&c.p, &refund_spec).unwrap(), &parties)).unwrap();
    // Funding goes out last, once the refund is signed.
    let sent = c.send(&fund).await;
    c.note("unshielded coinbase -> 2-of-2 funding", json!({"accepted": sent.is_ok(), "detail": format!("{sent:?}")}));
    sent.expect("funding accepted");
    let h_fund = c.confirm(&fund).await;
    c.node.mine_to(lock - 1).await.unwrap();
    let early = c.send(&refund).await;
    c.note("refund at tip = lock-1", json!({"tip": c.tip().await, "lock": lock, "accepted": early.is_ok(), "detail": format!("{early:?}")}));
    assert!(early.is_err(), "refund must not be accepted before its lock height");
    c.node.generate(1).await.unwrap();
    let ok = c.send(&refund).await;
    c.note("refund at tip = lock", json!({"tip": c.tip().await, "accepted": ok.is_ok(), "detail": format!("{ok:?}")}));
    ok.expect("refund accepted at its lock height");
    let h_refund = c.confirm(&refund).await;
    c.note("refund confirmed", json!({"funding_height": h_fund, "lock": lock, "mined_height": h_refund, "txid": txid_hex(&refund.txid), "branch": format!("{:08x}", refund.branch_id)}));
    assert_eq!(h_refund, lock + 1);

    // ---- 2. Adaptor-completed CET confirms --------------------------------------------
    let parties = Parties::new("rt-cet");
    let (fund, coin) = c.funding(&parties).await;
    let mut o = oracle::DevOracle::new("rt", "coin-flip-1", &["heads", "tails"]);
    let ann = o.announcement().clone();
    let a = keys::p2pkh(&keys::pubkey(&parties.a));
    let b = keys::p2pkh(&keys::pubkey(&parties.b));
    let t = c.tip().await + 1;
    let cets = presign_cets(
        &parties,
        &ann,
        vec![
            ("heads".into(), split(&c.p, t, 0, None, vec![coin.clone()], &[(a, 700), (b, 300)])),
            ("tails".into(), split(&c.p, t, 0, None, vec![coin.clone()], &[(a, 300), (b, 700)])),
        ],
        &c.p,
    );
    c.send(&fund).await.expect("funding");
    c.confirm(&fund).await;
    let s = o.attest("heads").unwrap();
    let (cet, high) = complete_cet(&cets[0], &parties, &s);
    c.send(&cet).await.expect("CET accepted");
    let h_cet = c.confirm(&cet).await;
    let v = c.node.tx(&txid_hex(&cet.txid)).await.unwrap();
    let outs: Vec<u64> = v["vout"].as_array().unwrap().iter().map(|o| o["valueZat"].as_u64().unwrap_or(0)).collect();
    c.note("adaptor CET confirmed", json!({"outcome": "heads", "mined_height": h_cet, "txid": txid_hex(&cet.txid), "branch": format!("{:08x}", cet.branch_id), "bytes": cet.raw.len(), "outputs_zat": outs, "decrypted_high_s": high}));

    // ---- 3. Cooperative close to Ironwood (NU6.3) ---------------------------------------
    close(&mut c, "rt-close-nu6.3").await;

    // ---- 4. NU7: a CET is valid only under the branch it was signed for -----------------
    let parties = Parties::new("rt-nu7");
    let (fund, coin) = c.funding(&parties).await;
    let mut o = oracle::DevOracle::new("rt", "coin-flip-2", &["heads", "tails"]);
    let ann = o.announcement().clone();
    let a = keys::p2pkh(&keys::pubkey(&parties.a));
    let b = keys::p2pkh(&keys::pubkey(&parties.b));
    let t = c.tip().await + 1;
    assert!(t < NU7_AT - 5);
    let mk = |target| split(&c.p, target, 0, None, vec![coin.clone()], &[(a, 700), (b, 300)]);
    let pre = presign_cets(&parties, &ann, vec![("heads".into(), mk(t))], &c.p);
    let post = presign_cets(&parties, &ann, vec![("heads".into(), mk(NU7_AT))], &c.p);
    c.send(&fund).await.expect("funding");
    c.confirm(&fund).await;
    let s = o.attest("heads").unwrap();
    let (cet63, _) = complete_cet(&pre[0], &parties, &s);
    let (cet7, _) = complete_cet(&post[0], &parties, &s);
    assert_eq!((cet63.branch_id, cet7.branch_id), (BRANCH_NU6_3, BRANCH_NU7));
    let r = c.send(&cet7).await;
    c.note("NU7-signed CET before activation", json!({"tip": c.tip().await, "accepted": r.is_ok(), "detail": format!("{r:?}")}));
    assert!(r.is_err());
    c.node.mine_to(NU7_AT - 1).await.unwrap();
    let r = c.send(&cet63).await;
    c.note("NU6.3-signed CET once the next block is NU7", json!({"tip": c.tip().await, "accepted": r.is_ok(), "detail": format!("{r:?}")}));
    assert!(r.is_err());
    let r = c.send(&cet7).await;
    c.note("NU7-signed CET once the next block is NU7", json!({"tip": c.tip().await, "accepted": r.is_ok(), "detail": format!("{r:?}")}));
    r.expect("NU7 CET accepted");
    let h7 = c.confirm(&cet7).await;
    let info = c.node.blockchain_info().await.unwrap();
    c.note("NU7 CET confirmed", json!({"mined_height": h7, "chaintip_branch": info["consensus"]["chaintip"], "txid": txid_hex(&cet7.txid)}));
    assert_eq!(h7, NU7_AT);

    // ---- 5. Cooperative close to Ironwood (NU7) -----------------------------------------
    close(&mut c, "rt-close-nu7").await;
    let _ = miner;

    if let Ok(path) = std::env::var("ZDLC_RESULTS") {
        std::fs::write(path, serde_json::to_string_pretty(&c.log).unwrap()).unwrap();
    }
}

async fn close(c: &mut Chain, tag: &str) {
    let parties = Parties::new(tag);
    let (fund, coin) = c.funding(&parties).await;
    c.send(&fund).await.expect("funding");
    c.confirm(&fund).await;
    let before = c.node.pool_zat("ironwood").await.unwrap_or(-1);
    let t = c.tip().await + 1;
    let (spec, _) = close_to_ironwood(&c.p, t, t + 20, vec![coin], orchard::Anchor::empty_tree());
    let t0 = std::time::Instant::now();
    let f = tx::extract(cosign(tx::build(&c.p, &spec).unwrap(), &parties)).unwrap();
    let prove = t0.elapsed();
    let r = c.send(&f).await;
    c.note(&format!("{tag}: P2SH -> Ironwood close"), json!({"accepted": r.is_ok(), "detail": format!("{r:?}"), "branch": format!("{:08x}", f.branch_id), "bytes": f.raw.len(), "build_prove_ms": prove.as_millis()}));
    r.expect("close accepted");
    let h = c.confirm(&f).await;
    let after = c.node.pool_zat("ironwood").await.unwrap_or(-1);
    c.note(&format!("{tag}: close confirmed"), json!({"mined_height": h, "ironwood_pool_before": before, "ironwood_pool_after": after, "txid": txid_hex(&f.txid)}));
}
