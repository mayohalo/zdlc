//! The two desk agents. A taker (the recipient of a stable-value hedge) posts an RFQ,
//! picks the best firm quote, and runs offer → accept → sign with the maker (the hedger)
//! through the desk's sealed mailbox. Each agent has its own wallet and talks to the
//! other only through the desk, the oracles and the chain.

use std::collections::{BTreeMap, BTreeSet};

use secp256k1::SecretKey;
use serde_json::{json, Value};
use zcash_protocol::{consensus::NetworkType, local_consensus::LocalNetwork};

use crate::{
    contract::{Contract, Event, Party, PartyInfo, Payouts, Terms},
    desk::{self, DeskClient, QuoteBody, RfqBody, Signed},
    enclave::EventSpec,
    feeds,
    node::Node,
    oracle_client::{self, OracleClient},
    params,
    payout::StableValue,
    tx::{self, Coin, Spend},
    txid_hex,
    wallet::Wallet,
    wire::{self, Envelope, WireCoin},
    Error, Result,
};

/// Extra value the funding inputs must cover beyond collateral: reserve and fee shares.
pub const CUSHION: u64 = 20_0000;

pub struct Agent {
    pub name: String,
    pub node: Node,
    pub p: LocalNetwork,
    pub net: NetworkType,
    pub desk: DeskClient,
    pub wallet: Wallet,
    pub id_key: SecretKey,
    pub oracles: Vec<OracleClient>,
    pub state_path: String,
    pub mail_seq: u64,
    pub reserved: BTreeSet<(String, u32)>,
}

pub fn log(who: &str, what: &str, v: Value) {
    println!("[{who}] {what}: {v}");
}

/// A step log: printed as it happens, returned as JSON.
pub struct Log {
    pub name: String,
    pub out: Vec<Value>,
}

impl Log {
    pub fn note(&mut self, what: &str, v: Value) {
        log(&self.name, what, v.clone());
        self.out.push(json!({"step": what, "result": v}));
    }
}

impl Agent {
    pub async fn new(name: &str, node: Node, desk: &str, seed: [u8; 32], oracles: &[String], state_path: &str) -> Result<Self> {
        let info = node.blockchain_info().await?;
        let net = match info["chain"].as_str() {
            Some("main") => NetworkType::Main,
            Some("test") => NetworkType::Test,
            _ => NetworkType::Regtest,
        };
        let p = params::from_node(&info["upgrades"]).ok_or(Error::Rpc("node lacks NU6.3".into()))?;
        let wallet = Wallet::from_seed(seed);
        Ok(Self {
            name: name.into(),
            node,
            p,
            net,
            desk: DeskClient::new(desk),
            id_key: wallet.identity_key(),
            wallet,
            oracles: oracles.iter().map(OracleClient::new).collect(),
            state_path: state_path.into(),
            mail_seq: 0,
            reserved: BTreeSet::new(),
        })
    }

    pub fn identity(&self) -> String {
        wire::identity(&self.id_key)
    }

