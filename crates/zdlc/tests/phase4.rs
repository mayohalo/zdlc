//! Phase 4 on Zakura regtest: two counterparties trade through the desk. Every party is
//! its own process with its own seed: the desk, two oracles, a maker, and a taker run
//! three times: settled by cooperative close, by CET, and by a close whose taker side
//! pays two addresses (60 % to the taker, 40 % to a partner's Ironwood receiver). The
//! test only funds the wallets and mines a block a second. Needs internet access for the
//! ZEC/USD feed:
//!
//! ```sh
//! infra/zakura/scripts/up.sh
//! cargo test -p zdlc --release --test phase4 -- --ignored --nocapture
//! ```

use std::process::{Child, Command, Stdio};

use serde_json::Value;
use orchard::keys::Scope;
use zdlc::{feeds, keys, params::NU7_AT, regtest::{dev_beneficiary, Regtest}, wallet::Wallet};

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

fn spawn(bin: &str, args: &[&str]) -> Proc {
    Proc(Command::new(bin).args(args).stdout(Stdio::inherit()).stderr(Stdio::inherit()).spawn().expect(bin))
}

async fn wait_http(url: &str) {
    for _ in 0..100 {
        if reqwest::get(url).await.is_ok() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("{url} did not start");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs infra/zakura and internet access"]
async fn phase4_two_counterparties_trade_through_the_desk() {
    let node = std::env::var("ZDLC_NODE_URL").unwrap_or("http://127.0.0.1:18252/".into());
    let rt = Regtest::connect(&node).await.expect("infra/zakura/scripts/up.sh");
    rt.node.mine_to(NU7_AT + 10).await.unwrap();
    let dir = std::env::temp_dir().join(format!("zdlc-phase4-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = |f: &str| dir.join(f).to_str().unwrap().to_string();

    // Two wallets that know nothing of each other.
    let (maker_seed, taker_seed) = ([0x4Du8; 32], [0x7Au8; 32]);
    std::fs::write(path("maker.seed"), hex::encode(maker_seed)).unwrap();
    std::fs::write(path("taker.seed"), hex::encode(taker_seed)).unwrap();
    let (mw, tw) = (Wallet::from_seed(maker_seed), Wallet::from_seed(taker_seed));
    rt.pay(&[(mw.coin_address(0), 3000_0000), (mw.coin_address(0), 3000_0000), (mw.coin_address(0), 3000_0000),
        (tw.coin_address(0), 3000_0000), (tw.coin_address(0), 3000_0000), (tw.coin_address(0), 3000_0000)])
        .await
        .unwrap();

    // Infrastructure: the desk and two oracles.
    let _desk = spawn(env!("CARGO_BIN_EXE_zdlc-desk"), &["--listen", "127.0.0.1:17900", "--state", &path("desk.json"), "--node", &node]);
    let _oa = spawn(env!("CARGO_BIN_EXE_zdlc-oracled"), &["--listen", "127.0.0.1:17811", "--state", &path("a.sealed"), "--sealing-key", &path("a.key")]);
    let _ob = spawn(env!("CARGO_BIN_EXE_zdlc-oracled"), &["--listen", "127.0.0.1:17812", "--state", &path("b.sealed"), "--sealing-key", &path("b.key")]);
    for u in ["http://127.0.0.1:17900/health", "http://127.0.0.1:17811/pubkey", "http://127.0.0.1:17812/pubkey"] {
        wait_http(u).await;
    }
    let desk = "http://127.0.0.1:17900";
    let oracles = "http://127.0.0.1:17811,http://127.0.0.1:17812";

    // A block a second.
    let miner = {
        let rt = Regtest::connect(&node).await.unwrap();
        tokio::spawn(async move {
            loop {
                let _ = rt.mine(1).await;
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        })
    };

    let mut maker = Command::new(env!("CARGO_BIN_EXE_zdlc-trader"))
        .args(["maker", "--seed", &path("maker.seed"), "--node", &node, "--desk", desk, "--oracles", oracles])
        .args(["--cap", "20000000", "--premium-bps", "50", "--ttl", "60", "--contracts", "3", "--log", &path("maker.json")])
        .spawn()
        .unwrap();

    // The partner only gave addresses: a transparent one for CETs, an Ironwood one for the close.
    let (partner, partner_fvk) = dev_beneficiary(&format!("phase4-{}", std::process::id()), "partner", 0x5A, true);
    let split = format!(
        "self:6000,{}+{}:4000",
        keys::encode_regtest(&partner.payout),
        hex::encode(partner.close.unwrap().to_raw_address_bytes())
    );
    let start = rt.tip().await.unwrap();
    for (settle, log, split) in [("close", "taker-close.json", ""), ("cet", "taker-cet.json", ""), ("close", "taker-split.json", split.as_str())] {
        let st = Command::new(env!("CARGO_BIN_EXE_zdlc-trader"))
            .args(["taker", "--seed", &path("taker.seed"), "--node", &node, "--desk", desk, "--oracles", oracles])
            .args(["--collateral", "10000000", "--maturity", "4", "--refund", "300", "--event-offset-min", "-3"])
            .args(["--quote-wait", "3", "--settle", settle, "--log", &path(log), "--split", split])
            .status()
            .unwrap();
        assert!(st.success(), "taker ({log}) failed");
    }
    let st = maker.wait().unwrap();
    miner.abort();
    assert!(st.success(), "maker failed");

    let read = |f: &str| -> Vec<Value> { serde_json::from_slice(&std::fs::read(path(f)).unwrap()).unwrap() };
    let steps = |l: &[Value]| l.iter().map(|s| s["step"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    let (tc, tt, ts, mk) = (read("taker-close.json"), read("taker-cet.json"), read("taker-split.json"), read("maker.json"));
    assert!(steps(&tc).contains(&"closed to Ironwood".to_string()), "{:?}", steps(&tc));
    assert!(steps(&tt).contains(&"settled by CET".to_string()), "{:?}", steps(&tt));
    assert!(steps(&ts).contains(&"closed to Ironwood".to_string()), "{:?}", steps(&ts));
    assert_eq!(steps(&mk).iter().filter(|s| *s == "contract finished").count(), 3);

    // The partner finds 40 % of the taker's side with its viewing key; the taker the rest.
    let tip = rt.tip().await.unwrap();
    let got = feeds::received(&rt.node, &partner_fvk.to_ivk(Scope::External), start, tip).await.unwrap();
    let owed = ts.iter().find(|s| s["step"] == "attested").unwrap()["result"]["payout"][0].as_u64().unwrap();
    println!("[split] taker side {owed} zat; partner received {got} zat in Ironwood");
    assert!(got > 0 && got.abs_diff(owed * 4 / 10) < 20_000, "partner got {got} of {owed}");
    let all = serde_json::json!({"taker_close": tc, "taker_cet": tt, "taker_split": ts, "maker": mk,
        "split": {"taker_side_zat": owed, "partner_received_zat": got, "partner_bps": 4000}});
    if let Ok(p) = std::env::var("ZDLC_RESULTS") {
        std::fs::write(p, serde_json::to_string_pretty(&all).unwrap()).unwrap();
    }
}
