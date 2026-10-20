//! Dev BIP-340 oracle for numeric events (one nonce per binary digit).
//!
//! ```sh
//! zdlc-oracle announce <event-id> [--digits 12] [--unit-cents 10]
//! zdlc-oracle attest   <event-id> <value>
//! ```
//!
//! The key derives from `--label` (default "dev"): dev and regtest only. Each announce
//! draws fresh random nonces and stores them in `--state` (default `.zdlc-oracle.json`,
//! secret). Attest only signs an announced event, once: the event is marked attested and
//! the state written before the signatures are printed, so a crash cannot lead to a second
//! signature under the same nonces.

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

fn save(path: &str, state: &BTreeMap<String, Value>) {
    let tmp = format!("{path}.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(state).unwrap())
        .and_then(|_| fs::rename(&tmp, path))
        .unwrap_or_else(|e| die(&format!("state: {e}")));
}

fn restore(label: &str, event: &str, e: &Value) -> NumericOracle {
    let nonces = e["nonces"]
        .as_array()
        .unwrap_or_else(|| die("corrupt state"))
        .iter()
        .map(|n| {
            hex::decode(n.as_str().unwrap_or_default())
                .ok()
                .and_then(|b| b.try_into().ok())
                .unwrap_or_else(|| die("corrupt nonce"))
        })
        .collect();
    NumericOracle::with_nonces(label, event, e["unit_cents"].as_u64().unwrap_or(10), nonces)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, event) = match (args.first(), args.get(1)) {
        (Some(c), Some(e)) => (c.as_str(), e.clone()),
        _ => die("usage: zdlc-oracle announce|attest <event-id> [value] [--digits N] [--unit-cents C] [--label L] [--state FILE]"),
    };
    let label = arg(&args, "--label", "dev");
    let state_path = arg(&args, "--state", ".zdlc-oracle.json");
    let mut state: BTreeMap<String, Value> = match fs::read(&state_path) {
        Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|_| die("corrupt state file")),
        Err(_) => BTreeMap::new(),
    };
    match cmd {
        "announce" => {
            let o = match state.get(&event) {
                Some(e) => restore(&label, &event, e),
                None => {
                    let digits: usize = arg(&args, "--digits", "12").parse().unwrap_or_else(|_| die("--digits"));
                    let unit: u64 = arg(&args, "--unit-cents", "10").parse().unwrap_or_else(|_| die("--unit-cents"));
                    let o = NumericOracle::new(&label, &event, digits, unit);
                    state.insert(
                        event.clone(),
                        json!({
                            "nonces": o.nonce_secrets().iter().map(hex::encode).collect::<Vec<_>>(),
                            "unit_cents": unit,
                            "attested": null,
                        }),
                    );
                    save(&state_path, &state);
                    o
                }
            };
            let a = o.announcement();
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
            let entry = state
                .get(&event)
                .cloned()
                .unwrap_or_else(|| die(&format!("event {event} was never announced")));
            if !entry["attested"].is_null() {
                die(&format!("event {event} already attested: {}", entry["attested"]));
            }
            let mut o = restore(&label, &event, &entry);
            let unit = o.announcement().unit_cents;
            let clamped = value.min((1 << o.announcement().nb_digits) - 1);
            state.get_mut(&event).unwrap()["attested"] = json!(clamped);
            save(&state_path, &state);
            let att = o.attest(value).unwrap_or_else(|e| die(&e.to_string()));
            let out = json!({
                "event_id": att.event_id,
                "value": att.value,
                "price_cents": att.value as u64 * unit,
                "digits": att.digits,
                "signatures": att.signatures.iter().map(|s| hex::encode(s.as_ref())).collect::<Vec<_>>(),
            });
            println!("{}", serde_json::to_string_pretty(&out).unwrap());
        }
        _ => die("command must be announce or attest"),
    }
}
