//! What the oracles attest.
//!
//! - ZEC/USD: the close of one fixed UTC minute on four exchanges (Coinbase, Kraken,
//!   Bitstamp, Binance ZEC/USDT), median of those that answer, at least two. A closed
//!   minute does not change, so two oracles fetching it independently, at different
//!   times, normally reach the same value; if they do not, no CET opens and the contract
//!   refunds.
//! - Shielded payment: whether Ironwood notes decryptable by a viewing key sum to at
//!   least an amount between two heights.

use serde_json::{json, Value};

use crate::{node::Node, Error, Result};

#[derive(Clone, Debug)]
pub struct Quote {
    pub source: &'static str,
    pub close_cents: u64,
}

fn cents(s: &str) -> Option<u64> {
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    let mut f: String = frac.chars().chain("000".chars()).take(3).collect();
    let round_up = f.pop().is_some_and(|c| c >= '5');
    let c = int.parse::<u64>().ok()? * 100 + f.parse::<u64>().ok()?;
    Some(c + round_up as u64)
}

fn num(v: &Value) -> Option<u64> {
    match v {
        Value::String(s) => cents(s),
        Value::Number(n) => cents(&n.to_string()),
        _ => None,
    }
}

async fn get(http: &reqwest::Client, url: &str) -> Option<Value> {
    http.get(url)
        .header("user-agent", "zdlc-oracle")
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()
}

/// Each source's close for the minute starting at `minute` (unix seconds, a multiple of 60).
pub async fn zecusd_quotes(minute: i64) -> Vec<Quote> {
    let http = reqwest::Client::new();
    let iso = |t: i64| {
        let d = t.div_euclid(86_400);
        let (y, m, dd) = civil(d);
        let s = t.rem_euclid(86_400);
        format!("{y:04}-{m:02}-{dd:02}T{:02}:{:02}:{:02}Z", s / 3600, s % 3600 / 60, s % 60)
    };
    let mut out = vec![];
    // Coinbase: [time, low, high, open, close, volume]
    if let Some(v) = get(&http, &format!(
        "https://api.exchange.coinbase.com/products/ZEC-USD/candles?granularity=60&start={}&end={}",
        iso(minute), iso(minute + 60)
    )).await {
        if let Some(c) = v.as_array().and_then(|a| a.iter().find(|k| k[0].as_i64() == Some(minute))).and_then(|k| num(&k[4])) {
            out.push(Quote { source: "coinbase", close_cents: c });
        }
    }
    // Kraken: [time, open, high, low, close, vwap, volume, count]
    if let Some(v) = get(&http, &format!("https://api.kraken.com/0/public/OHLC?pair=ZECUSD&interval=1&since={}", minute - 60)).await {
        let rows = v["result"].as_object().and_then(|o| o.iter().find(|(k, _)| *k != "last")).map(|(_, r)| r.clone());
        if let Some(c) = rows.as_ref().and_then(Value::as_array).and_then(|a| a.iter().find(|k| k[0].as_i64() == Some(minute))).and_then(|k| num(&k[4])) {
            out.push(Quote { source: "kraken", close_cents: c });
        }
    }
    // Bitstamp: {"data": {"ohlc": [{"timestamp", "close", ...}]}}
    if let Some(v) = get(&http, &format!("https://www.bitstamp.net/api/v2/ohlc/zecusd/?step=60&limit=1&start={minute}")).await {
        if let Some(c) = v["data"]["ohlc"].as_array().and_then(|a| a.iter().find(|k| k["timestamp"].as_str() == Some(&minute.to_string()))).and_then(|k| num(&k["close"])) {
            out.push(Quote { source: "bitstamp", close_cents: c });
        }
    }
    // Binance ZEC/USDT: [openTime ms, open, high, low, close, ...]
    if let Some(v) = get(&http, &format!("https://api.binance.com/api/v3/klines?symbol=ZECUSDT&interval=1m&startTime={}&limit=1", minute * 1000)).await {
        if let Some(c) = v.as_array().and_then(|a| a.iter().find(|k| k[0].as_i64() == Some(minute * 1000))).and_then(|k| num(&k[4])) {
            out.push(Quote { source: "binance", close_cents: c });
        }
    }
    out
}

/// The attested ZEC/USD price for a minute: the lower median of the sources' closes, in
/// cents. Needs at least two sources.
pub async fn zecusd_minute(minute: i64) -> Result<(u64, Vec<Quote>)> {
    if minute % 60 != 0 {
        return Err(Error::Build("minute must be a multiple of 60".into()));
    }
    let q = zecusd_quotes(minute).await;
    if q.len() < 2 {
        return Err(Error::Rpc(format!("only {} price sources answered for minute {minute}", q.len())));
    }
    let mut c: Vec<u64> = q.iter().map(|x| x.close_cents).collect();
    c.sort();
    Ok((c[(c.len() - 1) / 2], q))
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + (m <= 2) as i64, m, d)
}

/// Sum of Ironwood notes decryptable by `ivk` in blocks `from..=to`.
pub async fn received(node: &Node, ivk: &orchard::keys::IncomingViewingKey, from: u32, to: u32) -> Result<u64> {
    use zcash_note_encryption::try_note_decryption;
    let pivk = orchard::keys::PreparedIncomingViewingKey::new(ivk);
    let mut total = 0u64;
    for h in from..=to {
        let block = node.block(h).await?;
        for txid in block["tx"].as_array().cloned().unwrap_or_default() {
            let raw = node
                .call("getrawtransaction", json!([txid.as_str().unwrap_or_default(), 0]))
                .await?;
            let raw = hex::decode(raw.as_str().unwrap_or_default()).map_err(|e| Error::Rpc(e.to_string()))?;
            let Some(t) = crate::recovery::parse_tx(&raw) else { continue };
            let Some(bundle) = t.ironwood_bundle() else { continue };
            for a in bundle.actions() {
                let domain = orchard::note_encryption::IronwoodDomain::for_action(a);
                if let Some((note, _, _)) = try_note_decryption(&domain, &pivk, a) {
                    total += note.value().inner();
                }
            }
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_to_cents_and_dates() {
        assert_eq!(cents("1141.57"), Some(114_157));
        assert_eq!(cents("1140.05000000"), Some(114_005));
        assert_eq!(cents("1142.965"), Some(114_297));
        assert_eq!(cents("1142"), Some(114_200));
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(20_746), (2026, 10, 20));
    }
}
