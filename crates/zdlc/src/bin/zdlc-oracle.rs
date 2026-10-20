//! Dev BIP-340 oracle for numeric events (one nonce per binary digit).
//!
//! ```sh
//! zdlc-oracle announce <event-id> [--digits 12] [--unit-cents 10]
//! zdlc-oracle attest   <event-id> <value>  [--digits 12] [--unit-cents 10]
//! ```
//!
//! Keys derive from `--label` (default "dev"); dev and regtest only. Attestations are
//! recorded in `--state` (default `.zdlc-oracle.json`) and an event is attested once:
//! a second signature under the same nonces would leak the key.

use std::{collections::BTreeMap, fs, process::exit};

use serde_json::{json, Value};
use zdlc::oracle::NumericOracle;

fn arg(args: &[String], name: &str, default: &str) -> String {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| default.to_string())
}

fn die(msg: &str) -> ! {
    eprintln!("zdlc-oracle: {msg}");
    exit(1)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, event) = match (args.first(), args.get(1)) {
        (Some(c), Some(e)) => (c.as_str(), e.clone()),
        _ => die("usage: zdlc-oracle announce|attest <event-id> [value] [--digits N] [--unit-cents C] [--label L] [--state FILE]"),
    };
    let digits: usize = arg(&args, "--digits", "12").parse().unwrap_or_else(|_| die("--digits"));
    let unit: u64 = arg(&args, "--unit-cents", "10").parse().unwrap_or_else(|_| die("--unit-cents"));
    let label = arg(&args, "--label", "dev");
    let state_path = arg(&args, "--state", ".zdlc-oracle.json");
    let mut state: BTreeMap<String, Value> = fs::read(&state_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let mut o = NumericOracle::new(&label, &event, digits, unit);
    let a = o.announcement().clone();
    match cmd {
        "announce" => {
            let out = json!({
                "event_id": a.event_id,
                "pubkey": hex::encode(a.pubkey.serialize()),
                "nonces": a.nonces.iter().map(|n| hex::encode(n.serialize())).collect::<Vec<_>>(),
                "nb_digits": a.nb_digits,
                "base": 2,
                "unit_cents": a.unit_cents,
            });
            println!("{}", serde_json::to_string_pretty(&out).unwrap());
        }
        "attest" => {
            let value: usize = args
                .get(2)
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(|| die("attest needs a value"));
            if let Some(prev) = state.get(&event) {
                die(&format!("event {event} already attested: {prev}"));
            }
            let att = o.attest(value).unwrap_or_else(|e| die(&e.to_string()));
            let out = json!({
                "event_id": att.event_id,
                "value": att.value,
                "price_cents": att.value as u64 * a.unit_cents,
                "digits": att.digits,
                "signatures": att.signatures.iter().map(|s| hex::encode(s.as_ref())).collect::<Vec<_>>(),
            });
            state.insert(event, json!({"value": att.value}));
            fs::write(&state_path, serde_json::to_vec_pretty(&state).unwrap())
                .unwrap_or_else(|e| die(&format!("state: {e}")));
            println!("{}", serde_json::to_string_pretty(&out).unwrap());
        }
        _ => die("command must be announce or attest"),
    }
}
