//! The zdlc testnet wallet: creates a random seed at `~/.zdlc/testnet-seed.hex` (mode 600)
//! if there is none, and prints the transparent address to fund (coin key, index 0).
//! The seed never enters the repository.

use std::{fs, io::Write, os::unix::fs::OpenOptionsExt, path::PathBuf};

use zcash_protocol::consensus::NetworkType;
use zdlc::{keys, wallet::Wallet};

fn main() {
    let dir = PathBuf::from(std::env::var("HOME").expect("HOME")).join(".zdlc");
    let path = dir.join("testnet-seed.hex");
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
    let w = Wallet::from_seed(seed);
    println!("{}", keys::encode(&w.coin_address(0), NetworkType::Test));
}
