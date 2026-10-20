//! The wire format: offer, accept and sign as JSON (binary fields in hex), wrapped in
//! signed, end-to-end encrypted envelopes for the desk's mailbox.
//!
//! An envelope is signed (ECDSA, secp256k1) by the sender's identity key over a tagged
//! hash of every field, then encrypted to the recipient's identity key: an ephemeral
//! secp256k1 key, ECDH, SHA-256 KDF, ChaCha20-Poly1305. The desk stores and forwards
//! boxes; it sees the recipient, sizes and timing, never the contents or the sender.

use chacha20poly1305::{aead::Aead, ChaCha20Poly1305, KeyInit, Nonce};
use secp256k1::{ecdh::SharedSecret, ecdsa, Message, PublicKey, SecretKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zcash_transparent::address::TransparentAddress;

use crate::{
    adaptor::AdaptorSignature,
    contract::{Accept, Beneficiary, Event, Offer, PartyInfo, Payee, Payouts, SignMsg, Split, Splits, Terms},
    oracle::{Announcement, NumericAnnouncement},
    payout::StableValue,
    tx::{Coin, Spend},
    txid_from_hex, txid_hex, Error, Result,
};

fn err(e: impl std::fmt::Display) -> Error {
    Error::Build(format!("wire: {e}"))
}

fn unhex<const N: usize>(h: &str) -> Result<[u8; N]> {
    hex::decode(h).map_err(err)?.try_into().map_err(|_| err("length"))
}

fn taddr(a: &TransparentAddress) -> String {
    match a {
        TransparentAddress::PublicKeyHash(h) => format!("pkh:{}", hex::encode(h)),
        TransparentAddress::ScriptHash(h) => format!("sh:{}", hex::encode(h)),
    }
}

fn parse_taddr(s: &str) -> Result<TransparentAddress> {
    match s.split_once(':') {
        Some(("pkh", h)) => Ok(TransparentAddress::PublicKeyHash(unhex(h)?)),
        Some(("sh", h)) => Ok(TransparentAddress::ScriptHash(unhex(h)?)),
        _ => Err(err("transparent address")),
    }
}

fn xonly(h: &str) -> Result<secp256k1_zkp::XOnlyPublicKey> {
    secp256k1_zkp::XOnlyPublicKey::from_slice(&hex::decode(h).map_err(err)?).map_err(err)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WireCoin {
    pub txid: String,
    pub index: u32,
    pub value: u64,
    /// P2PKH key; funding inputs are P2PKH.
    pub pubkey: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WireParty {
    pub funding_pubkey: String,
    pub inputs: Vec<WireCoin>,
    pub change: String,
    pub payout: String,
    /// Raw Ironwood address, 43 bytes.
    pub close: String,
    /// Funds from its own Ironwood notes (it builds the funding transaction).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ironwood: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WireNumeric {
    pub pubkey: String,
    pub nonces: Vec<String>,
    pub event_id: String,
    pub nb_digits: usize,
    pub unit_cents: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WireEnum {
    pub pubkey: String,
    pub nonce: String,
    pub event_id: String,
    pub outcomes: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum WireEvent {
    Numeric(Vec<WireNumeric>),
    Enumerated(Vec<WireEnum>),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum WirePayouts {
    Enumerated(Vec<(String, u64)>),
    StableValue {
        d_cents: u64,
        q_recipient: u64,
        q_hedger: u64,
        unit_cents: u64,
        nb_digits: usize,
        rounding: u64,
        premium: u64,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WireTerms {
    pub event: WireEvent,
    pub payouts: WirePayouts,
    pub offer_collateral: u64,
    pub accept_collateral: u64,
    pub maturity: u32,
    pub refund_height: u32,
    pub sign_height: u32,
    pub dual_sign: bool,
    /// Beneficiaries and splits. Absent (older agents, default contracts): each side is
    /// paid to its own party.
    #[serde(default, skip_serializing_if = "WireSplits::is_empty")]
    pub splits: WireSplits,
}

/// Payees are "offer", "accept" or "b<index>" (a beneficiary).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct WireSplits {
    #[serde(default)]
    pub beneficiaries: Vec<WireBeneficiary>,
    #[serde(default)]
    pub offer: Vec<(String, u32)>,
    #[serde(default)]
    pub accept: Vec<(String, u32)>,
    #[serde(default)]
    pub outcomes: Vec<(String, Vec<(String, u64)>)>,
}

impl WireSplits {
    pub fn is_empty(&self) -> bool {
        *self == WireSplits::default()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WireBeneficiary {
    pub label: String,
    pub payout: String,
    /// Raw Ironwood address (43 bytes, hex), if the close pays it shielded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close: Option<String>,
}

fn payee_to_wire(p: &Payee) -> String {
    match p {
        Payee::Offer => "offer".into(),
        Payee::Accept => "accept".into(),
        Payee::Beneficiary(i) => format!("b{i}"),
    }
}

fn payee_from_wire(s: &str) -> Result<Payee> {
    match s {
        "offer" => Ok(Payee::Offer),
        "accept" => Ok(Payee::Accept),
        b => b
            .strip_prefix('b')
            .and_then(|i| i.parse().ok())
            .map(Payee::Beneficiary)
            .ok_or(err("payee")),
    }
}

fn ironwood_from_hex(h: &str) -> Result<orchard::Address> {
    Option::from(orchard::Address::from_raw_address_bytes(&unhex(h)?)).ok_or(err("Ironwood address"))
}

pub fn splits_to_wire(s: &Splits) -> WireSplits {
    let split = |x: &Split| x.0.iter().map(|(p, b)| (payee_to_wire(p), *b)).collect();
    WireSplits {
        beneficiaries: s
            .beneficiaries
            .iter()
            .map(|b| WireBeneficiary {
                label: b.label.clone(),
                payout: taddr(&b.payout),
                close: b.close.map(|a| hex::encode(a.to_raw_address_bytes())),
            })
            .collect(),
        offer: split(&s.offer),
        accept: split(&s.accept),
        outcomes: s
            .outcomes
            .iter()
            .map(|(o, l)| (o.clone(), l.iter().map(|(p, v)| (payee_to_wire(p), *v)).collect()))
            .collect(),
    }
}

pub fn splits_from_wire(w: &WireSplits) -> Result<Splits> {
    let split = |x: &[(String, u32)]| -> Result<Split> {
        Ok(Split(x.iter().map(|(p, b)| Ok((payee_from_wire(p)?, *b))).collect::<Result<_>>()?))
    };
    Ok(Splits {
        beneficiaries: w
            .beneficiaries
            .iter()
            .map(|b| {
                Ok(Beneficiary {
                    label: b.label.clone(),
                    payout: parse_taddr(&b.payout)?,
                    close: b.close.as_deref().map(ironwood_from_hex).transpose()?,
                })
            })
            .collect::<Result<_>>()?,
        offer: split(&w.offer)?,
        accept: split(&w.accept)?,
        outcomes: w
            .outcomes
            .iter()
            .map(|(o, l)| {
                Ok((o.clone(), l.iter().map(|(p, v)| Ok((payee_from_wire(p)?, *v))).collect::<Result<_>>()?))
            })
            .collect::<Result<_>>()?,
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WireOffer {
    pub terms: WireTerms,
    pub offer: WireParty,
    /// The Ironwood-funding offerer's redacted funding PCZT, hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub funding: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WireAccept {
    pub accept: WireParty,
    pub cet_adaptors: Vec<Vec<String>>,
    pub refund_sig: String,
    /// The Ironwood-funding accepter's redacted funding PCZT, hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub funding: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WireSign {
    pub cet_adaptors: Vec<Vec<String>>,
    pub refund_sig: String,
    pub funding_pczt: String,
    /// The offerer's signatures on leg inputs: (input index, compact signature hex).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub leg_sigs: Vec<(usize, String)>,
}

pub fn party_to_wire(p: &PartyInfo) -> Result<WireParty> {
    Ok(WireParty {
        funding_pubkey: hex::encode(p.funding_pubkey.serialize()),
        inputs: p
            .inputs
            .iter()
            .map(|c| match &c.spend {
                Spend::P2pkh(pk) => Ok(WireCoin {
                    txid: txid_hex(&c.txid),
                    index: c.index,
                    value: c.value,
                    pubkey: hex::encode(pk.serialize()),
                }),
                Spend::P2sh(_) | Spend::Leg(_) => Err(err("funding inputs must be P2PKH")),
            })
            .collect::<Result<_>>()?,
        change: taddr(&p.change),
        payout: taddr(&p.payout),
        close: hex::encode(p.close.to_raw_address_bytes()),
        ironwood: p.ironwood,
    })
}

pub fn party_from_wire(w: &WireParty) -> Result<PartyInfo> {
    let pk = |h: &str| PublicKey::from_slice(&hex::decode(h).map_err(err)?).map_err(err);
    Ok(PartyInfo {
        funding_pubkey: pk(&w.funding_pubkey)?,
        inputs: w
            .inputs
            .iter()
            .map(|c| {
                Ok(Coin {
                    txid: txid_from_hex(&c.txid)?,
                    index: c.index,
                    value: c.value,
                    spend: Spend::P2pkh(pk(&c.pubkey)?),
                })
            })
            .collect::<Result<_>>()?,
        change: parse_taddr(&w.change)?,
        payout: parse_taddr(&w.payout)?,
        close: Option::from(orchard::Address::from_raw_address_bytes(&unhex::<43>(&w.close)?))
            .ok_or(err("Ironwood address"))?,
        ironwood: w.ironwood,
    })
}

pub fn terms_to_wire(t: &Terms) -> WireTerms {
    WireTerms {
        event: match &t.event {
            Event::Numeric(a) => WireEvent::Numeric(
                a.iter()
                    .map(|a| WireNumeric {
                        pubkey: hex::encode(a.pubkey.serialize()),
                        nonces: a.nonces.iter().map(|n| hex::encode(n.serialize())).collect(),
                        event_id: a.event_id.clone(),
                        nb_digits: a.nb_digits,
                        unit_cents: a.unit_cents,
                    })
                    .collect(),
            ),
            Event::Enumerated(a) => WireEvent::Enumerated(
                a.iter()
                    .map(|a| WireEnum {
                        pubkey: hex::encode(a.pubkey.serialize()),
                        nonce: hex::encode(a.nonce.serialize()),
                        event_id: a.event_id.clone(),
                        outcomes: a.outcomes.clone(),
                    })
                    .collect(),
            ),
        },
        payouts: match &t.payouts {
            Payouts::Enumerated(l) => WirePayouts::Enumerated(l.clone()),
            Payouts::StableValue(c) => WirePayouts::StableValue {
                d_cents: c.d_cents,
                q_recipient: c.q_recipient,
                q_hedger: c.q_hedger,
                unit_cents: c.unit_cents,
                nb_digits: c.nb_digits,
                rounding: c.rounding,
                premium: c.premium,
            },
        },
        offer_collateral: t.offer_collateral,
        accept_collateral: t.accept_collateral,
        maturity: t.maturity,
        refund_height: t.refund_height,
        sign_height: t.sign_height,
        dual_sign: t.dual_sign,
        splits: splits_to_wire(&t.splits),
    }
}

pub fn terms_from_wire(w: &WireTerms) -> Result<Terms> {
    Ok(Terms {
        event: match &w.event {
            WireEvent::Numeric(a) => Event::Numeric(
                a.iter()
                    .map(|a| {
                        Ok(NumericAnnouncement {
                            pubkey: xonly(&a.pubkey)?,
                            nonces: a.nonces.iter().map(|n| xonly(n)).collect::<Result<_>>()?,
                            event_id: a.event_id.clone(),
                            nb_digits: a.nb_digits,
                            unit_cents: a.unit_cents,
                        })
                    })
                    .collect::<Result<_>>()?,
            ),
            WireEvent::Enumerated(a) => Event::Enumerated(
                a.iter()
                    .map(|a| {
                        Ok(Announcement {
                            pubkey: xonly(&a.pubkey)?,
                            nonce: xonly(&a.nonce)?,
                            event_id: a.event_id.clone(),
                            outcomes: a.outcomes.clone(),
                        })
                    })
                    .collect::<Result<_>>()?,
            ),
        },
        payouts: match &w.payouts {
            WirePayouts::Enumerated(l) => Payouts::Enumerated(l.clone()),
            WirePayouts::StableValue { d_cents, q_recipient, q_hedger, unit_cents, nb_digits, rounding, premium } => {
                Payouts::StableValue(StableValue {
                    d_cents: *d_cents,
                    q_recipient: *q_recipient,
                    q_hedger: *q_hedger,
                    unit_cents: *unit_cents,
                    nb_digits: *nb_digits,
                    rounding: *rounding,
                    premium: *premium,
                })
            }
        },
        offer_collateral: w.offer_collateral,
        accept_collateral: w.accept_collateral,
        maturity: w.maturity,
        refund_height: w.refund_height,
        sign_height: w.sign_height,
        dual_sign: w.dual_sign,
        roll: None,
        splits: splits_from_wire(&w.splits)?,
    })
}

fn adaptors_to_wire(a: &[Vec<AdaptorSignature>]) -> Vec<Vec<String>> {
    a.iter().map(|r| r.iter().map(|s| hex::encode(s.as_ref())).collect()).collect()
}

fn adaptors_from_wire(a: &[Vec<String>]) -> Result<Vec<Vec<AdaptorSignature>>> {
    a.iter()
        .map(|r| {
            r.iter()
                .map(|s| AdaptorSignature::from_slice(&hex::decode(s).map_err(err)?).map_err(err))
                .collect()
        })
        .collect()
}

fn sig(h: &str) -> Result<ecdsa::Signature> {
    ecdsa::Signature::from_compact(&hex::decode(h).map_err(err)?).map_err(err)
}

pub fn offer_to_wire(o: &Offer) -> Result<WireOffer> {
    if o.terms.roll.is_some() {
        return Err(err("rolled contracts are negotiated in-process for now, not over the desk"));
    }
    Ok(WireOffer { terms: terms_to_wire(&o.terms), offer: party_to_wire(&o.offer)?, funding: o.funding.as_ref().map(hex::encode) })
}

pub fn offer_from_wire(w: &WireOffer) -> Result<Offer> {
    Ok(Offer {
        terms: terms_from_wire(&w.terms)?,
        offer: party_from_wire(&w.offer)?,
        funding: w.funding.as_deref().map(hex::decode).transpose().map_err(err)?,
    })
}

pub fn accept_to_wire(a: &Accept) -> Result<WireAccept> {
    Ok(WireAccept {
        accept: party_to_wire(&a.accept)?,
        cet_adaptors: adaptors_to_wire(&a.cet_adaptors),
        refund_sig: hex::encode(a.refund_sig.serialize_compact()),
        funding: a.funding.as_ref().map(hex::encode),
    })
}

pub fn accept_from_wire(w: &WireAccept) -> Result<Accept> {
    Ok(Accept {
        accept: party_from_wire(&w.accept)?,
        cet_adaptors: adaptors_from_wire(&w.cet_adaptors)?,
        refund_sig: sig(&w.refund_sig)?,
        funding: w.funding.as_deref().map(hex::decode).transpose().map_err(err)?,
    })
}

pub fn sign_to_wire(s: &SignMsg) -> WireSign {
    WireSign {
        cet_adaptors: adaptors_to_wire(&s.cet_adaptors),
        refund_sig: hex::encode(s.refund_sig.serialize_compact()),
        funding_pczt: hex::encode(&s.funding_pczt),
        leg_sigs: s.leg_sigs.iter().map(|(i, g)| (*i, hex::encode(g.serialize_compact()))).collect(),
    }
}

pub fn sign_from_wire(w: &WireSign) -> Result<SignMsg> {
    Ok(SignMsg {
        cet_adaptors: adaptors_from_wire(&w.cet_adaptors)?,
        refund_sig: sig(&w.refund_sig)?,
        funding_pczt: hex::decode(&w.funding_pczt).map_err(err)?,
        leg_sigs: w.leg_sigs.iter().map(|(i, g)| Ok((*i, sig(g)?))).collect::<Result<_>>()?,
    })
}

// ---- Envelopes ----------------------------------------------------------------------

/// A signed message between two identities. `thread` names the negotiation (the quote).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Envelope {
    pub from: String,
    pub to: String,
    pub thread: String,
    pub kind: String,
    pub body: serde_json::Value,
    pub sig: String,
}

/// An envelope encrypted to `to`, as the desk stores it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SealedBox {
    pub to: String,
    pub epk: String,
    pub nonce: String,
    pub ct: String,
}

fn envelope_digest(from: &str, to: &str, thread: &str, kind: &str, body: &serde_json::Value) -> [u8; 32] {
    let mut h = Sha256::new();
    for part in [b"zdlc/envelope/v1" as &[u8], from.as_bytes(), to.as_bytes(), thread.as_bytes(), kind.as_bytes(), body.to_string().as_bytes()] {
        h.update((part.len() as u32).to_be_bytes());
        h.update(part);
    }
    h.finalize().into()
}

pub fn identity(sk: &SecretKey) -> String {
    hex::encode(PublicKey::from_secret_key(sk).serialize())
}

impl Envelope {
    pub fn new(from_sk: &SecretKey, to: &str, thread: &str, kind: &str, body: serde_json::Value) -> Self {
        let from = identity(from_sk);
        let d = envelope_digest(&from, to, thread, kind, &body);
        let sig = ecdsa::sign(Message::from_digest(d), from_sk);
        Self { from, to: to.into(), thread: thread.into(), kind: kind.into(), body, sig: hex::encode(sig.serialize_compact()) }
    }

    pub fn verify(&self) -> Result<()> {
        let pk = PublicKey::from_slice(&hex::decode(&self.from).map_err(err)?).map_err(err)?;
        let d = envelope_digest(&self.from, &self.to, &self.thread, &self.kind, &self.body);
        ecdsa::verify(&sig(&self.sig)?, Message::from_digest(d), &pk).map_err(|_| err("envelope signature"))
    }

    pub fn seal(&self) -> Result<SealedBox> {
        let to = PublicKey::from_slice(&hex::decode(&self.to).map_err(err)?).map_err(err)?;
        let mut e = [0u8; 32];
        let esk = loop {
            rand10::fill(&mut e);
            if let Ok(k) = SecretKey::from_secret_bytes(e) {
                break k;
            }
        };
        let epk = PublicKey::from_secret_key(&esk);
        let key = kdf(&SharedSecret::new(&to, &esk), &epk);
        let mut n = [0u8; 12];
        rand10::fill(&mut n);
        let ct = ChaCha20Poly1305::new((&key).into())
            .encrypt(Nonce::from_slice(&n), serde_json::to_vec(self).map_err(err)?.as_slice())
            .map_err(|_| err("encrypt"))?;
        Ok(SealedBox { to: self.to.clone(), epk: hex::encode(epk.serialize()), nonce: hex::encode(n), ct: hex::encode(ct) })
    }
}

fn kdf(shared: &SharedSecret, epk: &PublicKey) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"zdlc/mailbox/v1");
    h.update(shared.to_secret_bytes());
    h.update(epk.serialize());
    h.finalize().into()
}

impl SealedBox {
    /// Decrypts with the recipient's identity key and checks the sender's signature.
    pub fn open(&self, sk: &SecretKey) -> Result<Envelope> {
        let epk = PublicKey::from_slice(&hex::decode(&self.epk).map_err(err)?).map_err(err)?;
        let key = kdf(&SharedSecret::new(&epk, sk), &epk);
        let pt = ChaCha20Poly1305::new((&key).into())
            .decrypt(Nonce::from_slice(&unhex::<12>(&self.nonce)?), hex::decode(&self.ct).map_err(err)?.as_slice())
            .map_err(|_| err("not for this key, or tampered"))?;
        let env: Envelope = serde_json::from_slice(&pt).map_err(err)?;
        if env.to != identity(sk) {
            return Err(err("envelope addressed to someone else"));
        }
        env.verify()?;
        Ok(env)
    }
}
