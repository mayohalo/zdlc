//! zDLC's protocol core for JavaScript (WebAssembly): a stateless contract API in the
//! style of ddk-ffi. The app keeps the messages and its own records, talks to its peer,
//! its oracles and the chain; these functions build, check and sign. Every value
//! crossing the boundary is plain JSON-shaped data: the same wire messages
//! (`WireOffer`, `WireAccept`, `WireSign`), templates and oracle messages as
//! `zdlc-node`.
//!
//! A context names the wallet and contract: `{ seed, index, network, inputs }`, where
//! `seed` is 32 bytes of hex, `index` the contract's key index, `network` is "regtest",
//! "testnet" or a node's `getblockchaininfo` result, and `inputs` this side's treasury
//! coins (`{ txid, index, value, pubkey }`, spent with the wallet's treasury key).

use orchard::Anchor;
use serde::Deserialize;
use serde_json::{json, Value};
use wasm_bindgen::prelude::*;
use zcash_protocol::{consensus::NetworkType, local_consensus::LocalNetwork};
use zdlc::{
    contract::{Contract, Party, PartyInfo},
    events::{self, AnnouncementMsg, AttestationMsg, EventSpec, MemoryOracle},
    keys, params, paylink,
    templates::{Role, Template},
    tx::{self, Coin, Spend},
    txid_from_hex, txid_hex,
    wallet::Wallet,
    wire::{self, WireAccept, WireCoin, WireOffer, WireSign, WireTerms},
};

type R<T> = Result<T, JsError>;

fn err(e: impl std::fmt::Display) -> JsError {
    JsError::new(&e.to_string())
}

fn input<T: serde::de::DeserializeOwned>(v: JsValue) -> R<T> {
    serde_wasm_bindgen::from_value(v).map_err(err)
}

fn output(v: &impl serde::Serialize) -> R<JsValue> {
    v.serialize(&serde_wasm_bindgen::Serializer::json_compatible()).map_err(err)
}

#[wasm_bindgen(start)]
pub fn start() {
    #[cfg(target_arch = "wasm32")]
    console_error_panic_hook::set_once();
}

#[wasm_bindgen]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").into()
}

// ---- Context ----------------------------------------------------------------------------

#[derive(Deserialize)]
struct Ctx {
    seed: String,
    index: u32,
    network: Value,
    #[serde(default)]
    inputs: Vec<WireCoin>,
}

struct Side {
    wallet: Wallet,
    index: u32,
    p: LocalNetwork,
    net: NetworkType,
    inputs: Vec<Coin>,
}

fn network(v: &Value) -> R<(LocalNetwork, NetworkType)> {
    Ok(match v {
        Value::String(s) if s == "regtest" => (params::regtest(Some(params::NU7_AT)), NetworkType::Regtest),
        Value::String(s) if s == "testnet" => (params::testnet(), NetworkType::Test),
        Value::Object(o) => {
            let p = params::from_node(&o["upgrades"]).ok_or(err("network: no NU6.3 in upgrades"))?;
            let net = match o.get("chain").and_then(Value::as_str) {
                Some("main") => NetworkType::Main,
                Some("test") => NetworkType::Test,
                _ => NetworkType::Regtest,
            };
            (p, net)
        }
        _ => return Err(err("network: \"regtest\", \"testnet\" or a getblockchaininfo result")),
    })
}

fn side(ctx: JsValue) -> R<Side> {
    let c: Ctx = input(ctx)?;
    let seed: [u8; 32] = hex::decode(&c.seed).ok().and_then(|b| b.try_into().ok()).ok_or(err("seed: 32 bytes of hex"))?;
    let (p, net) = network(&c.network)?;
    let inputs = c
        .inputs
        .iter()
        .map(|w| {
            let pk = secp256k1::PublicKey::from_slice(&hex::decode(&w.pubkey).map_err(err)?).map_err(err)?;
            Ok(Coin { txid: txid_from_hex(&w.txid).map_err(err)?, index: w.index, value: w.value, spend: Spend::P2pkh(pk) })
        })
        .collect::<R<_>>()?;
    Ok(Side { wallet: Wallet::from_seed(seed), index: c.index, p, net, inputs })
}

