//! Payment links (ZIP 321) and their QR codes: how a user funds a contract from any
//! Zcash wallet.
//!
//! - Path A pays the contract's deposit receiver in the user's zDLC wallet: a unified
//!   address whose Orchard receiver is the Ironwood address (Ironwood shares the Orchard
//!   receiver), with a text memo naming the contract. The receiver identifies the
//!   deposit; the memo is only a label for the payer's history.
//! - Path B pays the contract's leg, a `t2…` address. ZIP 321 allows no memo to a
//!   transparent recipient, and none is needed: the address is unique to the contract.

use base64::Engine;
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use qrcode::{render::unicode, QrCode};
use zcash_address::unified::{self, Container, Encoding, Receiver, Uitem};
use zcash_protocol::consensus::NetworkType;
use zcash_transparent::address::TransparentAddress;

use crate::{keys, Error, Result};

/// ZIP 321 `qchar` allows unreserved characters plus a few sub-delims; everything else
/// in a label is percent-encoded.
const QCHAR_ESCAPE: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');

/// A unified address with only the Ironwood (Orchard-typed) receiver.
pub fn unified_address(addr: &orchard::Address, net: NetworkType) -> String {
    let ua = unified::Address::try_from_items(
        unified::Revision::R0,
        vec![Uitem::Data(Receiver::Orchard(addr.to_raw_address_bytes()))],
    )
    .expect("one shielded receiver is a valid unified address");
    ua.encode(&net)
}

/// The Ironwood address in a unified address (its Orchard receiver).
pub fn decode_unified(s: &str) -> Option<(NetworkType, orchard::Address)> {
    let (net, _, ua) = unified::Address::decode(s).ok()?;
    ua.items().into_iter().find_map(|r| match r {
        Receiver::Orchard(raw) => Option::from(orchard::Address::from_raw_address_bytes(&raw)).map(|a| (net, a)),
        _ => None,
    })
}

/// ZEC with up to eight decimals and no trailing zeros, as ZIP 321 writes amounts.
pub fn zec(zat: u64) -> String {
    let (whole, frac) = (zat / 100_000_000, zat % 100_000_000);
    if frac == 0 {
        return whole.to_string();
    }
    let f = format!("{frac:08}");
    format!("{whole}.{}", f.trim_end_matches('0'))
}

pub enum To<'a> {
    /// A deposit receiver (path A).
    Ironwood(&'a orchard::Address),
    /// A leg (path B).
    Transparent(&'a TransparentAddress),
}

/// A ZIP 321 payment request. A memo is only added for a shielded recipient.
pub fn uri(to: To<'_>, net: NetworkType, zat: u64, label: &str, memo: Option<&str>) -> String {
    let (addr, shielded) = match to {
        To::Ironwood(a) => (unified_address(a, net), true),
        To::Transparent(t) => (keys::encode(t, net), false),
    };
    let mut u = format!("zcash:{addr}?amount={}", zec(zat));
    if !label.is_empty() {
        u += &format!("&label={}", utf8_percent_encode(label, QCHAR_ESCAPE));
    }
    if let (true, Some(m)) = (shielded, memo) {
        u += &format!("&memo={}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(m.as_bytes()));
    }
    u
}

/// The deposit link for contract `id`: path A.
pub fn deposit_uri(addr: &orchard::Address, net: NetworkType, zat: u64, id: &str) -> String {
    uri(To::Ironwood(addr), net, zat, &format!("zDLC {id}"), Some(&format!("zDLC1:{id}")))
}

/// The leg link for contract `id`: path B.
pub fn leg_uri(addr: &TransparentAddress, net: NetworkType, zat: u64, id: &str) -> String {
    uri(To::Transparent(addr), net, zat, &format!("zDLC {id}"), None)
}

/// What a wallet reads from a link: (address, zatoshis, memo). Single-payment links only.
pub fn parse(uri: &str) -> Option<(String, u64, Option<Vec<u8>>)> {
    let rest = uri.strip_prefix("zcash:")?;
    let (addr, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (mut zat, mut memo) = (None, None);
    for kv in query.split('&').filter(|x| !x.is_empty()) {
        let (k, v) = kv.split_once('=')?;
        match k {
            "amount" => {
                let (w, f) = v.split_once('.').unwrap_or((v, ""));
                if f.len() > 8 {
                    return None;
                }
                zat = Some(w.parse::<u64>().ok()? * 100_000_000 + format!("{f:0<8}").parse::<u64>().ok()?);
            }
            "memo" => memo = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(v).ok(),
            _ => {}
        }
    }
    Some((addr.to_string(), zat?, memo))
}

fn code(data: &str) -> Result<QrCode> {
    QrCode::new(data.as_bytes()).map_err(|e| Error::Build(format!("QR: {e}")))
}

/// The QR as terminal text (two rows per character).
pub fn qr_text(data: &str) -> Result<String> {
    Ok(code(data)?.render::<unicode::Dense1x2>().quiet_zone(true).build())
}

/// The QR as an SVG document, for apps and pages.
pub fn qr_svg(data: &str) -> Result<String> {
    Ok(code(data)?.render::<qrcode::render::svg::Color>().min_dimensions(240, 240).quiet_zone(true).build())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ironwood(seed: u8) -> orchard::Address {
        let sk = (0u8..=255)
            .find_map(|i| Option::<orchard::keys::SpendingKey>::from(orchard::keys::SpendingKey::from_bytes([seed ^ i; 32])))
            .unwrap();
        orchard::keys::FullViewingKey::from(&sk).address_at(0u32, orchard::keys::Scope::External)
    }

    #[test]
    fn amounts_as_zip321_writes_them() {
        // ZIP 321's examples: "amount=1", "amount=0.0001", "amount=123.456".
        assert_eq!(zec(1_0000_0000), "1");
        assert_eq!(zec(1_0000), "0.0001");
        assert_eq!(zec(123_4560_0000), "123.456");
        assert_eq!(zec(1), "0.00000001");
        assert_eq!(zec(1_0200_0000), "1.02");
        assert_eq!(zec(0), "0");
    }

    #[test]
    fn deposit_link_round_trips() {
        let a = ironwood(7);
        let ua = unified_address(&a, NetworkType::Test);
        assert!(ua.starts_with("utest1"), "{ua}");
        assert_eq!(decode_unified(&ua), Some((NetworkType::Test, a)));
        let u = deposit_uri(&a, NetworkType::Test, 1_0200_0000, "9f3a");
        assert_eq!(u, format!("zcash:{ua}?amount=1.02&label=zDLC%209f3a&memo=ekRMQzE6OWYzYQ"));
        // base64url of "zDLC1:9f3a", no padding (ZIP 321 memos).
        assert_eq!(base64::engine::general_purpose::URL_SAFE_NO_PAD.decode("ekRMQzE6OWYzYQ").unwrap(), b"zDLC1:9f3a");
        assert_eq!(parse(&u), Some((ua.clone(), 1_0200_0000, Some(b"zDLC1:9f3a".to_vec()))));
        assert!(qr_text(&u).unwrap().lines().count() > 20);
        assert!(qr_svg(&u).unwrap().starts_with("<?xml"));
    }

    #[test]
    fn leg_link_has_no_memo() {
        let t = TransparentAddress::ScriptHash([3; 20]);
        let u = leg_uri(&t, NetworkType::Test, 1_0000, "ab");
        assert!(u.starts_with("zcash:t2"), "{u}");
        assert!(u.ends_with("?amount=0.0001&label=zDLC%20ab"));
    }
}
