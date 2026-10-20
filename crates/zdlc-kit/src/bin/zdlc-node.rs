//! zdlc-node: one zDLC node with a REST API (see `openapi.json`) for apps in any language.
//!
//! ```sh
//! zdlc-node --seed node.seed --node $ZDLC_RPC --desk http://127.0.0.1:7900 --oracles http://127.0.0.1:7801,http://127.0.0.1:7802 \
//!           [--db zdlc.db] [--listen 127.0.0.1:7950] [--token-file node.token] [--no-auto-shield]
//! ```
//!
//! `--seed` and `--token-file` are created if missing (mode 600). The API needs the
//! token as `Authorization: Bearer <token>`. It can spend the treasury and commit
//! collateral: keep it on loopback or behind TLS and authentication you trust.

use std::{io::Write, os::unix::fs::OpenOptionsExt, sync::Arc};

use zdlc_kit::{impls::*, Zdlc};

fn arg(args: &[String], name: &str, default: &str) -> String {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned().unwrap_or_else(|| default.to_string())
}

fn die(m: impl std::fmt::Display) -> ! {
    eprintln!("zdlc-node: {m}");
    std::process::exit(1)
}

/// Reads 32 bytes of hex from `path`, creating it (mode 600) if missing.
fn secret(path: &str) -> [u8; 32] {
    if let Ok(h) = std::fs::read_to_string(path) {
        return hex::decode(h.trim()).ok().and_then(|b| b.try_into().ok()).unwrap_or_else(|| die(format!("{path}: 32 bytes of hex")));
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
    let seed_path = arg(&args, "--seed", "");
    if seed_path.is_empty() {
        die("--seed FILE is required");
    }
    let seed = secret(&seed_path);
    let token = hex::encode(secret(&arg(&args, "--token-file", &format!("{seed_path}.token"))));
    let rpc = match arg(&args, "--node", "").as_str() {
        "" => std::env::var("ZDLC_RPC").unwrap_or_else(|_| die("--node URL or ZDLC_RPC")),
        u => u.to_string(),
    };
    let wallet = zdlc::wallet::Wallet::from_seed(seed);
    let db = arg(&args, "--db", &format!("{seed_path}.db"));
    let storage = Arc::new(SqliteStorage::open(&db).unwrap_or_else(|e| die(e)));
    let mut b = Zdlc::builder()
        .seed(seed)
        .chain(Arc::new(ZakuraRpc::new(&rpc)))
        .storage(storage)
        .transport(Arc::new(DeskTransport::new(&arg(&args, "--desk", ""), wallet.identity_key())))
        .auto_shield(!args.iter().any(|a| a == "--no-auto-shield"));
    for o in arg(&args, "--oracles", "").split(',').filter(|s| !s.is_empty()) {
        b = b.oracle(Arc::new(HttpOracle::new(o)));
    }
    let z = Arc::new(b.build().await.unwrap_or_else(|e| die(e)));
    let listen = arg(&args, "--listen", "127.0.0.1:7950");
    println!("zdlc-node {listen}: identity {} treasury {} db {db}", z.identity(), z.treasury_address());
    tokio::spawn(z.clone().run(std::time::Duration::from_millis(1000)));
    let l = tokio::net::TcpListener::bind(&listen).await.unwrap_or_else(|e| die(e));
    axum::serve(l, zdlc_kit::api::router(z, token)).await.unwrap_or_else(|e| die(e));
}
