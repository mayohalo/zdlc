//! The zDLC developer kit: one [`Zdlc`] object that runs contracts end to end.
//!
//! An app configures where messages go ([`Transport`]), where contracts live
//! ([`Storage`]), which oracles attest ([`Oracle`]) and how to reach the chain
//! ([`Chain`]); describes a contract with a [`Template`]; and reacts to [`Event`]s. The
//! kit runs setup, funding (treasury coins or a shielded deposit paid from any wallet),
//! the cooperative close, the CET and the refund. Contract state is persisted after
//! every step, so a restart resumes where it stopped.
//!
//! ```no_run
//! # async fn demo() -> zdlc_kit::Result<()> {
//! use std::sync::Arc;
//! use zdlc_kit::{impls::*, templates::Template, Funding, Zdlc};
//! let seed = [7u8; 32];
//! let wallet = zdlc::wallet::Wallet::from_seed(seed);
//! let z = Zdlc::builder()
//!     .seed(seed)
//!     .chain(Arc::new(ZakuraRpc::new("http://127.0.0.1:18252/")))
//!     .storage(Arc::new(SqliteStorage::open("zdlc.db")?))
//!     .oracle(Arc::new(HttpOracle::new("http://127.0.0.1:7801")))
//!     .transport(Arc::new(DeskTransport::new("http://127.0.0.1:7900", wallet.identity_key())))
//!     .build()
//!     .await?;
//! let escrow = Template::Escrow { amount_zat: 150_000_000, question: "Milestone 1 delivered?".into(), payees: vec![], maturity_blocks: 20, refund_blocks: 2000 };
//! let id = z.offer(escrow, "02ab…client identity").await?;
//! // on the client's node: z.accept(&id, Funding::Deposit).await?
//! # Ok(()) }
//! ```

pub mod agent;
pub mod api;
pub mod impls;
pub mod templates;
pub mod traits;

use std::{collections::HashMap, sync::Arc};

use secp256k1::SecretKey;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use zcash_protocol::{consensus::NetworkType, local_consensus::LocalNetwork};
use zdlc::{
    contract::{self, Contract, Event as CEvent, IronwoodFunds, Party, PartyInfo, SIGN_SLACK},
    enclave::AnnouncementMsg,
    funds, oracle_client, paylink, script,
    tx::{self, Coin, Pay, Spec, Spend},
    txid_from_hex, txid_hex,
    wallet::Wallet,
    wire::{self, WireAccept, WireCoin, WireOffer, WireSign},
};

