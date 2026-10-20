//! Watches one contract from its kit file (`zdlc::watch::Kit`) and broadcasts the refund
//! when it is due and nothing else has spent the funding output. Holds no keys.
//!
//! ```sh
//! zdlc-watch <kit.json> [--node http://127.0.0.1:18252/] [--interval 10] [--once]
//! ```

use std::time::Duration;

use zdlc::{node::Node, watch::{Kit, Status}};

fn arg(args: &[String], name: &str, default: &str) -> String {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| default.to_string())
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(path) = args.first().filter(|a| !a.starts_with("--")).cloned() else {
        eprintln!("usage: zdlc-watch <kit.json> [--node URL] [--interval SECS] [--once]");
        std::process::exit(2);
    };
    let node = Node::new(arg(&args, "--node", "http://127.0.0.1:18252/"));
    let interval: u64 = arg(&args, "--interval", "10").parse().unwrap_or(10);
    let once = args.iter().any(|a| a == "--once");
    loop {
        let result = async {
            let mut kit = Kit::load(&path)?;
            let st = kit.step(&node).await?;
            kit.save(&path)?;
            Ok::<_, zdlc::Error>((kit, st))
        }
        .await;
        match result {
            Ok((kit, st)) => {
                println!("{}: scanned to {}, {:?}", kit.label, kit.scanned_to, st);
                if matches!(st, Status::Settled { .. }) || once {
                    return;
                }
            }
            Err(e) => eprintln!("zdlc-watch: {e}"),
        }
        tokio::time::sleep(Duration::from_secs(interval)).await;
    }
}