    /// The next contract index (persisted).
    pub fn next_index(&self) -> Result<u32> {
        let mut st: Value = std::fs::read(&self.state_path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or(json!({"next_index": 1}));
        let i = st["next_index"].as_u64().unwrap_or(1) as u32;
        st["next_index"] = json!(i + 1);
        std::fs::write(&self.state_path, serde_json::to_vec_pretty(&st).unwrap()).map_err(|e| Error::Build(e.to_string()))?;
        Ok(i)
    }

    pub fn treasury_address(&self) -> String {
        crate::keys::encode(&self.wallet.coin_address(0), self.net)
    }

    /// Unreserved treasury coins covering `need`, largest first.
    pub async fn select(&mut self, need: u64) -> Result<Vec<Coin>> {
        let mut utxos = self.node.utxos(&self.treasury_address()).await?;
        utxos.sort_by_key(|u| std::cmp::Reverse(u.2));
        let mut out = vec![];
        let mut got = 0;
        for (txid, index, value, _) in utxos {
            let key = (txid_hex(&txid), index);
            if self.reserved.contains(&key) {
                continue;
            }
            out.push(Coin { txid, index, value, spend: Spend::P2pkh(crate::keys::pubkey(&self.wallet.coin_key(0))) });
            got += value;
            if got >= need {
                for c in &out {
                    self.reserved.insert((txid_hex(&c.txid), c.index));
                }
                return Ok(out);
            }
        }
        Err(Error::Build(format!("{}: treasury {} cannot cover {need} zat", self.name, self.treasury_address())))
    }

    /// The party this wallet plays in contract `i`, funding from treasury `inputs`;
    /// change returns to the treasury.
    pub fn party(&self, i: u32, inputs: Vec<Coin>) -> Party {
        let w = &self.wallet;
        Party {
            funding_sk: w.funding_key(i),
            input_keys: vec![w.coin_key(0); inputs.len()],
            close_fvk: w.close_fvk(i),
            roll_key: None,
            info: PartyInfo {
                funding_pubkey: crate::keys::pubkey(&w.funding_key(i)),
                inputs,
                change: w.coin_address(0),
                payout: w.payout_address(i),
                close: w.close_address(i),
            },
        }
    }

    pub async fn send(&self, to: &str, thread: &str, kind: &str, body: Value) -> Result<()> {
        let env = Envelope::new(&self.id_key, to, thread, kind, body);
        self.desk.send(&env.seal()?).await
    }

    /// New envelopes for this agent, verified and decrypted.
    pub async fn inbox(&mut self) -> Result<Vec<Envelope>> {
        let boxes = self.desk.read(&self.id_key, self.mail_seq).await?;
        let mut out = vec![];
        for (seq, b) in boxes {
            self.mail_seq = self.mail_seq.max(seq + 1);
            match b.open(&self.id_key) {
                Ok(e) => out.push(e),
                Err(e) => log(&self.name, "dropped a box", json!(e.to_string())),
            }
        }
        Ok(out)
    }

    /// Waits for an envelope of `kind` in `thread` from `from`.
    pub async fn wait_for(&mut self, thread: &str, kind: &str, from: &str, timeout_s: u64) -> Result<Envelope> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_s);
        loop {
            for e in self.inbox().await? {
                if e.thread == thread && e.kind == kind && e.from == from {
                    return Ok(e);
                }
            }
            if std::time::Instant::now() > deadline {
                return Err(Error::Rpc(format!("{}: no {kind} in {thread}", self.name)));
            }
            tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        }
    }

    pub async fn oracle_keys(&self) -> Result<Vec<String>> {
        let mut keys = vec![];
        for o in &self.oracles {
            keys.push(o.info().await?["pubkey"].as_str().unwrap_or_default().to_string());
        }
        Ok(keys)
    }

    pub async fn send_wait(&self, f: &tx::Final) -> Result<u32> {
        self.node.send(&f.raw).await?;
        self.node.wait_mined(&txid_hex(&f.txid)).await
    }
}

pub fn coin_to_wire(c: &Coin) -> WireCoin {
    let Spend::P2pkh(pk) = &c.spend else { unreachable!("treasury coins are P2PKH") };
    WireCoin { txid: txid_hex(&c.txid), index: c.index, value: c.value, pubkey: hex::encode(pk.serialize()) }
}

// ---- Taker -----------------------------------------------------------------------------

pub struct TakerArgs {
    /// Trade on a memo invitation found in the inbox from this height, instead of an RFQ.
    pub invitations_from: Option<u32>,
    pub q_recipient: u64,
    pub maturity_blocks: u32,
    pub refund_blocks: u32,
    /// Minutes from now of the ZEC/USD minute the oracles attest (negative: a closed one).
    pub event_offset_min: i64,
    pub quote_wait_s: u64,
    pub settle: String,
}

