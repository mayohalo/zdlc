//! A zdlc wallet for a live network: creates a random seed at `~/.zdlc/<network>-seed.hex`
//! (mode 600) if there is none, and prints the transparent address to fund (the treasury,
//! index 0). The seed never enters the repository.
//!
//! ```sh
//! cargo run -p zdlc --release --example wallet -- testnet
//! cargo run -p zdlc --release --example wallet -- mainnet
//! ```

use std::{fs, io::Write, os::unix::fs::OpenOptionsExt, path::PathBuf};

use zcash_protocol::consensus::NetworkType;
use zdlc::{keys, wallet::Wallet};

fn main() {
    let net = std::env::args().nth(1).unwrap_or("testnet".into());
    let ty = match net.as_str() {
        "testnet" => NetworkType::Test,
        "mainnet" => NetworkType::Main,
        _ => {
            eprintln!("usage: wallet testnet|mainnet");
            std::process::exit(2);
        }
    };
    let dir = PathBuf::from(std::env::var("HOME").expect("HOME")).join(".zdlc");
    let path = dir.join(format!("{net}-seed.hex"));
    let seed: [u8; 32] = match fs::read_to_string(&path) {
        Ok(h) => hex::decode(h.trim()).expect("seed hex").try_into().expect("32 bytes"),
        Err(_) => {
            fs::create_dir_all(&dir).expect("mkdir ~/.zdlc");
            let mut s = [0u8; 32];
            rand10::fill(&mut s);
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .expect("create seed file");
            writeln!(f, "{}", hex::encode(s)).expect("write seed");
            eprintln!("created {}", path.display());
            s
        }
    };
    println!("{}", keys::encode(&Wallet::from_seed(seed).coin_address(0), ty));
}
