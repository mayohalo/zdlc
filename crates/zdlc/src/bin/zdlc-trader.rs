//! A desk agent with its own wallet: a maker (hedger) that quotes RFQs, or a taker
//! (recipient) that requests a stable-value hedge and settles it.
//!
//! ```sh
//! zdlc-trader address --seed s.hex --node URL
//! zdlc-trader maker   --seed s.hex --node URL --desk URL --oracles A,B [--premium-bps 50] [--cap ZAT] [--ttl 40] [--contracts N]
//! zdlc-trader taker   --seed s.hex --node URL --desk URL --oracles A,B --collateral ZAT [--maturity 6] [--refund 60]
//!                     [--event-offset-min 2] [--quote-wait 8] [--settle close|cet]
//! zdlc-trader maker   ... --invite INBOX_HEX --invite-collateral ZAT [--invite-maturity 6] [--invite-refund 60]
//! zdlc-trader taker   ... --invitations-from HEIGHT          (trade on a memo invitation, no RFQ)
//! ```
//!
//! `--seed` is a file holding 32 bytes of hex (created if missing, mode 600). `--node`
//! may come from `ZDLC_RPC`. `--log FILE` writes the steps as JSON.

use std::{io::Write, os::unix::fs::OpenOptionsExt};

use zdlc::{
    node::Node,
    trader::{self, Agent, MakerArgs, TakerArgs},
};

fn arg(args: &[String], name: &str, default: &str) -> String {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned().unwrap_or_else(|| default.to_string())
}

fn die(m: impl std::fmt::Display) -> ! {
    eprintln!("zdlc-trader: {m}");
    std::process::exit(1)
}

fn seed(path: &str) -> [u8; 32] {
    if let Ok(h) = std::fs::read_to_string(path) {
        return hex::decode(h.trim()).ok().and_then(|b| b.try_into().ok()).unwrap_or_else(|| die("seed must be 32 bytes of hex"));
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
    let url = match arg(&args, "--node", "").as_str() {
        "" => std::env::var("ZDLC_RPC").unwrap_or_else(|_| die("--node URL or ZDLC_RPC")),
        u => u.to_string(),
    };
    let oracles: Vec<String> = arg(&args, "--oracles", "").split(',').filter(|s| !s.is_empty()).map(String::from).collect();
    let state = arg(&args, "--state", &format!("{seed_path}.state.json"));
    let mut agent = Agent::new(&cmd, Node::new(url), &arg(&args, "--desk", ""), seed(&seed_path), &oracles, &state)
        .await
        .unwrap_or_else(|e| die(e));
    let out = match cmd.as_str() {
        "address" => {
            println!("treasury {}", agent.treasury_address());
            println!("identity {}", agent.identity());
            println!("inbox (Ironwood, raw) {}", hex::encode(agent.wallet.close_address(0).to_raw_address_bytes()));
            return;
        }
        "maker" => {
            let invite = match arg(&args, "--invite", "").as_str() {
                "" => None,
                h => Some(trader::Invite {
                    inbox: hex::decode(h).ok().and_then(|b| <[u8; 43]>::try_from(b).ok())
                        .and_then(|b| Option::from(orchard::Address::from_raw_address_bytes(&b)))
                        .unwrap_or_else(|| die("--invite takes the taker's raw Ironwood inbox (hex)")),
                    q_recipient: arg(&args, "--invite-collateral", "").parse().unwrap_or_else(|_| die("--invite-collateral ZAT")),
                    maturity_blocks: arg(&args, "--invite-maturity", "6").parse().unwrap_or_else(|_| die("--invite-maturity")),
                    refund_blocks: arg(&args, "--invite-refund", "60").parse().unwrap_or_else(|_| die("--invite-refund")),
                }),
            };
            let a = MakerArgs {
                invite,
                premium_bps: arg(&args, "--premium-bps", "50").parse().unwrap_or_else(|_| die("--premium-bps")),
                cap: arg(&args, "--cap", "5000000").parse().unwrap_or_else(|_| die("--cap")),
                quote_ttl_blocks: arg(&args, "--ttl", "40").parse().unwrap_or_else(|_| die("--ttl")),
                contracts: arg(&args, "--contracts", "0").parse().unwrap_or_else(|_| die("--contracts")),
            };
            trader::run_maker(&mut agent, &a).await
        }
        "taker" => {
            let a = TakerArgs {
                invitations_from: match arg(&args, "--invitations-from", "").as_str() {
                    "" => None,
                    h => Some(h.parse().unwrap_or_else(|_| die("--invitations-from HEIGHT"))),
                },
                q_recipient: arg(&args, "--collateral", "0").parse().unwrap_or_else(|_| die("--collateral ZAT")),
                maturity_blocks: arg(&args, "--maturity", "6").parse().unwrap_or_else(|_| die("--maturity")),
                refund_blocks: arg(&args, "--refund", "60").parse().unwrap_or_else(|_| die("--refund")),
                event_offset_min: arg(&args, "--event-offset-min", "2").parse().unwrap_or_else(|_| die("--event-offset-min")),
                quote_wait_s: arg(&args, "--quote-wait", "8").parse().unwrap_or_else(|_| die("--quote-wait")),
                settle: arg(&args, "--settle", "close"),
            };
            trader::run_taker(&mut agent, &a).await
        }
        _ => die("command must be address, maker or taker"),
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