impl Side {
    /// This side of contract `index`: treasury coins in, payouts to the contract's own
    /// addresses (the same layout as zdlc-node).
    fn party(&self) -> Party {
        let (w, i) = (&self.wallet, self.index);
        Party {
            funding_sk: w.funding_key(i),
            input_keys: vec![w.coin_key(0); self.inputs.len()],
            close_fvk: w.close_fvk(i),
            roll_key: None,
            ironwood: None,
            info: PartyInfo {
                funding_pubkey: keys::pubkey(&w.funding_key(i)),
                inputs: self.inputs.clone(),
                change: w.coin_address(0),
                payout: w.payout_address(i),
                close: w.close_address(i),
                ironwood: false,
            },
        }
    }

    fn contract(&self, role: &str, offer: JsValue, accept: JsValue, sign: JsValue) -> R<Contract> {
        let offer = wire::offer_from_wire(&input::<WireOffer>(offer)?).map_err(err)?;
        let accept = wire::accept_from_wire(&input::<WireAccept>(accept)?).map_err(err)?;
        let sign = wire::sign_from_wire(&input::<WireSign>(sign)?).map_err(err)?;
        self.party().restore(&self.p, role_of(role)? == Role::Offerer, &offer, &accept, &sign).map_err(err)
    }
}

fn role_of(r: &str) -> R<Role> {
    match r {
        "offerer" => Ok(Role::Offerer),
        "accepter" => Ok(Role::Accepter),
        _ => Err(err("role: \"offerer\" or \"accepter\"")),
    }
}

fn final_tx(f: &tx::Final) -> Value {
    json!({"tx": hex::encode(&f.raw), "txid": txid_hex(&f.txid), "lockTime": f.lock_time})
}

// ---- Wallet ------------------------------------------------------------------------------

/// The wallet's addresses for contract `index`: identity (what peers offer to), treasury
/// (fund it with transparent coins), and the contract's deposit and close receivers.
#[wasm_bindgen(js_name = walletInfo)]
pub fn wallet_info(ctx: JsValue) -> R<JsValue> {
    let s = side(ctx)?;
    let (w, i) = (&s.wallet, s.index);
    output(&json!({
        "identity": wire::identity(&w.identity_key()),
        "fundingPubkey": hex::encode(keys::pubkey(&w.funding_key(i)).serialize()),
        "treasury": keys::encode(&w.coin_address(0), s.net),
        "treasuryPubkey": hex::encode(keys::pubkey(&w.coin_key(0)).serialize()),
        "payoutAddress": keys::encode(&w.payout_address(i), s.net),
        "depositAddress": paylink::unified_address(&w.deposit_address(i), s.net),
        "closeAddress": paylink::unified_address(&w.close_address(i), s.net),
    }))
}

// ---- Templates and oracle messages ------------------------------------------------------

/// Fills what an offerer decides at offer time (price minute, payment window).
#[wasm_bindgen(js_name = templateFill)]
pub fn template_fill(template: JsValue, now_unix: f64, tip: u32, block_secs: f64) -> R<JsValue> {
    let mut t: Template = input(template)?;
    t.fill(now_unix as i64, tip, block_secs as i64);
    output(&t)
}

/// The oracle event a (filled) template settles on: what to announce.
#[wasm_bindgen(js_name = templateEvent)]
pub fn template_event(template: JsValue) -> R<JsValue> {
    let t: Template = input(template)?;
    t.validate().map_err(err)?;
    output(&t.event().map_err(err)?)
}

/// The exact terms from a template, the oracles' announcements and the sign height. An
/// accepter derives them itself and compares with the offer's.
#[wasm_bindgen(js_name = templateTerms)]
pub fn template_terms(template: JsValue, announcements: JsValue, sign_height: u32) -> R<JsValue> {
    let t: Template = input(template)?;
    let anns: Vec<AnnouncementMsg> = input(announcements)?;
    output(&wire::terms_to_wire(&t.terms(&anns, sign_height).map_err(err)?))
}

