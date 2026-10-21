//! zdlc-cli: drives a zdlc-node.
//!
//! ```sh
//! zdlc-cli [--url http://127.0.0.1:7950] [--token-file node.seed.token] <command>
//!   info | balance | contracts | show ID | events [--after N] [--follow]
//!   offer escrow --to ID --amount ZEC --question Q [--payee ADDR[+UA]:BPS,...] [--maturity 20] [--refund 2000]
//!   offer stable-payment --to ID --usd 100.00 --collateral ZEC [--premium ZEC] [--maturity 6] [--refund 60] [--minute UNIX]
//!   offer bet --to ID --question Q --outcomes a,b --offerer-wins a --stake ZEC [--maturity 20] [--refund 2000]
//!   offer json --to ID --template FILE
//!   offer hedge --to ID --amount ZEC --price USD [--premium ZEC] [--maturity 6] [--refund 60] [--minute UNIX]
//!   offer payroll --to ID --usd 1000.00 --collateral ZEC --periods N --period-blocks B [--refund-margin 40]
//!   offer pay-on-payment --to ID --amount ZEC --collateral ZEC --within BLOCKS [--refund 2000]
//!   accept ID [--funding treasury|deposit] | decline ID | qr ID | withdraw --to UA | recover [--from H] [--gap 20]
//! ```

use serde_json::{json, Value};

fn arg(args: &[String], name: &str, default: &str) -> String {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned().unwrap_or_else(|| default.to_string())
}

fn die(m: impl std::fmt::Display) -> ! {
    eprintln!("zdlc-cli: {m}");
    std::process::exit(1)
}

/// Decimal ZEC to zatoshis.
fn zat(s: &str) -> u64 {
    let (w, f) = s.split_once('.').unwrap_or((s, ""));
    if f.len() > 8 {
        die(format!("{s}: at most 8 decimals"));
    }
    w.parse::<u64>().ok().zip(format!("{f:0<8}").parse::<u64>().ok()).map(|(w, f)| w * 100_000_000 + f).unwrap_or_else(|| die(format!("{s}: not an amount")))
}

fn num(args: &[String], name: &str, default: &str) -> u64 {
    arg(args, name, default).parse().unwrap_or_else(|_| die(format!("{name}: a number")))
}

struct Api {
    url: String,
    token: String,
    http: reqwest::Client,
}

impl Api {
    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Value {
        let mut r = self.http.request(method, format!("{}{path}", self.url)).bearer_auth(&self.token);
        if let Some(b) = body {
            r = r.json(&b);
        }
        let resp = r.send().await.unwrap_or_else(|e| die(e));
        let ok = resp.status().is_success();
        let v: Value = resp.json().await.unwrap_or_else(|e| die(e));
        if !ok {
            die(v["error"].as_str().unwrap_or("request failed"));
        }
        v
    }
    async fn get(&self, path: &str) -> Value {
        self.call(reqwest::Method::GET, path, None).await
    }
    async fn post(&self, path: &str, body: Value) -> Value {
        self.call(reqwest::Method::POST, path, Some(body)).await
    }
}

fn print(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap());
}