/// Runs one contract as taker, from RFQ to settlement. Returns a JSON log.
pub async fn run_taker(a: &mut Agent, args: &TakerArgs) -> Result<Vec<Value>> {
    let mut lg = Log { name: a.name.clone(), out: vec![] };
    if let Some(from) = args.invitations_from {
        // Watch the inbox until an invitation arrives (up to two minutes).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        let mut scanned = from;
        let inv = loop {
            let tip = a.node.height().await?;
            if tip >= scanned {
                let invs = crate::invite::scan(&a.node, &a.wallet, scanned, tip).await?;
                scanned = tip + 1;
                if let Some(i) = invs.into_iter().rev().find(|i| i.valid_until > tip + 2) {
                    break i;
                }
            }
            if std::time::Instant::now() > deadline {
                return Err(Error::Build("no live invitation in the inbox".into()));
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        };
        lg.note("invitation found", json!({"quote": inv.quote_id, "maker": inv.maker, "premium": inv.premium, "valid_until": inv.valid_until}));
        let desk = DeskClient::new(&inv.desk);
        let quote = desk.quote(&inv.quote_id).await?;
        quote.verify(desk::QUOTE_TAG)?;
        if quote.from != inv.maker || quote.body.q_hedger != inv.q_hedger || quote.body.premium != inv.premium || quote.body.valid_until != inv.valid_until {
            return Err(Error::Build("the desk's quote does not match the invitation".into()));
        }
        a.desk = desk;
        let t = TakerArgs { q_recipient: inv.q_recipient, maturity_blocks: inv.maturity_blocks, refund_blocks: inv.refund_blocks, invitations_from: None, settle: args.settle.clone(), ..*args };
        return trade(a, &t, "private".into(), inv.quote_id, quote, lg).await;
    }
    let oracle_keys = a.oracle_keys().await?;
    let mut nonce = [0u8; 8];
    rand10::fill(&mut nonce);
    let rfq = Signed::new(
        desk::RFQ_TAG,
        RfqBody {
            q_recipient: args.q_recipient,
            maturity_blocks: args.maturity_blocks,
            refund_blocks: args.refund_blocks,
            oracle_keys: oracle_keys.clone(),
            unit_cents: 100,
            nb_digits: 14,
            nonce: hex::encode(nonce),
        },
        &a.id_key,
    );
    let rfq_id = a.desk.post_rfq(&rfq).await?;
    lg.note("rfq posted", json!({"id": rfq_id, "q_recipient": args.q_recipient, "taker": a.identity()}));

    // Collect firm quotes; keep the cheapest one that is still valid and verifiably funded.
    tokio::time::sleep(std::time::Duration::from_secs(args.quote_wait_s)).await;
    let tip = a.node.height().await?;
    let mut best: Option<(String, Signed<QuoteBody>)> = None;
    for (id, q) in a.desk.quotes(&rfq_id).await? {
        if q.verify(desk::QUOTE_TAG).is_err() || q.body.valid_until <= tip + 2 || q.body.q_hedger < args.q_recipient {
            continue;
        }
        if best.as_ref().is_none_or(|(_, b)| q.body.premium < b.body.premium) {
            best = Some((id, q));
        }
    }
    let (quote_id, quote) = best.ok_or(Error::Build("no usable quote".into()))?;
    lg.note("quote chosen", json!({"id": quote_id, "maker": quote.from, "q_hedger": quote.body.q_hedger,
        "premium": quote.body.premium, "valid_until": quote.body.valid_until, "inputs": quote.body.inputs.len()}));
    trade(a, args, rfq_id, quote_id, quote, lg).await
}

/// Offer → accept → sign → funding → settlement on a chosen quote.
async fn trade(a: &mut Agent, args: &TakerArgs, rfq_id: String, quote_id: String, quote: Signed<QuoteBody>, mut lg: Log) -> Result<Vec<Value>> {

    // The event: ZEC/USD at a fixed minute, announced by both oracles.
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let (spot, _) = feeds::zecusd_minute((now / 60 - 2) * 60).await?;
    let minute = (now / 60 + args.event_offset_min) * 60;
    let event_id = format!("zecusd-{minute}");
    let spec = EventSpec::ZecUsd { minute, unit_cents: 100, nb_digits: 14 };
    let mut anns = vec![];
    for o in &a.oracles {
        anns.push(oracle_client::to_numeric(&o.announce(&event_id, &spec).await?)?);
    }
    let curve = StableValue {
        d_cents: (spot as u128 * args.q_recipient as u128 / 100_000_000) as u64,
        q_recipient: args.q_recipient,
        q_hedger: quote.body.q_hedger,
        unit_cents: 100,
        nb_digits: 14,
        rounding: quote.body.rounding,
        premium: quote.body.premium,
    };
    let i = a.next_index()?;
    let inputs = a.select(args.q_recipient + quote.body.premium + CUSHION).await?;
    let me = a.party(i, inputs);
    let tip = a.node.height().await?;
    let terms = Terms {
        event: Event::Numeric(anns.clone()),
        payouts: Payouts::StableValue(curve.clone()),
        offer_collateral: args.q_recipient + quote.body.premium,
        accept_collateral: quote.body.q_hedger,
        maturity: tip + args.maturity_blocks,
        refund_height: tip + args.refund_blocks,
        sign_height: tip + 1,
        dual_sign: false,
        roll: None,
    };
    let offer = me.offer(terms.clone());
    a.send(&quote.from, &quote_id, "offer", json!({"rfq_id": rfq_id, "offer": wire::offer_to_wire(&offer)?})).await?;
    lg.note("offer sent", json!({"event": event_id, "owed_usd_cents": curve.d_cents, "spot_cents": spot, "index": i}));

    let env = a.wait_for(&quote_id, "accept", &quote.from, 300).await?;
    let accept = wire::accept_from_wire(&serde_json::from_value(env.body["accept"].clone()).map_err(|e| Error::Build(e.to_string()))?)?;
    let (sign, contract) = me.sign(&a.p, a.node.height().await?, &offer, &accept)?;
    a.send(&quote.from, &quote_id, "sign", json!({"sign": wire::sign_to_wire(&sign)})).await?;
    lg.note("accepted and signed", json!({"cets": contract.txs.cets.len(), "adaptor_sigs_verified": contract.adaptor_count()}));

    let env = a.wait_for(&quote_id, "funded", &quote.from, 600).await?;
    let fund_txid = env.body["txid"].as_str().unwrap_or_default().to_string();
    if fund_txid != txid_hex(&contract.txs.funding_txid) {
        return Err(Error::Build("maker reported another funding txid".into()));
    }
    let h = a.node.wait_mined(&fund_txid).await?;
    lg.note("funded", json!({"txid": fund_txid, "height": h}));

    settle(a, &contract, &terms, &quote.from, &quote_id, &args.settle, &mut lg).await?;
    Ok(lg.out)
}

async fn attestations(a: &Agent, terms: &Terms) -> Result<Vec<crate::oracle::NumericAttestation>> {
    let Event::Numeric(anns) = &terms.event else { return Err(Error::Build("numeric only".into())) };
    let mut v = vec![];
    for (o, ann) in a.oracles.iter().zip(anns) {
        let att = o.wait_attestation(&ann.event_id).await?;
        v.push(oracle_client::verify_numeric(ann, &att)?);
    }
    Ok(v)
}

async fn settle(
    a: &mut Agent,
    c: &Contract,
    terms: &Terms,
    maker: &str,
    thread: &str,
    mode: &str,
    lg: &mut Log,
) -> Result<()> {
    a.node.wait_height(terms.maturity).await?;
    let atts = attestations(a, terms).await?;
    let payout = terms.payout_at(atts[0].value)?;
    lg.note("attested", json!({"price_usd": atts[0].value, "agree": atts.iter().all(|x| x.digits == atts[0].digits), "payout": payout}));
    if mode == "close" {
        let tip = a.node.height().await?;
        let anchor = a.node.ironwood_anchor(tip).await?;
        let spec = c.close_spec(&a.p, tip, payout, anchor)?;
        let pczt = tx::build(&a.p, &spec)?;
        let bytes = pczt.serialize().map_err(|e| Error::Pczt(format!("{e:?}")))?;
        a.send(maker, thread, "close", json!({"tip": tip, "value": atts[0].value, "pczt": hex::encode(bytes)})).await?;
        match a.wait_for(thread, "close-signed", maker, 120).await {
            Ok(env) => {
                let back = pczt::Pczt::parse(&hex::decode(env.body["pczt"].as_str().unwrap_or_default()).map_err(|e| Error::Build(e.to_string()))?)
                    .map_err(|e| Error::Pczt(format!("{e:?}")))?;
                // Our own check still applies to what comes back.
                c.check_close(&back, &spec)?;
                let f = tx::extract(c.sign_close(back)?)?;
                let h = a.send_wait(&f).await?;
                lg.note("closed to Ironwood", json!({"txid": txid_hex(&f.txid), "height": h}));
                return Ok(());
            }
            Err(e) => lg.note("maker did not co-sign the close; using the CET", json!(e.to_string())),
        }
    }
    let (cet, i) = c.cet_numeric(&atts, a.node.height().await? + 1)?;
    let h = a.send_wait(&cet).await?;
    lg.note("settled by CET", json!({"index": i, "txid": txid_hex(&cet.txid), "height": h}));
    Ok(())
}

// ---- Maker -----------------------------------------------------------------------------

pub struct Invite {
    /// The taker's Ironwood inbox.
    pub inbox: orchard::Address,
    pub q_recipient: u64,
    pub maturity_blocks: u32,
    pub refund_blocks: u32,
}

pub struct MakerArgs {
    /// Send one private quote by memo invitation at startup.
    pub invite: Option<Invite>,
    pub premium_bps: u64,
    pub cap: u64,
    pub quote_ttl_blocks: u32,
    /// Stop after this many contracts are funded and settled (0: run forever).
    pub contracts: usize,
}

struct Open {
    rfq: RfqBody,
    quote: QuoteBody,
    inputs: Vec<Coin>,
    /// Who may offer on this quote: the RFQ's poster, or (private quotes) whoever first
    /// presents the quote id from the invitation.
    taker: Option<String>,
    contract: Option<(Contract, Terms)>,
    party: Option<Party>,
    offer: Option<crate::contract::Offer>,
    txs: Option<crate::contract::TxSet>,
}

/// The maker loop: quote every acceptable RFQ, run setup for chosen quotes, co-sign
/// closes. Returns after `contracts` contracts have been closed or settled.
pub async fn run_maker(a: &mut Agent, args: &MakerArgs) -> Result<Vec<Value>> {
    let trusted = a.oracle_keys().await?;
    let mut quoted: BTreeSet<String> = BTreeSet::new();
    let mut open: BTreeMap<String, Open> = BTreeMap::new();
    let mut done = 0usize;
    let mut lg = Log { name: a.name.clone(), out: vec![] };
    lg.note("ready", json!({"identity": a.identity(), "treasury": a.treasury_address(), "oracles": trusted}));
    if let Some(inv) = &args.invite {
        let inputs = a.select(inv.q_recipient + CUSHION).await?;
        let tip = a.node.height().await?;
        let mut nonce = [0u8; 8];
        rand10::fill(&mut nonce);
        let quote = QuoteBody {
            rfq_id: format!("private-{}", hex::encode(nonce)),
            q_hedger: inv.q_recipient,
            premium: inv.q_recipient * args.premium_bps / 10_000,
            rounding: (2 * inv.q_recipient / 100).max(1),
            inputs: inputs.iter().map(coin_to_wire).collect(),
            valid_until: tip + args.quote_ttl_blocks,
        };
        let qid = a.desk.post_quote(&Signed::new(desk::QUOTE_TAG, quote.clone(), &a.id_key), true).await?;
        let memo = crate::invite::Invitation {
            quote_id: qid.clone(),
            maker: a.identity(),
            desk: a.desk.base.clone(),
            q_recipient: inv.q_recipient,
            q_hedger: quote.q_hedger,
            premium: quote.premium,
            maturity_blocks: inv.maturity_blocks,
            refund_blocks: inv.refund_blocks,
            valid_until: quote.valid_until,
        };
        let pay = a.select(crate::invite::INVITE_VALUE + 50_000).await?;
        let anchor = a.node.ironwood_anchor(tip).await?;
        let f = crate::invite::invitation_tx(&a.p, tip, &a.wallet, pay[0].clone(), inv.inbox, &memo, anchor)?;
        let h = a.send_wait(&f).await?;
        lg.note("invitation sent", json!({"quote": qid, "txid": txid_hex(&f.txid), "height": h, "memo_bytes": memo.encode()?.len()}));
        let rfq = RfqBody {
            q_recipient: inv.q_recipient,
            maturity_blocks: inv.maturity_blocks,
            refund_blocks: inv.refund_blocks,
            oracle_keys: trusted.clone(),
            unit_cents: 100,
            nb_digits: 14,
            nonce: hex::encode(nonce),
        };
        open.insert(qid, Open { taker: None, rfq, quote, inputs, contract: None, party: None, offer: None, txs: None });
    }
    loop {
        // Quote new requests.
        for (rfq_id, rfq) in a.desk.rfqs().await? {
            if quoted.contains(&rfq_id) {
                continue;
            }
            quoted.insert(rfq_id.clone());
            let ok = rfq.verify(desk::RFQ_TAG).is_ok()
                && rfq.body.oracle_keys == trusted
                && rfq.body.q_recipient <= args.cap
                && rfq.body.nb_digits == 14
                && rfq.body.unit_cents == 100;
            if !ok {
                lg.note("rfq declined", json!({"id": rfq_id}));
                continue;
            }
            let q_hedger = rfq.body.q_recipient;
            let inputs = match a.select(q_hedger + CUSHION).await {
                Ok(i) => i,
                Err(e) => {
                    lg.note("cannot quote", json!(e.to_string()));
                    continue;
                }
            };
            let tip = a.node.height().await?;
            let quote = QuoteBody {
                rfq_id: rfq_id.clone(),
                q_hedger,
                premium: rfq.body.q_recipient * args.premium_bps / 10_000,
                rounding: (2 * q_hedger / 100).max(1),
                inputs: inputs.iter().map(coin_to_wire).collect(),
                valid_until: tip + args.quote_ttl_blocks,
            };
            let signed = Signed::new(desk::QUOTE_TAG, quote.clone(), &a.id_key);
            let qid = a.desk.post_quote(&signed, false).await?;
            lg.note("quoted", json!({"rfq": rfq_id, "quote": qid, "premium": quote.premium, "valid_until": quote.valid_until}));
            open.insert(qid, Open { taker: Some(rfq.from.clone()), rfq: rfq.body, quote, inputs, contract: None, party: None, offer: None, txs: None });
        }

        // Handle mail.
        for env in a.inbox().await? {
            let Some(o) = open.get_mut(&env.thread) else { continue };
            match &o.taker {
                Some(t) if *t != env.from => continue,
                None if env.kind == "offer" => o.taker = Some(env.from.clone()),
                None => continue,
                _ => {}
            }
            let taker = o.taker.clone().unwrap();
            let tip = a.node.height().await?;
            match env.kind.as_str() {
                "offer" if o.offer.is_none() => {
                    let check = async {
                        if tip >= o.quote.valid_until {
                            return Err(Error::Build("quote expired".into()));
                        }
                        let offer = wire::offer_from_wire(&serde_json::from_value(env.body["offer"].clone()).map_err(|e| Error::Build(e.to_string()))?)?;
                        let t = &offer.terms;
                        let Payouts::StableValue(c) = &t.payouts else { return Err(Error::Build("not stable value".into())) };
                        let Event::Numeric(anns) = &t.event else { return Err(Error::Build("not numeric".into())) };
                        let keys: Vec<String> = anns.iter().map(|x| hex::encode(x.pubkey.serialize())).collect();
                        if c.q_hedger != o.quote.q_hedger || c.premium != o.quote.premium || c.rounding != o.quote.rounding
                            || c.q_recipient != o.rfq.q_recipient || t.accept_collateral != o.quote.q_hedger
                            || keys != o.rfq.oracle_keys
                            || t.maturity.saturating_sub(t.sign_height) + 1 != o.rfq.maturity_blocks
                            || t.refund_height.saturating_sub(t.sign_height) + 1 != o.rfq.refund_blocks
                        {
                            return Err(Error::Build("offer does not match the quote".into()));
                        }
                        // The announcements must be the oracles' own.
                        for (oc, ann) in a.oracles.iter().zip(anns) {
                            let real = oracle_client::to_numeric(&oc.announcement(&ann.event_id).await?)?;
                            if real.nonces != ann.nonces || real.pubkey != ann.pubkey {
                                return Err(Error::Build("announcement not from our oracle".into()));
                            }
                        }
                        Ok(offer)
                    }
                    .await;
                    match check {
                        Ok(offer) => {
                            let i = a.next_index()?;
                            let me = a.party(i, o.inputs.clone());
                            let (accept, txs) = me.accept(&a.p, tip, &offer)?;
                            a.send(&taker, &env.thread, "accept", json!({"accept": wire::accept_to_wire(&accept)?})).await?;
                            lg.note("accepted", json!({"quote": env.thread, "cets": txs.cets.len(), "index": i}));
                            o.party = Some(me);
                            o.offer = Some(offer);
                            o.txs = Some(txs);
                        }
                        Err(e) => lg.note("offer refused", json!({"quote": env.thread, "why": e.to_string()})),
                    }
                }
                "sign" if o.contract.is_none() && o.txs.is_some() => {
                    let sign = wire::sign_from_wire(&serde_json::from_value(env.body["sign"].clone()).map_err(|e| Error::Build(e.to_string()))?)?;
                    let (me, offer, txs) = (o.party.as_ref().unwrap(), o.offer.as_ref().unwrap(), o.txs.take().unwrap());
                    let (funding, c) = me.finish(offer, txs, &sign)?;
                    a.node.send(&funding.raw).await?;
                    a.send(&taker, &env.thread, "funded", json!({"txid": txid_hex(&funding.txid)})).await?;
                    lg.note("funded", json!({"quote": env.thread, "txid": txid_hex(&funding.txid)}));
                    o.contract = Some((c, offer.terms.clone()));
                }
                "close" => {
                    if let Some((c, terms)) = &o.contract {
                        let res = async {
                            let tip_req = env.body["tip"].as_u64().unwrap_or_default() as u32;
                            let value = env.body["value"].as_u64().unwrap_or_default() as usize;
                            // Our own attestations, not the taker's word.
                            let atts = attestations(a, terms).await?;
                            if atts[0].value != value {
                                return Err(Error::Build("close value differs from the attestation".into()));
                            }
                            let payout = terms.payout_at(value)?;
                            let pczt = pczt::Pczt::parse(&hex::decode(env.body["pczt"].as_str().unwrap_or_default()).map_err(|e| Error::Build(e.to_string()))?)
                                .map_err(|e| Error::Pczt(format!("{e:?}")))?;
                            let anchor = a.node.ironwood_anchor(tip_req).await?;
                            c.check_close(&pczt, &c.close_spec(&a.p, tip_req, payout, anchor)?)?;
                            let signed = c.sign_close(pczt)?;
                            Ok::<_, Error>(signed.serialize().map_err(|e| Error::Pczt(format!("{e:?}")))?)
                        }
                        .await;
                        match res {
                            Ok(bytes) => {
                                a.send(&taker, &env.thread, "close-signed", json!({"pczt": hex::encode(bytes)})).await?;
                                lg.note("co-signed close", json!({"quote": env.thread}));
                            }
                            Err(e) => lg.note("close refused", json!({"quote": env.thread, "why": e.to_string()})),
                        }
                    }
                }
                _ => {}
            }
        }

        // Count contracts whose funding output is spent (closed or settled).
        let mut finished = vec![];
        for (qid, o) in &open {
            if let Some((c, _)) = &o.contract {
                let spent = a.node.utxos(&crate::keys::encode(&crate::script::p2sh(&c.txs.redeem), a.net)).await?.is_empty();
                let mined = a.node.mined_height(&txid_hex(&c.txs.funding_txid)).await.ok().flatten().is_some();
                if mined && spent {
                    finished.push(qid.clone());
                }
            }
        }
        for qid in finished {
            open.remove(&qid);
            done += 1;
            lg.note("contract finished", json!({"quote": qid, "done": done}));
        }
        if args.contracts > 0 && done >= args.contracts {
            return Ok(lg.out);
        }
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    }
}