#[wasm_bindgen(js_name = templateSummary)]
pub fn template_summary(template: JsValue, role: &str) -> R<JsValue> {
    let t: Template = input(template)?;
    output(&t.summary(role_of(role)?))
}

/// Checks an attestation against its announcement: `{ outcome }` or `{ value }`.
#[wasm_bindgen(js_name = verifyAttestation)]
pub fn verify_attestation(announcement: JsValue, attestation: JsValue) -> R<JsValue> {
    let a: AnnouncementMsg = input(announcement)?;
    let t: AttestationMsg = input(attestation)?;
    match a.spec {
        EventSpec::ZecUsd { .. } => output(&json!({"value": events::verify_numeric(&events::to_numeric(&a).map_err(err)?, &t).map_err(err)?.value})),
        _ => output(&json!({"outcome": events::verify_outcome(&events::to_enumerated(&a).map_err(err)?, &t).map_err(err)?.0})),
    }
}

// ---- The contract, stateless ---------------------------------------------------------------

/// Offerer: the offer for `terms` (a `WireTerms`, e.g. from `templateTerms`).
#[wasm_bindgen]
pub fn offer(ctx: JsValue, terms: JsValue) -> R<JsValue> {
    let s = side(ctx)?;
    let terms = wire::terms_from_wire(&input::<WireTerms>(terms)?).map_err(err)?;
    output(&wire::offer_to_wire(&s.party().offer(terms)).map_err(err)?)
}

/// Accepter: checks the offer at `tip`, adaptor-signs every CET and signs the refund.
#[wasm_bindgen]
pub fn accept(ctx: JsValue, offer: JsValue, tip: u32) -> R<JsValue> {
    let s = side(ctx)?;
    let offer = wire::offer_from_wire(&input::<WireOffer>(offer)?).map_err(err)?;
    let (a, txs) = s.party().accept(&s.p, tip, &offer).map_err(err)?;
    output(&json!({"accept": wire::accept_to_wire(&a).map_err(err)?, "fundingTxid": txid_hex(&txs.funding_txid), "cets": txs.cets.len()}))
}

/// Offerer: verifies every accepter signature and signs: CETs, refund, its funding inputs.
#[wasm_bindgen]
pub fn sign(ctx: JsValue, offer: JsValue, accept: JsValue, tip: u32) -> R<JsValue> {
    let s = side(ctx)?;
    let offer = wire::offer_from_wire(&input::<WireOffer>(offer)?).map_err(err)?;
    let accept = wire::accept_from_wire(&input::<WireAccept>(accept)?).map_err(err)?;
    let (m, c) = s.party().sign(&s.p, tip, &offer, &accept).map_err(err)?;
    output(&json!({"sign": wire::sign_to_wire(&m), "fundingTxid": txid_hex(&c.txs.funding_txid)}))
}

/// Accepter: verifies the offerer's signatures and returns the funding transaction to
/// broadcast.
#[wasm_bindgen]
pub fn finish(ctx: JsValue, offer: JsValue, accept: JsValue, sign: JsValue) -> R<JsValue> {
    let s = side(ctx)?;
    let offer = wire::offer_from_wire(&input::<WireOffer>(offer)?).map_err(err)?;
    let accept = wire::accept_from_wire(&input::<WireAccept>(accept)?).map_err(err)?;
    let sign = wire::sign_from_wire(&input::<WireSign>(sign)?).map_err(err)?;
    let txs = zdlc::contract::build_txs(&s.p, &offer.terms, &offer.offer, &accept.accept).map_err(err)?;
    let (f, _) = s.party().finish(&offer, txs, &sign).map_err(err)?;
    output(&final_tx(&f))
}

