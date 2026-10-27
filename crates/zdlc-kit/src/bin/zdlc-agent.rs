//! zdlc-agent: desk agents on the kit. A maker quotes ZEC/USD hedge requests and accepts
//! matching offers; a taker requests a hedge, offers on the cheapest quote and follows
//! it to settlement. Each agent is a full kit node with its own seed and database.
//!
//! ```sh
//! zdlc-agent maker --seed m.seed --node URL --desk URL --oracles A,B [--premium-bps 50] [--cap ZEC] [--ttl 60] [--contracts N]
//! zdlc-agent taker --seed t.seed --node URL --desk URL --oracles A,B --collateral ZEC [--maturity 8] [--refund 300]
//!                  [--minute-offset 2] [--quote-wait 5]
//! zdlc-agent address --seed s.seed --node URL                       # the treasury to fund
//! ```
//!
//! `--log FILE` writes the steps as JSON. The legacy `zdlc-trader` keeps the features not
//! yet on the kit: memo invitations, deposit-funded takers and taker splits.

use std::sync::Arc;

use zdlc_kit::{agent, impls::*, Zdlc};

fn arg(args: &[String], name: &str, default: &str) -> String {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned().unwrap_or_else(|| default.to_string())
}

fn die(m: impl std::fmt::Display) -> ! {
    eprintln!("zdlc-agent: {m}");
    std::process::exit(1)
}

fn zat(s: &str) -> u64 {
    let (w, f) = s.split_once('.').unwrap_or((s, ""));
    w.parse::<u64>().ok().zip(format!("{f:0<8}").get(..8).and_then(|f| f.parse::<u64>().ok())).map(|(w, f)| w * 100_000_000 + f).unwrap_or_else(|| die(format!("{s}: not an amount")))
}

fn seed(path: &str) -> [u8; 32] {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    if let Ok(h) = std::fs::read_to_string(path) {
        return hex::decode(h.trim()).ok().and_then(|b| b.try_into().ok()).unwrap_or_else(|| die("seed: 32 bytes of hex"));
    }
    let mut s = [0u8; 32];
    rand10::fill(&mut s);
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path).unwrap_or_else(|e| die(e));
    writeln!(f, "{}", hex::encode(s)).unwrap_or_else(|e| die(e));
    s
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().cloned().unwrap_or_default();
    let seed_path = arg(&args, "--seed", "");
    if seed_path.is_empty() {
        die("--seed FILE is required");
    }
    let s = seed(&seed_path);
    let wallet = zdlc::wallet::Wallet::from_seed(s);
    let rpc = match arg(&args, "--node", "").as_str() {
        "" => std::env::var("ZDLC_RPC").unwrap_or_else(|_| die("--node URL or ZDLC_RPC")),
        u => u.to_string(),
    };
    let desk_url = arg(&args, "--desk", "http://127.0.0.1:7900");
    let oracle_urls: Vec<String> = arg(&args, "--oracles", "").split(',').filter(|s| !s.is_empty()).map(String::from).collect();
    let mut b = Zdlc::builder()
        .seed(s)
        .chain(Arc::new(ZakuraRpc::new(&rpc)))
        .storage(Arc::new(SqliteStorage::open(&arg(&args, "--db", &format!("{seed_path}.db"))).unwrap_or_else(|e| die(e))))
        .transport(Arc::new(DeskTransport::new(&desk_url, wallet.identity_key())));
    for o in &oracle_urls {
        b = b.oracle(Arc::new(HttpOracle::new(o)));
    }
    if cmd == "address" {
        b = b.oracle(Arc::new(HttpOracle::new("http://127.0.0.1:1")));
    }
    let z = Arc::new(b.build().await.unwrap_or_else(|e| die(e)));
    if cmd == "address" {
        println!("treasury {}\nidentity {}", z.treasury_address(), z.identity());
        return;
    }
    let mut keys = vec![];
    for o in &oracle_urls {
        keys.push(zdlc::oracle_client::OracleClient::new(o).info().await.unwrap_or_else(|e| die(e))["pubkey"].as_str().unwrap_or_default().to_string());
    }
    tokio::spawn(z.clone().run(std::time::Duration::from_millis(1000)));
    let desk = zdlc::desk::DeskClient::new(&desk_url);
    let out = match cmd.as_str() {
        "maker" => {
            let a = agent::MakerArgs {
                premium_bps: arg(&args, "--premium-bps", "50").parse().unwrap_or_else(|_| die("--premium-bps")),
                cap_zat: zat(&arg(&args, "--cap", "0.2")),
                ttl_blocks: arg(&args, "--ttl", "60").parse().unwrap_or_else(|_| die("--ttl")),
                contracts: arg(&args, "--contracts", "0").parse().unwrap_or_else(|_| die("--contracts")),
            };
            agent::run_maker(z, &desk, &wallet.identity_key(), keys, &a).await
        }
        "taker" => {
            let a = agent::TakerArgs {
                collateral_zat: zat(&arg(&args, "--collateral", "")),
                maturity_blocks: arg(&args, "--maturity", "8").parse().unwrap_or_else(|_| die("--maturity")),
                refund_blocks: arg(&args, "--refund", "300").parse().unwrap_or_else(|_| die("--refund")),
                minute_offset: arg(&args, "--minute-offset", "2").parse().unwrap_or_else(|_| die("--minute-offset")),
                quote_wait_s: arg(&args, "--quote-wait", "5").parse().unwrap_or_else(|_| die("--quote-wait")),
                rounding_bps: None,
            };
            agent::run_taker(z, &desk, &wallet.identity_key(), keys, &a).await
        }
        _ => die("command: maker | taker | address"),
    };
    match out {
        Ok(log) => {
            let p = arg(&args, "--log", "");
            if !p.is_empty() {
                std::fs::write(&p, serde_json::to_vec_pretty(&log).unwrap()).unwrap_or_else(|e| die(e));
            }
        }
        Err(e) => die(e),
    }
}