fn template(kind: &str, args: &[String]) -> Value {
    match kind {
        "escrow" => {
            let payees: Vec<Value> = arg(args, "--payee", "")
                .split(',')
                .filter(|s| !s.is_empty())
                .enumerate()
                .map(|(i, p)| {
                    let (addr, bps) = p.rsplit_once(':').unwrap_or_else(|| die("--payee ADDR[+UA]:BPS"));
                    let (t, ua) = addr.split_once('+').map(|(t, u)| (t, Some(u))).unwrap_or((addr, None));
                    json!({"label": format!("payee {}", i + 1), "address": t, "shielded": ua, "bps": bps.parse::<u32>().unwrap_or_else(|_| die("bps"))})
                })
                .collect();
            json!({"template": "escrow", "amount_zat": zat(&arg(args, "--amount", "")), "question": arg(args, "--question", ""),
                   "payees": payees, "maturity_blocks": num(args, "--maturity", "20"), "refund_blocks": num(args, "--refund", "2000")})
        }
        "stable-payment" => {
            let usd = arg(args, "--usd", "");
            let cents = zat(&usd) / 1_000_000; // two decimals
            let minute = arg(args, "--minute", "");
            json!({"template": "stable_payment", "usd_cents": cents, "collateral_zat": zat(&arg(args, "--collateral", "")),
                   "premium_zat": zat(&arg(args, "--premium", "0")), "maturity_blocks": num(args, "--maturity", "6"),
                   "refund_blocks": num(args, "--refund", "60"), "price_minute": if minute.is_empty() { Value::Null } else { json!(minute.parse::<i64>().unwrap_or_else(|_| die("--minute"))) }})
        }
        "bet" => {
            let list = |n: &str| arg(args, n, "").split(',').filter(|s| !s.is_empty()).map(String::from).collect::<Vec<_>>();
            json!({"template": "bet", "question": arg(args, "--question", ""), "outcomes": list("--outcomes"), "offerer_wins": list("--offerer-wins"),
                   "stake_zat": zat(&arg(args, "--stake", "")), "maturity_blocks": num(args, "--maturity", "20"), "refund_blocks": num(args, "--refund", "2000")})
        }
        "hedge" => {
            let minute = arg(args, "--minute", "");
            json!({"template": "hedge", "amount_zat": zat(&arg(args, "--amount", "")), "price_cents": zat(&arg(args, "--price", "")) / 1_000_000,
                   "premium_zat": zat(&arg(args, "--premium", "0")), "maturity_blocks": num(args, "--maturity", "6"), "refund_blocks": num(args, "--refund", "60"),
                   "price_minute": if minute.is_empty() { Value::Null } else { json!(minute.parse::<i64>().unwrap_or_else(|_| die("--minute"))) }})
        }
        "payroll" => json!({"template": "payroll", "usd_cents": zat(&arg(args, "--usd", "")) / 1_000_000, "collateral_zat": zat(&arg(args, "--collateral", "")),
                            "premium_zat": zat(&arg(args, "--premium", "0")), "periods": num(args, "--periods", "1"), "period_blocks": num(args, "--period-blocks", "20"),
                            "refund_margin_blocks": num(args, "--refund-margin", "40")}),
        "pay-on-payment" => json!({"template": "pay_on_payment", "amount_zat": zat(&arg(args, "--amount", "")), "collateral_zat": zat(&arg(args, "--collateral", "")),
                                   "by_blocks": num(args, "--within", "20"), "refund_blocks": num(args, "--refund", "2000")}),
        "json" => serde_json::from_slice(&std::fs::read(arg(args, "--template", "")).unwrap_or_else(|e| die(e))).unwrap_or_else(|e| die(e)),
        _ => die("offer escrow | stable-payment | hedge | payroll | pay-on-payment | bet | json"),
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let token_file = arg(&args, "--token-file", "");
    let token = if token_file.is_empty() { std::env::var("ZDLC_TOKEN").unwrap_or_else(|_| die("--token-file FILE or ZDLC_TOKEN")) } else {
        std::fs::read_to_string(&token_file).unwrap_or_else(|e| die(e)).trim().to_string()
    };
    let api = Api { url: arg(&args, "--url", "http://127.0.0.1:7950"), token, http: reqwest::Client::new() };
    let pos: Vec<&String> = args.iter().enumerate().filter(|(i, a)| !a.starts_with("--") && (*i == 0 || !args[i - 1].starts_with("--"))).map(|(_, a)| a).collect();
    let cmd = pos.first().map(|s| s.as_str()).unwrap_or("");
    let second = pos.get(1).map(|s| s.as_str()).unwrap_or("");
    match cmd {
        "info" => print(&api.get("/v1/info").await),
        "balance" => print(&api.get("/v1/balance").await),
        "contracts" => print(&api.get("/v1/contracts").await),
        "show" => print(&api.get(&format!("/v1/contracts/{second}")).await),
        "offer" => print(&api.post("/v1/contracts", json!({"counterparty": arg(&args, "--to", ""), "template": template(second, &args)})).await),
        "accept" => print(&api.post(&format!("/v1/contracts/{second}/accept"), json!({"funding": arg(&args, "--funding", "treasury")})).await),
        "decline" => print(&api.post(&format!("/v1/contracts/{second}/decline"), json!({})).await),
        "qr" => {
            let c = api.get(&format!("/v1/contracts/{second}")).await;
            let uri = c["deposit"]["uri"].as_str().unwrap_or_else(|| die("no deposit link"));
            println!("{uri}\n{}", zdlc::paylink::qr_text(uri).unwrap_or_else(|e| die(e)));
        }
        "withdraw" => print(&api.post("/v1/withdraw", json!({"to": arg(&args, "--to", "")})).await),
        "recover" => print(&api.post("/v1/recover", json!({"from": num(&args, "--from", "1"), "gap": num(&args, "--gap", "20")})).await),
        "events" => {
            let mut after: u64 = num(&args, "--after", "0");
            let follow = args.iter().any(|a| a == "--follow");
            loop {
                let v = api.get(&format!("/v1/events?after={after}&wait={}", if follow { 30 } else { 0 })).await;
                for e in v["events"].as_array().cloned().unwrap_or_default() {
                    println!("{e}");
                }
                after = v["next"].as_u64().unwrap_or(after);
                if !follow {
                    break;
                }
            }
        }
        _ => die("command: info | balance | contracts | show | offer | accept | decline | qr | withdraw | recover | events"),
    }
}