fn attested(c: &Contract, anns: JsValue, atts: JsValue) -> R<(Option<Vec<zdlc::oracle::NumericAttestation>>, Option<(String, Vec<secp256k1_zkp::SecretKey>)>)> {
    let anns: Vec<AnnouncementMsg> = input(anns)?;
    let atts: Vec<AttestationMsg> = input(atts)?;
    if anns.len() != atts.len() || anns.len() != c.terms.event.oracles() {
        return Err(err("one announcement and one attestation per oracle"));
    }
    match &c.terms.event {
        zdlc::contract::Event::Numeric(_) => Ok((
            Some(anns.iter().zip(&atts).map(|(a, t)| events::verify_numeric(&events::to_numeric(a)?, t)).collect::<zdlc::Result<_>>().map_err(err)?),
            None,
        )),
        zdlc::contract::Event::Enumerated(_) => {
            let v: Vec<(String, secp256k1_zkp::SecretKey)> =
                anns.iter().zip(&atts).map(|(a, t)| events::verify_outcome(&events::to_enumerated(a)?, t)).collect::<zdlc::Result<_>>().map_err(err)?;
            if v.iter().any(|(o, _)| *o != v[0].0) {
                return Err(err("the oracles disagree"));
            }
            Ok((None, Some((v[0].0.clone(), v.into_iter().map(|(_, s)| s).collect()))))
        }
    }
}


/// Either side, once the oracles attest: the CET for the attested outcome, signed and
/// ready to broadcast at height `tip + 1` or later.
#[wasm_bindgen]
#[allow(clippy::too_many_arguments)]
pub fn cet(ctx: JsValue, role: &str, offer: JsValue, accept: JsValue, sign: JsValue, announcements: JsValue, attestations: JsValue, tip: u32) -> R<JsValue> {
    let s = side(ctx)?;
    let c = s.contract(role, offer, accept, sign)?;
    let f = match attested(&c, announcements, attestations)? {
        (Some(n), _) => c.cet_numeric(&n, tip + 1).map_err(err)?.0,
        (_, Some((o, scalars))) => c.cet_outcome(&o, &scalars, tip + 1).map_err(err)?,
        _ => unreachable!(),
    };
    output(&final_tx(&f))
}

/// Either side: the refund, valid from its lock height.
#[wasm_bindgen]
pub fn refund(ctx: JsValue, role: &str, offer: JsValue, accept: JsValue, sign: JsValue) -> R<JsValue> {
    let s = side(ctx)?;
    let c = s.contract(role, offer, accept, sign)?;
    output(&final_tx(&c.refund().map_err(err)?))
}

fn anchor(h: &str) -> R<Anchor> {
    let mut b: [u8; 32] = hex::decode(h).ok().and_then(|b| b.try_into().ok()).ok_or(err("anchor: 32 bytes of hex"))?;
    if let Some(a) = Option::<Anchor>::from(Anchor::from_bytes(b)) {
        return Ok(a);
    }
    b.reverse();
    Option::<Anchor>::from(Anchor::from_bytes(b)).ok_or(err("anchor is not a field element"))
}

fn close_spec(s: &Side, c: &Contract, anns: JsValue, atts: JsValue, tip: u32, anchor_hex: &str) -> R<tx::Spec> {
    let a = anchor(anchor_hex)?;
    match attested(c, anns, atts)? {
        (Some(n), _) => c.close_spec(&s.p, tip, c.terms.payout_at(n[0].value).map_err(err)?, a).map_err(err),
        (_, Some((o, _))) => c.close_outcome(&s.p, tip, &o, a).map_err(err),
        _ => unreachable!(),
    }
}

/// Offerer: the cooperative close of the attested outcome, built at `tip` against the
/// Ironwood root `anchor` (`z_gettreestate` at `tip`). Proves the Ironwood outputs:
/// slow in a browser (seconds to build the proving key the first time).
#[wasm_bindgen(js_name = closeBuild)]
#[allow(clippy::too_many_arguments)]
pub fn close_build(ctx: JsValue, role: &str, offer: JsValue, accept: JsValue, sign: JsValue, announcements: JsValue, attestations: JsValue, tip: u32, anchor: &str) -> R<JsValue> {
    let s = side(ctx)?;
    let c = s.contract(role, offer, accept, sign)?;
    let spec = close_spec(&s, &c, announcements, attestations, tip, anchor)?;
    let p = tx::build(&s.p, &spec).map_err(err)?;
    output(&json!({"pczt": hex::encode(p.serialize().map_err(|e| err(format!("{e:?}")))?)}))
}