pub use templates::Template;
pub use traits::{Chain, Oracle, Storage, Transport};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Zdlc(#[from] zdlc::Error),
    #[error("storage: {0}")]
    Storage(String),
    #[error("chain: {0}")]
    Chain(String),
    #[error("template: {0}")]
    Template(String),
    #[error("{0}")]
    Usage(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Value the offerer's or a treasury-funding accepter's inputs cover beyond collateral:
/// its share of the reserve and the funding fee.
pub const CUSHION: u64 = zdlc::trader::CUSHION;
/// Blocks after maturity (or after asking) a side waits for a cooperative close before
/// settling by CET.
pub const CLOSE_WAIT: u32 = 6;
/// ...and at least this many seconds after it first saw the attestation, so a fast
/// chain (regtest) does not outrun the counterparty.
pub const CLOSE_WAIT_SECS: i64 = 60;

pub use zdlc::templates::Role;

/// How a side funds its collateral.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Funding {
    /// Transparent coins at the node's treasury address.
    Treasury,
    /// A shielded deposit to the contract's own receiver, paid from any wallet by a
    /// ZIP 321 link (accepter only).
    Deposit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Sent, waiting for the accepter.
    Offered,
    /// Received, waiting for this node's decision.
    Incoming,
    /// Accepted with a deposit: waiting for the payment.
    AwaitingDeposit,
    /// Asked the offerer for fresh heights (the offer went stale while waiting).
    Refreshing,
    Accepted,
    /// Every signature exchanged; the funding transaction is out.
    Signed,
    Funded,
    /// Asked the counterparty to co-sign the cooperative close.
    Closing,
    Settled,
    Declined,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Deposit {
    pub uri: String,
    pub zat: u64,
    /// Height the link was made: notes from here on count.
    pub from: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Settlement {
    /// "close", "cet", "refund", or "counterparty" (spent by a transaction this node did not send).
    pub how: String,
    pub txid: Option<String>,
    pub height: Option<u32>,
}

/// One contract as this node sees it. Persisted after every step.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub role: Role,
    /// The wallet's contract index: which keys.
    pub index: u32,
    pub counterparty: String,
    pub template: Template,
    pub state: State,
    pub event_id: String,
    pub created_height: u32,
    #[serde(default)]
    pub funding: Option<Funding>,
    /// This side's treasury inputs.
    #[serde(default)]
    pub inputs: Vec<WireCoin>,
    #[serde(default)]
    pub deposit: Option<Deposit>,
    #[serde(default)]
    pub offer: Option<WireOffer>,
    #[serde(default)]
    pub accept: Option<WireAccept>,
    #[serde(default)]
    pub sign: Option<WireSign>,
    /// A deposit-funded side's own funding PCZT, hex. Secret: note witnesses and the
    /// binding key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub own_funding: Option<String>,
    #[serde(default)]
    pub funding_txid: Option<String>,
    #[serde(default)]
    pub maturity: Option<u32>,
    #[serde(default)]
    pub refund_height: Option<u32>,
    #[serde(default)]
    pub attested: Option<String>,
    /// Unix time this node first saw the attestation.
    #[serde(default)]
    pub attested_at: Option<i64>,
    /// Tip the close was built at (offerer), when asked.
    #[serde(default)]
    pub close_tip: Option<u32>,
    /// What this node broadcast to settle, before it is seen spent.
    #[serde(default)]
    pub pending: Option<Settlement>,
    #[serde(default)]
    pub settlement: Option<Settlement>,
    #[serde(default)]
    pub shielded_payout: Option<String>,
    /// The recovery record this node posted (a memo to its own close address).
    #[serde(default)]
    pub record_txid: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

impl Record {
    /// What an app shows: the record without secrets or wire blobs.
    pub fn view(&self) -> Value {
        json!({
            "id": self.id, "role": self.role, "state": self.state, "counterparty": self.counterparty,
            "template": self.template, "summary": self.template.summary(self.role), "eventId": self.event_id,
            "funding": self.funding, "deposit": self.deposit, "fundingTxid": self.funding_txid,
            "maturity": self.maturity, "refundHeight": self.refund_height, "attested": self.attested,
            "settlement": self.settlement, "shieldedPayout": self.shielded_payout, "recordTxid": self.record_txid, "note": self.note,
        })
    }
}

/// What happened, in order. Apps read these from [`Zdlc::events_after`].
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Event {
    Offered { id: String, incoming: bool, counterparty: String, summary: Value },
    AwaitingDeposit { id: String, uri: String, zat: u64 },
    Accepted { id: String },
    Signed { id: String },
    Funded { id: String, txid: String, height: u32 },
    Attested { id: String, outcome: String },
    Settled { id: String, how: String, txid: Option<String>, height: Option<u32> },
    PayoutShielded { id: String, txid: String },
    /// Something went wrong on a funded contract; the kit keeps going (the CET and the
    /// refund still protect it).
    Warning { id: String, message: String },
    Declined { id: String },
    Failed { id: String, reason: String },
}

pub struct Builder {
    seed: Option<[u8; 32]>,
    transport: Option<Arc<dyn Transport>>,
    storage: Option<Arc<dyn Storage>>,
    oracles: Vec<Arc<dyn Oracle>>,
    chain: Option<Arc<dyn Chain>>,
    auto_shield: bool,
    records: bool,
    block_secs: i64,
}

impl Builder {
    pub fn seed(mut self, seed: [u8; 32]) -> Self {
        self.seed = Some(seed);
        self
    }
    pub fn transport(mut self, t: Arc<dyn Transport>) -> Self {
        self.transport = Some(t);
        self
    }
    pub fn storage(mut self, s: Arc<dyn Storage>) -> Self {
        self.storage = Some(s);
        self
    }
    /// Adds an oracle; a contract settles when every configured oracle agrees.
    pub fn oracle(mut self, o: Arc<dyn Oracle>) -> Self {
        self.oracles.push(o);
        self
    }
    pub fn chain(mut self, c: Arc<dyn Chain>) -> Self {
        self.chain = Some(c);
        self
    }
    /// Move CET and refund payouts into the shielded balance as they land (default on).
    pub fn auto_shield(mut self, on: bool) -> Self {
        self.auto_shield = on;
        self
    }
    /// Seconds a block takes (75 on Zcash): where templates put the price minute.
    pub fn block_secs(mut self, s: i64) -> Self {
        self.block_secs = s;
        self
    }
    /// Post a recovery record for every signed contract, paid from the treasury
    /// (default on): with it, the seed alone recovers the refund ([`Zdlc::recover`]).
    pub fn recovery_records(mut self, on: bool) -> Self {
        self.records = on;
        self
    }

    pub async fn build(self) -> Result<Zdlc> {
        let need = |what: &str| Error::Usage(format!("builder: {what} not set"));
        let chain = self.chain.ok_or(need("chain"))?;
        let (p, net) = chain.network().await?;
        if self.oracles.is_empty() {
            return Err(need("oracle"));
        }
        let seed = self.seed.ok_or(need("seed"))?;
        Ok(Zdlc {
            transport: self.transport.ok_or(need("transport"))?,
            storage: self.storage.ok_or(need("storage"))?,
            oracles: self.oracles,
            chain,
            wallet: Wallet::from_seed(seed),
            p,
            net,
            auto_shield: self.auto_shield,
            records: self.records,
            block_secs: self.block_secs,
            live: tokio::sync::Mutex::new(HashMap::new()),
            step: tokio::sync::Mutex::new(()),
            policy: std::sync::Mutex::new(None),
        })
    }
}

/// A balance across the treasury and every contract's shielded receivers.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Balance {
    pub treasury_zat: u64,
    pub shielded_zat: u64,
    pub shielded_notes: usize,
    pub in_contracts_zat: u64,
}

pub struct Zdlc {
    transport: Arc<dyn Transport>,
    storage: Arc<dyn Storage>,
    oracles: Vec<Arc<dyn Oracle>>,
    chain: Arc<dyn Chain>,
    wallet: Wallet,
    p: LocalNetwork,
    net: NetworkType,
    auto_shield: bool,
    records: bool,
    block_secs: i64,
    live: tokio::sync::Mutex<HashMap<String, Contract>>,
    step: tokio::sync::Mutex<()>,
    policy: std::sync::Mutex<Option<Policy>>,
}

/// Decides incoming offers without a person: `Some(funding)` accepts, `None` leaves the
/// offer for [`Zdlc::accept`] or [`Zdlc::decline`].
pub type Policy = Arc<dyn Fn(&Record) -> Option<Funding> + Send + Sync>;

fn now_unix() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

fn coin_from_wire(c: &WireCoin) -> Result<Coin> {
    let pk = secp256k1::PublicKey::from_slice(&hex::decode(&c.pubkey).map_err(|e| Error::Usage(e.to_string()))?)
        .map_err(|e| Error::Usage(e.to_string()))?;
    Ok(Coin { txid: txid_from_hex(&c.txid)?, index: c.index, value: c.value, spend: Spend::P2pkh(pk) })
}

fn pczt_hex(p: &pczt::Pczt) -> Result<String> {
    Ok(hex::encode(p.clone().serialize().map_err(|e| zdlc::Error::Pczt(format!("{e:?}")))?))
}

fn pczt_parse(h: &str) -> Result<pczt::Pczt> {
    let b = hex::decode(h).map_err(|e| Error::Usage(e.to_string()))?;
    Ok(pczt::Pczt::parse(&b).map_err(|e| zdlc::Error::Pczt(format!("{e:?}")))?)
}

fn from_value<T: serde::de::DeserializeOwned>(v: &Value) -> Result<T> {
    serde_json::from_value(v.clone()).map_err(|e| Error::Usage(format!("message: {e}")))
}

/// The secret an enumerated attestation reveals (`oracle_client::verify_outcome`).
pub type OutcomeScalar = secp256k1_zkp::SecretKey;

/// The settled outcome as attested: a numeric value or an outcome name, with what the
/// CET and close need.
enum Attested {
    Numeric(Vec<zdlc::oracle::NumericAttestation>),
    Outcome(String, Vec<OutcomeScalar>),
}

impl Zdlc {
    pub fn builder() -> Builder {
        Builder { seed: None, transport: None, storage: None, oracles: vec![], chain: None, auto_shield: true, records: true, block_secs: 75 }
    }

    pub fn identity(&self) -> String {
        self.transport.identity()
    }

    /// Sets the auto-accept policy (an agent's pricing, an app's whitelist).
    pub fn set_policy(&self, p: Policy) {
        *self.policy.lock().unwrap() = Some(p);
    }

    /// What the policy says about a new incoming offer. Later periods of a payroll this
    /// node already accepted are accepted the same way.
    async fn decide(&self, rec: &Record) -> Result<Option<Funding>> {
        if let Template::Payroll { period, series_id: Some(sid), .. } = &rec.template {
            if *period > 1 {
                if let Some(first) = self.storage.get(sid).await? {
                    if first.role == Role::Accepter && first.counterparty == rec.counterparty && first.funding.is_some() && first.state != State::Declined {
                        return Ok(first.funding);
                    }
                }
            }
        }
        let p = self.policy.lock().unwrap().clone();
        Ok(p.and_then(|p| p(rec)))
    }

    pub fn network(&self) -> NetworkType {
        self.net
    }

    /// Where to send transparent coins for treasury funding.
    pub fn treasury_address(&self) -> String {
        zdlc::keys::encode(&self.wallet.coin_address(0), self.net)
    }

    pub async fn info(&self) -> Result<Value> {
        let mut oracles = vec![];
        for o in &self.oracles {
            oracles.push(json!({"url": o.name(), "pubkey": o.pubkey().await.unwrap_or_default()}));
        }
        Ok(json!({
            "identity": self.identity(), "network": format!("{:?}", self.net), "tip": self.chain.height().await?,
            "treasury": self.treasury_address(), "oracles": oracles,
            "transport": self.transport.name(), "storage": self.storage.name(), "chain": self.chain.name(),
        }))
    }

    pub async fn contracts(&self) -> Result<Vec<Record>> {
        self.storage.list().await
    }

    pub async fn contract(&self, id: &str) -> Result<Option<Record>> {
        self.storage.get(id).await
    }

    pub async fn events_after(&self, seq: u64, limit: usize) -> Result<Vec<(u64, Event)>> {
        self.storage.events_after(seq, limit).await
    }

    async fn emit(&self, ev: Event) -> Result<()> {
        self.storage.push_event(&ev).await?;
        Ok(())
    }

    async fn next_index(&self) -> Result<u32> {
        let i = self.storage.get_kv("next_index").await?.and_then(|v| v.as_u64()).unwrap_or(1) as u32;
        self.storage.put_kv("next_index", &json!(i + 1)).await?;
        Ok(i)
    }

    async fn reserved(&self) -> Result<Vec<(String, u32)>> {
        Ok(self.storage.get_kv("reserved").await?.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default())
    }

    /// Unreserved treasury coins covering `need`, largest first; reserves them.
    async fn select(&self, need: u64) -> Result<Vec<Coin>> {
        let mut reserved = self.reserved().await?;
        let mut utxos = self.chain.utxos(&self.wallet.coin_address(0), self.net).await?;
        utxos.sort_by_key(|u| std::cmp::Reverse(u.2));
        let pk = zdlc::keys::pubkey(&self.wallet.coin_key(0));
        let (mut out, mut got) = (vec![], 0);
        for (txid, index, value, h) in utxos {
            if h == 0 || reserved.contains(&(txid_hex(&txid), index)) {
                continue;
            }
            out.push(Coin { txid, index, value, spend: Spend::P2pkh(pk) });
            got += value;
            if got >= need {
                reserved.extend(out.iter().map(|c| (txid_hex(&c.txid), c.index)));
                self.storage.put_kv("reserved", &json!(reserved)).await?;
                return Ok(out);
            }
        }
        Err(Error::Usage(format!("treasury {} holds {got} zat, short of {need}", self.treasury_address())))
    }

    async fn release(&self, coins: &[WireCoin]) -> Result<()> {
        let mut reserved = self.reserved().await?;
        reserved.retain(|r| !coins.iter().any(|c| c.txid == r.0 && c.index == r.1));
        self.storage.put_kv("reserved", &json!(reserved)).await
    }

    /// This node's side of contract `i`, funding from treasury `inputs`.
    fn party(&self, i: u32, inputs: Vec<Coin>) -> Party {
        let w = &self.wallet;
        Party {
            funding_sk: w.funding_key(i),
            input_keys: vec![w.coin_key(0); inputs.len()],
            close_fvk: w.close_fvk(i),
            roll_key: None,
            ironwood: None,
            info: PartyInfo {
                funding_pubkey: zdlc::keys::pubkey(&w.funding_key(i)),
                inputs,
                change: w.coin_address(0),
                payout: w.payout_address(i),
                close: w.close_address(i),
                ironwood: false,
            },
        }
    }

    fn party_of(&self, rec: &Record) -> Result<Party> {
        let mut me = self.party(rec.index, rec.inputs.iter().map(coin_from_wire).collect::<Result<_>>()?);
        if rec.funding == Some(Funding::Deposit) {
            me.info.ironwood = true;
            me.input_keys.clear();
        }
        Ok(me)
    }

    async fn announcements(&self, event_id: &str) -> Result<Vec<AnnouncementMsg>> {
        let mut out = vec![];
        for o in &self.oracles {
            let a = o.announcement(event_id).await?;
            if a.pubkey != o.pubkey().await? {
                return Err(Error::Usage(format!("{}: announcement signed by another key", o.name())));
            }
            out.push(a);
        }
        Ok(out)
    }

    // ---- Offer --------------------------------------------------------------------------

    /// Offers `template` to `counterparty` (its identity). Returns the contract id.
    pub async fn offer(&self, mut template: Template, counterparty: &str) -> Result<String> {
        let tip0 = self.chain.height().await?;
        template.fill(now_unix(), tip0, self.block_secs);
        let mut nonce = [0u8; 8];
        rand10::fill(&mut nonce);
        let id = hex::encode(nonce);
        let event_id = format!("zdlc-{id}");
        let index = self.next_index().await?;
        match &mut template {
            // The lender is repaid at this contract's own shielded address; the oracles
            // watch it through its viewing key.
            Template::PayOnPayment { repay_to, ivk, .. } if repay_to.is_none() => {
                let fvk = self.wallet.close_fvk(index);
                *repay_to = Some(paylink::unified_address(&self.wallet.close_address(index), self.net));
                *ivk = Some(hex::encode(fvk.to_ivk(orchard::keys::Scope::External).to_bytes()));
            }
            Template::Payroll { series_id, .. } if series_id.is_none() => *series_id = Some(id.clone()),
            _ => {}
        }
        template.validate()?;
        let spec = template.event()?;
        for o in &self.oracles {
            o.announce(&event_id, &spec).await?;
        }
        let mine = template.collateral(Role::Offerer);
        let inputs = if mine > 0 { self.select(mine + CUSHION).await? } else { vec![] };
        let tip = self.chain.height().await?;
        let mut rec = Record {
            id: id.clone(),
            role: Role::Offerer,
            index,
            counterparty: counterparty.into(),
            template,
            state: State::Offered,
            event_id,
            created_height: tip,
            funding: Some(Funding::Treasury),
            inputs: inputs.iter().map(zdlc::trader::coin_to_wire).collect(),
            deposit: None,
            offer: None,
            accept: None,
            sign: None,
            own_funding: None,
            funding_txid: None,
            maturity: None,
            refund_height: None,
            attested: None,
            attested_at: None,
            close_tip: None,
            pending: None,
            settlement: None,
            shielded_payout: None,
            record_txid: None,
            note: None,
        };
        self.send_offer(&mut rec).await?;
        self.emit(Event::Offered { id: id.clone(), incoming: false, counterparty: counterparty.into(), summary: rec.template.summary(Role::Offerer) })
            .await?;
        Ok(id)
    }

    /// Builds the offer at the current tip and sends it (first time, or refreshed).
    async fn send_offer(&self, rec: &mut Record) -> Result<()> {
        let tip = self.chain.height().await?;
        let anns = self.announcements(&rec.event_id).await?;
        let terms = rec.template.terms(&anns, tip + 1)?;
        terms.validate(&self.p, tip)?;
        let offer = self.party_of(rec)?.offer(terms.clone());
        let w = wire::offer_to_wire(&offer)?;
        rec.maturity = Some(terms.maturity);
        rec.refund_height = Some(terms.refund_height);
        rec.offer = Some(w.clone());
        rec.state = State::Offered;
        self.storage.put(rec).await?;
        self.transport
            .send(&rec.counterparty, &rec.id, "offer", json!({"template": rec.template, "event_id": rec.event_id, "offer": w}))
            .await
    }

    /// Checks an incoming offer: the event is this thread's, the announcements are our
    /// own oracles', and the terms are exactly what the template gives.
    async fn check_offer(&self, thread: &str, body: &Value) -> Result<(Template, WireOffer, String)> {
        let template: Template = from_value(&body["template"])?;
        let w: WireOffer = from_value(&body["offer"])?;
        let event_id = body["event_id"].as_str().unwrap_or_default().to_string();
        if event_id != format!("zdlc-{thread}") {
            return Err(Error::Usage("the offer's event is not its thread's".into()));
        }
        let anns = self.announcements(&event_id).await?;
        let want = template.terms(&anns, w.terms.sign_height)?;
        if serde_json::to_value(wire::terms_to_wire(&want)).ok() != serde_json::to_value(&w.terms).ok() {
            return Err(Error::Usage("the offer's terms are not what its template gives".into()));
        }
        Ok((template, w, event_id))
    }

    async fn on_offer(&self, from: &str, thread: &str, body: &Value) -> Result<()> {
        let (template, w, event_id) = self.check_offer(thread, body).await?;
        match self.storage.get(thread).await? {
            None => {
                let index = self.next_index().await?;
                let tip = self.chain.height().await?;
                let rec = Record {
                    id: thread.into(),
                    role: Role::Accepter,
                    index,
                    counterparty: from.into(),
                    template: template.clone(),
                    state: State::Incoming,
                    event_id,
                    created_height: tip,
                    funding: None,
                    inputs: vec![],
                    deposit: None,
                    maturity: Some(w.terms.maturity),
                    refund_height: Some(w.terms.refund_height),
                    offer: Some(w),
                    accept: None,
                    sign: None,
                    own_funding: None,
                    funding_txid: None,
                    attested: None,
                    attested_at: None,
                    close_tip: None,
                    pending: None,
                    settlement: None,
                    shielded_payout: None,
                    record_txid: None,
                    note: None,
                };
                self.storage.put(&rec).await?;
                self.emit(Event::Offered { id: thread.into(), incoming: true, counterparty: from.into(), summary: template.summary(Role::Accepter) })
                    .await?;
                match self.decide(&rec).await? {
                    Some(f) => self.accept_inner(rec, f).await,
                    None => Ok(()),
                }
            }
            // A refreshed offer (fresh heights) for a contract we are setting up.
            Some(mut rec) if rec.role == Role::Accepter && rec.counterparty == from && rec.template == template => {
                rec.maturity = Some(w.terms.maturity);
                rec.refund_height = Some(w.terms.refund_height);
                rec.offer = Some(w);
                match rec.state {
                    State::Refreshing | State::Accepted => {
                        rec.state = State::Accepted;
                        self.storage.put(&rec).await?;
                        self.do_accept(rec).await
                    }
                    _ => self.storage.put(&rec).await,
                }
            }
            Some(_) => Err(Error::Usage("an offer for a thread this node did not expect".into())),
        }
    }

    // ---- Accept ---------------------------------------------------------------------------

    /// Accepts an incoming offer, funding this side's collateral from the treasury or
    /// from a deposit link (any wallet pays it; see [`Event::AwaitingDeposit`]).
    pub async fn accept(&self, id: &str, funding: Funding) -> Result<()> {
        let _g = self.step.lock().await;
        let rec = self.storage.get(id).await?.ok_or(Error::Usage(format!("no contract {id}")))?;
        self.accept_inner(rec, funding).await
    }

    async fn accept_inner(&self, mut rec: Record, funding: Funding) -> Result<()> {
        let id = rec.id.clone();
        let id = id.as_str();
        if rec.state != State::Incoming {
            return Err(Error::Usage(format!("contract {id} is {:?}, not incoming", rec.state)));
        }
        rec.funding = Some(funding);
        let mine = rec.template.collateral(Role::Accepter);
        match funding {
            Funding::Treasury => {
                if mine > 0 {
                    rec.inputs = self.select(mine + CUSHION).await?.iter().map(zdlc::trader::coin_to_wire).collect();
                }
                rec.state = State::Accepted;
                self.storage.put(&rec).await?;
                self.do_accept(rec).await
            }
            Funding::Deposit => {
                if mine == 0 {
                    return Err(Error::Usage("this side puts nothing in: accept with treasury".into()));
                }
                let need = self.deposit_need(&rec, 1).await?;
                let tip = self.chain.height().await?;
                let uri = paylink::deposit_uri(&self.wallet.deposit_address(rec.index), self.net, need, &rec.id);
                rec.deposit = Some(Deposit { uri: uri.clone(), zat: need, from: tip + 1 });
                rec.state = State::AwaitingDeposit;
                self.storage.put(&rec).await?;
                self.emit(Event::AwaitingDeposit { id: id.into(), uri, zat: need }).await
            }
        }
    }

    pub async fn decline(&self, id: &str) -> Result<()> {
        let mut rec = self.storage.get(id).await?.ok_or(Error::Usage(format!("no contract {id}")))?;
        if !matches!(rec.state, State::Incoming | State::AwaitingDeposit) {
            return Err(Error::Usage(format!("contract {id} is {:?}", rec.state)));
        }
        rec.state = State::Declined;
        self.storage.put(&rec).await?;
        self.transport.send(&rec.counterparty, id, "decline", json!({})).await?;
        self.emit(Event::Declined { id: id.into() }).await
    }

    async fn deposit_need(&self, rec: &Record, notes: usize) -> Result<u64> {
        let offer = wire::offer_from_wire(rec.offer.as_ref().ok_or(Error::Usage("no offer".into()))?)?;
        let mut me = self.party(rec.index, vec![]);
        me.info.ironwood = true;
        Ok(contract::ironwood_deposit(&self.p, &offer.terms, &offer.offer, &me.info, notes)?)
    }

    /// Accepts with the chosen funding; if the offer went stale, asks for a fresh one.
    async fn do_accept(&self, mut rec: Record) -> Result<()> {
        let offer = wire::offer_from_wire(rec.offer.as_ref().ok_or(Error::Usage("no offer".into()))?)?;
        let tip = self.chain.height().await?;
        if offer.terms.sign_height + SIGN_SLACK < tip + 1 {
            rec.state = State::Refreshing;
            self.storage.put(&rec).await?;
            return self.transport.send(&rec.counterparty, &rec.id, "refresh", json!({})).await;
        }
        let mut me = self.party_of(&rec)?;
        if rec.funding == Some(Funding::Deposit) {
            let d = rec.deposit.clone().ok_or(Error::Usage("no deposit".into()))?;
            let fvk = self.wallet.deposit_fvk(rec.index);
            let notes = self.chain.notes(&fvk, d.from, tip).await?;
            let (spends, anchor) = self.chain.spends(&fvk, &notes, tip).await?;
            me.ironwood = Some(IronwoodFunds { sk: self.wallet.deposit_sk(rec.index), spends, anchor, change: self.wallet.deposit_address(rec.index) });
        }
        let (accept, txs) = me.accept(&self.p, tip, &offer)?;
        if let Some(full) = txs.own_funding() {
            rec.own_funding = Some(pczt_hex(full)?);
        }
        let w = wire::accept_to_wire(&accept)?;
        rec.accept = Some(w.clone());
        rec.funding_txid = Some(txid_hex(&txs.funding_txid));
        rec.state = State::Accepted;
        self.storage.put(&rec).await?;
        self.transport.send(&rec.counterparty, &rec.id, "accept", json!({"accept": w})).await?;
        self.emit(Event::Accepted { id: rec.id.clone() }).await
    }

    /// Watches a deposit link; accepts once it is paid, or shows a top-up link.
    async fn poll_deposit(&self, mut rec: Record) -> Result<()> {
        let d = rec.deposit.clone().ok_or(Error::Usage("no deposit".into()))?;
        let tip = self.chain.height().await?;
        let notes = self.chain.notes(&self.wallet.deposit_fvk(rec.index), d.from, tip).await?;
        let got: u64 = notes.iter().map(|n| n.value()).sum();
        let need = self.deposit_need(&rec, notes.len().max(1)).await?;
        if got >= need {
            rec.state = State::Accepted;
            self.storage.put(&rec).await?;
            return self.do_accept(rec).await;
        }
        if !notes.is_empty() {
            // Underpaid: a further note costs one more Ironwood action.
            let ask = self.deposit_need(&rec, notes.len() + 1).await? - got;
            if ask != d.zat {
                let uri = paylink::deposit_uri(&self.wallet.deposit_address(rec.index), self.net, ask, &rec.id);
                rec.deposit = Some(Deposit { uri: uri.clone(), zat: ask, from: d.from });
                self.storage.put(&rec).await?;
                self.emit(Event::AwaitingDeposit { id: rec.id.clone(), uri, zat: ask }).await?;
            }
        }
        Ok(())
    }

    // ---- Sign and finish ----------------------------------------------------------------

    async fn on_accept(&self, mut rec: Record, body: &Value) -> Result<()> {
        let accept = wire::accept_from_wire(&from_value(&body["accept"])?)?;
        let offer = wire::offer_from_wire(rec.offer.as_ref().unwrap())?;
        let tip = self.chain.height().await?;
        let me = self.party_of(&rec)?;
        let (sign, c) = match me.sign(&self.p, tip, &offer, &accept) {
            Ok(x) => x,
            Err(e) if e.to_string().contains("sign height") => {
                // The accept came too late for these heights: offer again.
                return self.send_offer(&mut rec).await;
            }
            Err(e) => return Err(e.into()),
        };
        let w = wire::sign_to_wire(&sign);
        rec.accept = Some(from_value(&body["accept"])?);
        rec.sign = Some(w.clone());
        rec.funding_txid = Some(txid_hex(&c.txs.funding_txid));
        rec.state = State::Signed;
        self.storage.put(&rec).await?;
        self.transport.send(&rec.counterparty, &rec.id, "sign", json!({"sign": w})).await?;
        self.post_record(&mut rec, &c).await;
        self.live.lock().await.insert(rec.id.clone(), c);
        self.emit(Event::Signed { id: rec.id.clone() }).await
    }

    /// The recovery record: everything the seed cannot give, as a memo to this
    /// contract's own close address. Paid from the treasury; without a coin there, a
    /// warning (the database is then the only backup).
    async fn post_record(&self, rec: &mut Record, c: &Contract) {
        if !self.records {
            return;
        }
        let r = async {
            let record = zdlc::recovery::Record::from_contract(c, rec.index)?;
            let coins = self.select(zdlc::recovery::RECORD_VALUE + 20_000).await?;
            let tip = self.chain.height().await?;
            let f = zdlc::recovery::memo_tx_from(
                &self.p,
                tip,
                self.wallet.close_address(rec.index),
                record.encode(),
                coins[0].clone(),
                &self.wallet.coin_key(0),
                self.wallet.coin_address(0),
                self.chain.ironwood_anchor(tip).await?,
            )?;
            let sent = self.chain.send(&f.raw).await;
            self.release(&coins.iter().map(zdlc::trader::coin_to_wire).collect::<Vec<_>>()).await?;
            sent?;
            Ok::<_, Error>(txid_hex(&f.txid))
        }
        .await;
        match r {
            Ok(t) => {
                rec.record_txid = Some(t);
                rec.note = None;
                let _ = self.storage.put(rec).await;
            }
            Err(e) => {
                let _ = self.storage.put(rec).await;
                let _ = self.note(&rec.id, &format!("recovery record not posted: {e}")).await;
            }
        }
    }

    async fn retry_record(&self, rec: &Record) {
        if self.contract_of(rec).await.is_err() {
            return;
        }
        let Some(mut rec) = self.storage.get(&rec.id).await.ok().flatten() else { return };
        let live = self.live.lock().await;
        if let Some(c) = live.get(&rec.id) {
            self.post_record(&mut rec, c).await;
        }
    }

    /// Seed-only recovery: finds this wallet's recovery records (contract indices
    /// `0..gap`, from height `from`) and broadcasts every refund that is due on a
    /// contract still unspent. Returns what it found.
    pub async fn recover(&self, from: u32, gap: u32) -> Result<Vec<Value>> {
        let tip = self.chain.height().await?;
        let mut out = vec![];
        let mut top = 0;
        for i in 0..gap {
            for n in self.chain.notes(&self.wallet.close_fvk(i), from, tip).await? {
                let Some(r) = zdlc::recovery::Record::decode(&n.memo).filter(|r| r.index == i) else { continue };
                top = top.max(i + 1);
                let own = zdlc::keys::pubkey(&self.wallet.funding_key(i));
                let redeem = if r.is_offerer { script::multisig_2of2(&own, &r.counterparty_pubkey)? } else { script::multisig_2of2(&r.counterparty_pubkey, &own)? };
                let unspent = self.chain.utxos(&script::p2sh(&redeem), self.net).await?.iter().any(|u| u.0 == r.funding_txid && u.1 == r.funding_index);
                let status = if !unspent {
                    "settled".to_string()
                } else if tip < r.refund_height {
                    format!("refund due at {}", r.refund_height)
                } else {
                    let f = zdlc::recovery::rebuild_refund(&self.p, &self.wallet, &r)?;
                    match self.chain.send(&f.raw).await {
                        Ok(()) => format!("refund sent {}", txid_hex(&f.txid)),
                        Err(e) => format!("refund refused: {e}"),
                    }
                };
                out.push(json!({"index": i, "fundingTxid": txid_hex(&r.funding_txid), "refundHeight": r.refund_height,
                    "collateral": if r.is_offerer { r.offer_collateral } else { r.accept_collateral }, "status": status}));
            }
        }
        // A restored wallet must not reuse a contract index.
        let next = self.storage.get_kv("next_index").await?.and_then(|v| v.as_u64()).unwrap_or(1) as u32;
        if top + 1 > next {
            self.storage.put_kv("next_index", &json!(top + 1)).await?;
        }
        Ok(out)
    }

    async fn on_sign(&self, mut rec: Record, body: &Value) -> Result<()> {
        let sign = wire::sign_from_wire(&from_value(&body["sign"])?)?;
        let offer = wire::offer_from_wire(rec.offer.as_ref().unwrap())?;
        let accept = wire::accept_from_wire(rec.accept.as_ref().unwrap())?;
        let given = accept.funding.as_ref().or(offer.funding.as_ref()).map(|b| pczt::Pczt::parse(b)).transpose().map_err(|e| zdlc::Error::Pczt(format!("{e:?}")))?;
        let mut txs = contract::build_txs_with(&self.p, &offer.terms, &offer.offer, &accept.accept, given.as_ref())?;
        if let Some(h) = &rec.own_funding {
            txs = txs.with_own_funding(pczt_parse(h)?)?;
        }
        let me = self.party_of(&rec)?;
        let (funding, c) = me.finish(&offer, txs, &sign)?;
        self.chain.send(&funding.raw).await?;
        rec.sign = Some(from_value(&body["sign"])?);
        rec.funding_txid = Some(txid_hex(&funding.txid));
        rec.own_funding = None;
        rec.state = State::Signed;
        self.storage.put(&rec).await?;
        self.post_record(&mut rec, &c).await;
        self.live.lock().await.insert(rec.id.clone(), c);
        self.emit(Event::Signed { id: rec.id.clone() }).await
    }

    /// The live contract, rebuilt from the stored messages after a restart.
    async fn contract_of(&self, rec: &Record) -> Result<()> {
        let mut live = self.live.lock().await;
        if live.contains_key(&rec.id) {
            return Ok(());
        }
        let offer = wire::offer_from_wire(rec.offer.as_ref().ok_or(Error::Usage("no offer".into()))?)?;
        let accept = wire::accept_from_wire(rec.accept.as_ref().ok_or(Error::Usage("no accept".into()))?)?;
        let sign = wire::sign_from_wire(rec.sign.as_ref().ok_or(Error::Usage("no sign".into()))?)?;
        let c = self.party_of(rec)?.restore(&self.p, rec.role == Role::Offerer, &offer, &accept, &sign)?;
        live.insert(rec.id.clone(), c);
        Ok(())
    }

    // ---- Settlement ---------------------------------------------------------------------

    async fn attested(&self, rec: &Record, c: &Contract) -> Result<Option<Attested>> {
        let mut atts = vec![];
        for o in &self.oracles {
            match o.attestation(&rec.event_id).await? {
                Some(a) => atts.push(a),
                None => return Ok(None),
            }
        }
        Ok(Some(match &c.terms.event {
            CEvent::Numeric(anns) => Attested::Numeric(
                anns.iter().zip(&atts).map(|(a, t)| oracle_client::verify_numeric(a, t)).collect::<zdlc::Result<_>>()?,
            ),
            CEvent::Enumerated(anns) => {
                let v: Vec<(String, OutcomeScalar)> = anns.iter().zip(&atts).map(|(a, t)| oracle_client::verify_outcome(a, t)).collect::<zdlc::Result<_>>()?;
                if v.iter().any(|(o, _)| *o != v[0].0) {
                    return Err(Error::Usage("the oracles disagree: waiting for the refund".into()));
                }
                Attested::Outcome(v[0].0.clone(), v.into_iter().map(|(_, s)| s).collect())
            }
        }))
    }

    fn outcome_name(a: &Attested) -> String {
        match a {
            Attested::Numeric(n) => n[0].value.to_string(),
            Attested::Outcome(o, _) => o.clone(),
        }
    }

    fn close_spec(&self, c: &Contract, a: &Attested, tip: u32, anchor: orchard::Anchor) -> Result<Spec> {
        Ok(match a {
            Attested::Numeric(n) => c.close_spec(&self.p, tip, c.terms.payout_at(n[0].value)?, anchor)?,
            Attested::Outcome(o, _) => c.close_outcome(&self.p, tip, o, anchor)?,
        })
    }

    fn cet(&self, c: &Contract, a: &Attested, tip: u32) -> Result<tx::Final> {
        Ok(match a {
            Attested::Numeric(n) => c.cet_numeric(n, tip + 1)?.0,
            Attested::Outcome(o, s) => c.cet_outcome(o, s, tip + 1)?,
        })
    }

    async fn broadcast(&self, rec: &mut Record, how: &str, f: &tx::Final) -> Result<()> {
        match self.chain.send(&f.raw).await {
            Ok(()) => {
                rec.pending = Some(Settlement { how: how.into(), txid: Some(txid_hex(&f.txid)), height: None });
                self.storage.put(rec).await
            }
            // Already spent or not yet valid: the next step sees which.
            Err(e) => {
                self.storage.put(rec).await?;
                self.note(&rec.id, &format!("{how}: {e}")).await
            }
        }
    }

    async fn settle_step(&self, mut rec: Record) -> Result<()> {
        let tip = self.chain.height().await?;
        let ftxid = txid_from_hex(rec.funding_txid.as_deref().unwrap_or_default())?;
        if rec.state == State::Signed {
            match self.chain.mined_height(&ftxid).await? {
                Some(h) => {
                    rec.state = State::Funded;
                    self.storage.put(&rec).await?;
                    self.emit(Event::Funded { id: rec.id.clone(), txid: txid_hex(&ftxid), height: h }).await?;
                }
                None => return Ok(()),
            }
        }
        self.contract_of(&rec).await?;
        let live = self.live.lock().await;
        let c = live.get(&rec.id).expect("loaded");
        // Spent? Then it is settled, by whatever got there first: the close this side
        // built or co-signed, the CET of the attested outcome, or the refund.
        let p2sh = script::p2sh(&c.txs.redeem);
        if self.chain.utxos(&p2sh, self.net).await?.is_empty() {
            let mut known: Vec<(String, [u8; 32])> = vec![];
            if let Some(Settlement { how, txid: Some(t), .. }) = &rec.pending {
                known.push((how.clone(), txid_from_hex(t)?));
            }
            if tip >= c.terms.maturity {
                if let Some(a) = self.attested(&rec, c).await.ok().flatten() {
                    known.push(("cet".into(), self.cet(c, &a, tip)?.txid));
                }
            }
            known.push(("refund".into(), c.refund()?.txid));
            let mut s = Settlement { how: "counterparty".into(), txid: None, height: None };
            for (how, t) in known {
                if let Some(h) = self.chain.mined_height(&t).await? {
                    s = Settlement { how, txid: Some(txid_hex(&t)), height: Some(h) };
                    break;
                }
            }
            if s.txid.is_none() && rec.pending.as_ref().is_some_and(|p| p.txid.is_some()) {
                return Ok(()); // ours may still be in the mempool
            }
            drop(live);
            if let Some(cur) = self.storage.get(&rec.id).await? {
                rec.note = cur.note; // keep what happened on the way
            }
            rec.state = State::Settled;
            rec.settlement = Some(s.clone());
            rec.pending = None;
            self.storage.put(&rec).await?;
            self.release(&rec.inputs).await?;
            self.live.lock().await.remove(&rec.id);
            self.emit(Event::Settled { id: rec.id.clone(), how: s.how, txid: s.txid, height: s.height }).await?;
            if rec.role == Role::Offerer {
                if let Some(next) = rec.template.next_period() {
                    // A payroll's next period, to the same employer.
                    self.offer(next, &rec.counterparty).await?;
                }
            }
            return Ok(());
        }
        if rec.pending.is_some() {
            return Ok(());
        }
        if tip >= c.terms.refund_height {
            let f = c.refund()?;
            drop(live);
            return self.broadcast(&mut rec, "refund", &f).await;
        }
        if tip < c.terms.maturity {
            return Ok(());
        }
        let Some(a) = self.attested(&rec, c).await? else { return Ok(()) };
        if rec.attested.is_none() {
            rec.attested = Some(Self::outcome_name(&a));
            rec.attested_at = Some(now_unix());
            self.storage.put(&rec).await?;
            self.emit(Event::Attested { id: rec.id.clone(), outcome: Self::outcome_name(&a) }).await?;
        }
        let late = tip >= c.terms.maturity + CLOSE_WAIT
            && rec.close_tip.is_none_or(|t| tip >= t + CLOSE_WAIT)
            && rec.attested_at.is_some_and(|t| now_unix() - t >= CLOSE_WAIT_SECS);
        if rec.role == Role::Offerer && rec.state == State::Funded {
            // Ask for the cooperative close.
            let anchor = self.chain.ironwood_anchor(tip).await?;
            let spec = self.close_spec(c, &a, tip, anchor)?;
            let pczt = tx::build(&self.p, &spec)?;
            drop(live);
            rec.close_tip = Some(tip);
            rec.state = State::Closing;
            self.storage.put(&rec).await?;
            return self.transport.send(&rec.counterparty, &rec.id, "close", json!({"tip": tip, "pczt": pczt_hex(&pczt)?})).await;
        }
        if late {
            let f = self.cet(c, &a, tip)?;
            drop(live);
            return self.broadcast(&mut rec, "cet", &f).await;
        }
        Ok(())
    }

    /// Accepter: co-signs a close that pays exactly the attested outcome.
    async fn on_close(&self, mut rec: Record, body: &Value) -> Result<()> {
        self.contract_of(&rec).await?;
        let live = self.live.lock().await;
        let c = live.get(&rec.id).expect("loaded");
        let a = self.attested(&rec, c).await?.ok_or(Error::Usage("close asked before the oracles attested".into()))?;
        let tip_req = body["tip"].as_u64().unwrap_or_default() as u32;
        let anchor = self.chain.ironwood_anchor(tip_req).await?;
        let spec = self.close_spec(c, &a, tip_req, anchor)?;
        let pczt = pczt_parse(body["pczt"].as_str().unwrap_or_default())?;
        c.check_close(&pczt, &spec)?;
        let txid = tx::txid(&pczt)?;
        let signed = c.sign_close(pczt)?;
        drop(live);
        rec.pending = Some(Settlement { how: "close".into(), txid: Some(txid_hex(&txid)), height: None });
        self.storage.put(&rec).await?;
        self.transport.send(&rec.counterparty, &rec.id, "close-signed", json!({"pczt": pczt_hex(&signed)?})).await
    }

    /// Offerer: checks the co-signed close, signs and broadcasts it.
    async fn on_close_signed(&self, mut rec: Record, body: &Value) -> Result<()> {
        self.contract_of(&rec).await?;
        let live = self.live.lock().await;
        let c = live.get(&rec.id).expect("loaded");
        let a = self.attested(&rec, c).await?.ok_or(Error::Usage("not attested".into()))?;
        let tip = rec.close_tip.ok_or(Error::Usage("no close asked".into()))?;
        let spec = self.close_spec(c, &a, tip, self.chain.ironwood_anchor(tip).await?)?;
        let back = pczt_parse(body["pczt"].as_str().unwrap_or_default())?;
        c.check_close(&back, &spec)?;
        let f = tx::extract(c.sign_close(back)?)?;
        drop(live);
        self.broadcast(&mut rec, "close", &f).await
    }

    /// Moves a CET or refund payout into the shielded balance.
    async fn shield_payout(&self, mut rec: Record) -> Result<()> {
        let key = self.wallet.payout_key(rec.index);
        let coins: Vec<(Coin, SecretKey)> = self
            .chain
            .utxos(&self.wallet.payout_address(rec.index), self.net)
            .await?
            .into_iter()
            .filter(|u| u.3 > 0)
            .map(|(txid, index, value, _)| (Coin { txid, index, value, spend: Spend::P2pkh(zdlc::keys::pubkey(&key)) }, key))
            .collect();
        if coins.is_empty() {
            return Ok(());
        }
        let tip = self.chain.height().await?;
        let f = funds::shield(&self.p, tip, &coins, self.wallet.deposit_address(rec.index), self.chain.ironwood_anchor(tip).await?)?;
        self.chain.send(&f.raw).await?;
        rec.shielded_payout = Some(txid_hex(&f.txid));
        self.storage.put(&rec).await?;
        self.emit(Event::PayoutShielded { id: rec.id.clone(), txid: txid_hex(&f.txid) }).await
    }

    // ---- The loop -----------------------------------------------------------------------

    async fn on_message(&self, env: wire::Envelope) -> Result<()> {
        if env.kind == "offer" {
            return self.on_offer(&env.from, &env.thread, &env.body).await;
        }
        let Some(rec) = self.storage.get(&env.thread).await? else { return Ok(()) };
        if rec.counterparty != env.from {
            return Ok(());
        }
        match (env.kind.as_str(), rec.role, rec.state) {
            ("accept", Role::Offerer, State::Offered) => self.on_accept(rec, &env.body).await,
            ("refresh", Role::Offerer, State::Offered) => {
                let mut rec = rec;
                self.send_offer(&mut rec).await
            }
            ("sign", Role::Accepter, State::Accepted) => self.on_sign(rec, &env.body).await,
            // A close may arrive before this side has seen the funding mined.
            ("close", Role::Accepter, State::Signed | State::Funded) => self.on_close(rec, &env.body).await,
            ("close-signed", Role::Offerer, State::Closing) => self.on_close_signed(rec, &env.body).await,
            ("decline", Role::Offerer, State::Offered) => {
                let mut rec = rec;
                rec.state = State::Declined;
                self.storage.put(&rec).await?;
                self.release(&rec.inputs).await?;
                self.emit(Event::Declined { id: rec.id.clone() }).await
            }
            (kind, role, state) => {
                eprintln!("zdlc: {kind} on {} ignored ({role:?}, {state:?})", rec.id);
                Ok(())
            }
        }
    }

    /// One pass: read messages, watch deposits, funding and settlement. Errors on one
    /// contract are recorded on it and do not stop the others.
    pub async fn step(&self) -> Result<()> {
        let _g = self.step.lock().await;
        for env in self.transport.receive().await? {
            let (id, kind) = (env.thread.clone(), env.kind.clone());
            if std::env::var("ZDLC_TRACE").is_ok() {
                eprintln!("zdlc: {} received {kind} on {id}", &self.identity()[..8]);
            }
            if let Err(e) = self.on_message(env).await {
                self.fail(&id, &format!("{kind}: {e}")).await?;
            }
        }
        for rec in self.storage.list().await? {
            let id = rec.id.clone();
            let r = match rec.state {
                State::AwaitingDeposit => self.poll_deposit(rec).await,
                State::Signed | State::Funded | State::Closing => {
                    if self.records && rec.record_txid.is_none() {
                        // No free treasury coin at signing (the collateral took it): try
                        // again; the funding change will do.
                        self.retry_record(&rec).await;
                    }
                    match self.storage.get(&id).await? {
                        Some(rec) => self.settle_step(rec).await,
                        None => Ok(()),
                    }
                }
                State::Settled if self.auto_shield && rec.shielded_payout.is_none() && rec.settlement.as_ref().is_some_and(|s| s.how != "close") => {
                    self.shield_payout(rec).await
                }
                _ => Ok(()),
            };
            if let Err(e) = r {
                self.note(&id, &e.to_string()).await?;
            }
        }
        Ok(())
    }

    async fn note(&self, id: &str, msg: &str) -> Result<()> {
        if let Some(mut rec) = self.storage.get(id).await? {
            if rec.note.as_deref() != Some(msg) {
                rec.note = Some(msg.into());
                self.storage.put(&rec).await?;
                self.emit(Event::Warning { id: id.into(), message: msg.into() }).await?;
            }
        }
        Ok(())
    }

    /// A setup that cannot continue (a bad message, a refused check). Settled-side
    /// problems only note: the refund still protects a funded contract.
    async fn fail(&self, id: &str, reason: &str) -> Result<()> {
        let Some(mut rec) = self.storage.get(id).await? else { return Ok(()) };
        if matches!(rec.state, State::Signed | State::Funded | State::Closing | State::Settled) {
            return self.note(id, reason).await;
        }
        rec.state = State::Failed;
        rec.note = Some(reason.into());
        self.storage.put(&rec).await?;
        self.release(&rec.inputs).await?;
        self.emit(Event::Failed { id: id.into(), reason: reason.into() }).await
    }

    /// Runs [`Zdlc::step`] every `every` until the process stops.
    pub async fn run(self: Arc<Self>, every: std::time::Duration) {
        loop {
            if let Err(e) = self.step().await {
                eprintln!("zdlc: step: {e}");
            }
            tokio::time::sleep(every).await;
        }
    }

    // ---- Money --------------------------------------------------------------------------

    async fn shielded(&self) -> Result<Vec<(orchard::keys::SpendingKey, u32, Vec<zdlc::notes::OwnedNote>)>> {
        let tip = self.chain.height().await?;
        let mut out = vec![];
        for rec in self.storage.list().await? {
            for sk in [self.wallet.deposit_sk(rec.index), self.wallet.close_sk(rec.index)] {
                let fvk = orchard::keys::FullViewingKey::from(&sk);
                let n: Vec<_> = self.chain.notes(&fvk, rec.created_height, tip).await?.into_iter().filter(|n| n.value() > 0).collect();
                if !n.is_empty() {
                    out.push((sk, rec.index, n));
                }
            }
        }
        Ok(out)
    }

    pub async fn balance(&self) -> Result<Balance> {
        let treasury = self.chain.utxos(&self.wallet.coin_address(0), self.net).await?.iter().map(|u| u.2).sum();
        let held = self.shielded().await?;
        let in_contracts = self
            .storage
            .list()
            .await?
            .iter()
            .filter(|r| matches!(r.state, State::Signed | State::Funded | State::Closing))
            .map(|r| r.template.collateral(r.role))
            .sum();
        Ok(Balance {
            treasury_zat: treasury,
            shielded_zat: held.iter().flat_map(|(_, _, n)| n.iter().map(|x| x.value())).sum(),
            shielded_notes: held.iter().map(|(_, _, n)| n.len()).sum(),
            in_contracts_zat: in_contracts,
        })
    }

    /// Sends the whole shielded balance to a unified address (its Ironwood receiver).
    /// Returns the txids, one per key that held notes.
    pub async fn withdraw(&self, unified_address: &str) -> Result<Vec<String>> {
        let (_, to) = paylink::decode_unified(unified_address).ok_or(Error::Usage("not a unified address with an Ironwood receiver".into()))?;
        let mut out = vec![];
        for (sk, _, notes) in self.shielded().await? {
            let fvk = orchard::keys::FullViewingKey::from(&sk);
            let tip = self.chain.height().await?;
            let (spends, anchor) = self.chain.spends(&fvk, &notes, tip).await?;
            let have: u64 = notes.iter().map(|n| n.value()).sum();
            let mut spec = Spec {
                target_height: tip + 1,
                expiry_height: tip + 1 + contract::EXPIRY_DELTA,
                lock_height: None,
                inputs: vec![],
                outputs: vec![Pay::Ironwood(to, 1)],
                ironwood_anchor: Some(anchor),
                ironwood_spends: spends,
            };
            let fee = tx::fee(&self.p, &spec)?;
            spec.outputs = vec![Pay::Ironwood(to, have.checked_sub(fee).ok_or(Error::Usage("notes below the fee".into()))?)];
            let (pczt, _) = tx::sign_ironwood(tx::build(&self.p, &spec)?, &orchard::keys::SpendAuthorizingKey::from(&sk))?;
            let f = tx::extract(pczt)?;
            self.chain.send(&f.raw).await?;
            out.push(txid_hex(&f.txid));
        }
        Ok(out)
    }
}
