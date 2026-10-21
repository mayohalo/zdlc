//! zdlc-dev: regtest helpers for the local dev stack (`scripts/dev.sh`, `just dev`).
//!
//! ```sh
//! zdlc-dev fund  --node URL ADDR ZEC [ADDR ZEC ...]    # pay transparent addresses from coinbase
//! zdlc-dev mine  --node URL [--every SECS]             # a block every SECS (default 2), forever
//! zdlc-dev pay   --node URL URI                        # a dev wallet pays a ZIP 321 link, shielded
//! ```
//!
//! `pay` plays the user's phone wallet: it holds shielded funds (topped up from coinbase
//! and shielded when short) and pays deposit links (unified addresses) shielded, and leg
//! links (t2…) from its notes. Regtest only.

use zdlc::{funds, keys, notes, paylink, regtest::Regtest, tx, wallet::Wallet};

fn arg(args: &[String], name: &str, default: &str) -> String {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned().unwrap_or_else(|| default.to_string())
}

fn die(m: impl std::fmt::Display) -> ! {
    eprintln!("zdlc-dev: {m}");
    std::process::exit(1)
}

fn zat(s: &str) -> u64 {
    let (w, f) = s.split_once('.').unwrap_or((s, ""));
    w.parse::<u64>().ok().zip(format!("{f:0<8}").get(..8).and_then(|f| f.parse::<u64>().ok())).map(|(w, f)| w * 100_000_000 + f).unwrap_or_else(|| die(format!("{s}: not an amount")))
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let url = arg(&args, "--node", "http://127.0.0.1:18252/");
    let rt = Regtest::connect(&url).await.unwrap_or_else(|e| die(e));
    // Contracts run past the last upgrade.
    rt.node.mine_to(zdlc::params::NU7_AT + 10).await.unwrap_or_else(|e| die(e));
    let pos: Vec<&String> = args.iter().enumerate().filter(|(i, a)| !a.starts_with("--") && (*i == 0 || !args[i - 1].starts_with("--"))).map(|(_, a)| a).collect();
    match pos.first().map(|s| s.as_str()) {
        Some("fund") => {
            let pairs: Vec<_> = pos[1..]
                .chunks(2)
                .map(|c| {
                    let a = keys::decode(c[0]).unwrap_or_else(|| die(format!("{}: not a transparent address", c[0]))).1;
                    (a, zat(c.get(1).unwrap_or_else(|| die("ADDR ZEC pairs"))))
                })
                .collect();
            let txid = rt.pay(&pairs).await.unwrap_or_else(|e| die(e));
            println!("funded {} address(es): {}", pairs.len(), zdlc::txid_hex(&txid));
        }
        Some("mine") => {
            let every: u64 = arg(&args, "--every", "2").parse().unwrap_or_else(|_| die("--every SECS"));
            loop {
                if let Err(e) = rt.mine(1).await {
                    eprintln!("zdlc-dev: mine: {e}");
                }
                tokio::time::sleep(std::time::Duration::from_secs(every)).await;
            }
        }
        Some("pay") => {
            let uri = pos.get(1).unwrap_or_else(|| die("pay URI"));
            let (addr, amount, memo) = paylink::parse(uri).unwrap_or_else(|| die("not a ZIP 321 link"));
            let w = Wallet::from_seed([0xDE; 32]);
            let (sk, fvk) = (w.deposit_sk(0), w.deposit_fvk(0));
            let tip = rt.tip().await.unwrap_or_else(|e| die(e));
            let mut held = notes::unspent(&rt.node, &fvk, 200, tip).await.unwrap_or_else(|e| die(e));
            if held.iter().map(|n| n.value()).sum::<u64>() < amount + 100_000 {
                // Top up: coinbase to a transparent coin, shielded into the wallet.
                let top = (amount + 1_0000_0000).min(5_0000_0000);
                let txid = rt.pay(&[(w.coin_address(0), top)]).await.unwrap_or_else(|e| die(e));
                let coin = tx::Coin { txid, index: 0, value: top, spend: tx::Spend::P2pkh(keys::pubkey(&w.coin_key(0))) };
                let tip = rt.tip().await.unwrap_or_else(|e| die(e));
                let anchor = rt.node.ironwood_anchor(tip).await.unwrap_or_else(|e| die(e));
                let sh = funds::shield(&rt.p, tip, &[(coin, w.coin_key(0))], w.deposit_address(0), anchor).unwrap_or_else(|e| die(e));
                let h = rt.send_confirm(&sh).await.unwrap_or_else(|e| die(e));
                held = notes::unspent(&rt.node, &fvk, h, h).await.unwrap_or_else(|e| die(e));
            }
            let to = match paylink::decode_unified(&addr) {
                Some((_, z)) => funds::To::Ironwood(z, memo),
                None => funds::To::Transparent(keys::decode(&addr).unwrap_or_else(|| die(format!("{addr}: unknown address"))).1),
            };
            let f = funds::pay_from_notes(&rt.node, &rt.p, &sk, &held, to, Some(amount), w.deposit_address(0)).await.unwrap_or_else(|e| die(e));
            rt.node.send(&f.raw).await.unwrap_or_else(|e| die(e));
            println!("paid {} ZEC to {addr}: {}", paylink::zec(amount), zdlc::txid_hex(&f.txid));
        }
        _ => die("fund | mine | pay"),
    }
}