/// Either side: checks a close against the attested outcome (every output re-derived
/// from its commitment) and signs it. The accepter co-signs and returns it; the offerer
/// signs last and gets the transaction to broadcast.
#[wasm_bindgen(js_name = closeSign)]
#[allow(clippy::too_many_arguments)]
pub fn close_sign(ctx: JsValue, role: &str, offer: JsValue, accept: JsValue, sign: JsValue, announcements: JsValue, attestations: JsValue, pczt: &str, tip: u32, anchor: &str) -> R<JsValue> {
    let s = side(ctx)?;
    let r = role_of(role)?;
    let c = s.contract(role, offer, accept, sign)?;
    let spec = close_spec(&s, &c, announcements, attestations, tip, anchor)?;
    let p = pczt::Pczt::parse(&hex::decode(pczt).map_err(err)?).map_err(|e| err(format!("{e:?}")))?;
    c.check_close(&p, &spec).map_err(err)?;
    let signed = c.sign_close(p).map_err(err)?;
    if r == Role::Offerer {
        output(&final_tx(&tx::extract(signed).map_err(err)?))
    } else {
        output(&json!({"pczt": hex::encode(signed.serialize().map_err(|e| err(format!("{e:?}")))?)}))
    }
}

// ---- Payment links -------------------------------------------------------------------------

/// A ZIP 321 deposit link for a unified address (its Ironwood receiver).
#[wasm_bindgen(js_name = depositUri)]
pub fn deposit_uri(unified_address: &str, zat: f64, id: &str) -> R<String> {
    let (net, a) = paylink::decode_unified(unified_address).ok_or(err("not a unified address with an Ironwood receiver"))?;
    Ok(paylink::deposit_uri(&a, net, zat as u64, id))
}

/// A ZIP 321 link to a transparent address (a leg); no memo.
#[wasm_bindgen(js_name = legUri)]
pub fn leg_uri(address: &str, zat: f64, id: &str) -> R<String> {
    let (net, a) = keys::decode(address).ok_or(err("not a transparent address"))?;
    Ok(paylink::leg_uri(&a, net, zat as u64, id))
}

/// What a wallet reads from a link: `{ address, zat, memo }`.
#[wasm_bindgen(js_name = parseUri)]
pub fn parse_uri(uri: &str) -> R<JsValue> {
    let (address, zat, memo) = paylink::parse(uri).ok_or(err("not a ZIP 321 link"))?;
    output(&json!({"address": address, "zat": zat, "memo": memo.map(|m| String::from_utf8_lossy(&m).into_owned())}))
}

#[wasm_bindgen(js_name = qrSvg)]
pub fn qr_svg(text: &str) -> R<String> {
    paylink::qr_svg(text).map_err(err)
}

// ---- An oracle in memory --------------------------------------------------------------------

/// An oracle held in memory (demos, tests, an in-browser arbiter). Same messages and
/// signatures as zdlc-oracled; refuses a second outcome for an event.
#[wasm_bindgen]
pub struct Oracle(MemoryOracle);

#[wasm_bindgen]
impl Oracle {
    #[wasm_bindgen(constructor)]
    pub fn new(seed: &str) -> R<Oracle> {
        let s: [u8; 32] = hex::decode(seed).ok().and_then(|b| b.try_into().ok()).ok_or(err("seed: 32 bytes of hex"))?;
        Ok(Oracle(MemoryOracle::new(s)))
    }

    pub fn pubkey(&self) -> String {
        self.0.pubkey()
    }

    pub fn announce(&self, event_id: &str, spec: JsValue) -> R<JsValue> {
        let spec: EventSpec = input(spec)?;
        output(&self.0.announce(event_id, &spec))
    }

    /// `outcome`: an outcome name, or the value (in the event's units) for ZEC/USD.
    pub fn attest(&mut self, event_id: &str, spec: JsValue, outcome: &str) -> R<JsValue> {
        let spec: EventSpec = input(spec)?;
        output(&self.0.attest(event_id, &spec, outcome).map_err(err)?)
    }
}
