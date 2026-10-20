//! Multi-beneficiary payouts offline: a client escrows 1.5 ZEC for a team of three. The
//! client funds alone (offer side); the team lead signs (accept side) and is paid nothing
//! itself. "delivered" pays the team 50 / 30 / 20 %, by CET and by cooperative close.

use orchard::keys::{FullViewingKey, PreparedIncomingViewingKey, Scope};
use pczt::Pczt;
use zcash_transparent::address::TransparentAddress;
use zdlc::{
    contract::{Contract, Event, Party, Payee, Payouts, Split, Splits, Terms},
    oracle,
    params::{self, NU7_AT},
    regtest::{dev_beneficiary, dev_party},
    tx::{self, Pay},
};

const POT: u64 = 1_5000_0000;

thread_local! {
    /// The last setup's signed funding transaction.
    static FUNDING: std::cell::RefCell<Option<tx::Final>> = const { std::cell::RefCell::new(None) };
}

/// Transparent outputs of a PCZT, from its effects.
fn vout(pczt: &Pczt) -> Vec<(TransparentAddress, u64)> {
    let fx = pczt.clone().into_effects().unwrap();
    fx.transparent_bundle()
        .map(|b| {
            b.vout
                .iter()
                .map(|o| (o.recipient_address().unwrap(), o.value().into_u64()))
                .collect()
        })
        .unwrap_or_default()
}

pub fn team(tag: &str, ironwood: [bool; 3]) -> (Splits, Vec<FullViewingKey>) {
    let mut bens = vec![];
    let mut fvks = vec![];
    for (i, name) in ["ana", "ben", "chen"].iter().enumerate() {
        let (b, f) = dev_beneficiary(tag, name, 0x30 + i as u8, ironwood[i]);
        bens.push(b);
        fvks.push(f);
    }
    let splits = Splits {
        beneficiaries: bens,
        offer: Split::default(),
        accept: Split(vec![
            (Payee::Beneficiary(0), 5_000),
            (Payee::Beneficiary(1), 3_000),
            (Payee::Beneficiary(2), 2_000),
        ]),
        outcomes: vec![],
    };
    (splits, fvks)
}

fn setup(tag: &str, splits: Splits, tip: u32) -> (Terms, Party, Party, Contract, Contract, oracle::DevOracle) {
    let client = dev_party(tag, "client", 0x21).into_party([0x21; 32], 0, 2_0000_0000);
    setup_with(&params::regtest(Some(NU7_AT)), tag, client, splits, tip, tip + 30)
}

fn setup_with(
    p: &zcash_protocol::local_consensus::LocalNetwork,
    tag: &str,
    client: Party,
    splits: Splits,
    tip: u32,
    refund_height: u32,
) -> (Terms, Party, Party, Contract, Contract, oracle::DevOracle) {
    let mut lead = dev_party(tag, "lead", 0x22).into_party([0x22; 32], 0, 1);
    lead.info.inputs.clear();
    lead.input_keys.clear();
    let o = oracle::DevOracle::new(tag, "delivery", &["delivered", "not delivered"]);
    let terms = Terms {
        event: Event::Enumerated(vec![o.announcement().clone()]),
        payouts: Payouts::Enumerated(vec![("delivered".into(), 0), ("not delivered".into(), POT)]),
        offer_collateral: POT,
        accept_collateral: 0,
        maturity: tip + 10,
        refund_height,
        sign_height: tip + 1,
        dual_sign: false,
        roll: None,
        splits,
    };
    let offer = client.offer(terms.clone());
    let (accept, lead_txs) = lead.accept(p, tip, &offer).unwrap();
    let (sign, client_c) = client.sign(p, tip, &offer, &accept).unwrap();
    let (funding, lead_c) = lead.finish(&offer, lead_txs, &sign).unwrap();
    FUNDING.with(|f| *f.borrow_mut() = Some(funding));
    (terms, client, lead, client_c, lead_c, o)
}

#[test]
fn team_escrow_pays_three_beneficiaries_by_cet() {
    let p = params::regtest(Some(NU7_AT));
    let tip = 120;
    let (splits, _) = team("team-cet", [true; 3]);
    let bens = splits.beneficiaries.clone();
    let (terms, client, _, client_c, lead_c, mut o) = setup("team-cet", splits, tip);

    // The reserve covers a three-output CET: ZIP 317 charges 5,000 zat per output past two.
    assert_eq!(client_c.txs.reserve, 15_000);
    let (delivered, ..) = &client_c.txs.cets[0];
    let outs = vout(&client_c.txs.cets[0].1);
    println!("delivered CET outputs: {outs:?}");
    assert_eq!(
        outs,
        vec![(bens[0].payout, 7500_0000), (bens[1].payout, 4500_0000), (bens[2].payout, 3000_0000)]
    );
    assert_eq!(delivered.outputs.len(), 3);
    // "not delivered" and the refund pay the client alone; the reserve a one-output CET
    // leaves (15,000 less its 10,000 fee) goes to it.
    assert_eq!(vout(&client_c.txs.cets[1].1), vec![(client.info.payout, POT + 5_000)]);
    assert_eq!(vout(&client_c.txs.refund), vec![(client.info.payout, POT + 5_000)]);

    let s = o.attest("delivered").unwrap();
    let cet = lead_c.cet_outcome("delivered", &[s], tip + 11).unwrap();
    let cet_c = client_c.cet_outcome("delivered", &[s], tip + 11).unwrap();
    assert_eq!(cet.txid, cet_c.txid, "either signer can complete the CET");
    let _ = (terms, p);
}

#[test]
fn team_escrow_closes_to_each_beneficiary() {
    let p = params::regtest(Some(NU7_AT));
    let tip = 120;
    // Chen has no shielded address: the close pays Chen transparently.
    let (splits, fvks) = team("team-close", [true, true, false]);
    let bens = splits.beneficiaries.clone();
    let (_, _, _, client_c, lead_c, _) = setup("team-close", splits, tip);
    let anchor = orchard::Anchor::empty_tree();

    let agreed = lead_c.close_outcome(&p, tip + 5, "delivered", anchor).unwrap();
    let fee = tx::fee(&p, &agreed).unwrap();
    let paid: u64 = agreed.outputs.iter().map(Pay::value).sum();
    assert_eq!(paid + fee, client_c.txs.funding_coin.value);
    assert!(matches!(agreed.outputs[0], Pay::Ironwood(a, _) if a == bens[0].close.unwrap()));
    assert!(matches!(agreed.outputs[2], Pay::Transparent(a, _) if a == bens[2].payout));
    println!("close: {:?} values, fee {fee}", agreed.outputs.iter().map(Pay::value).collect::<Vec<_>>());

    let close = tx::build(&p, &client_c.close_outcome(&p, tip + 5, "delivered", anchor).unwrap()).unwrap();
    lead_c.check_close(&close, &agreed).unwrap();
    client_c.check_close(&close, &agreed).unwrap();
    // Each beneficiary finds its own note with its viewing key.
    for (i, f) in fvks.iter().take(2).enumerate() {
        let ivk = PreparedIncomingViewingKey::new(&f.to_ivk(Scope::External));
        assert!(decrypts(&close, &ivk), "beneficiary {i} finds its note");
    }

    // A close that pays another split is refused.
    let mut other = agreed.clone();
    let (a0, a1) = (other.outputs[0].value(), other.outputs[1].value());
    other.outputs[0] = Pay::Ironwood(bens[0].close.unwrap(), a0 + 1_000_0000);
    other.outputs[1] = Pay::Ironwood(bens[1].close.unwrap(), a1 - 1_000_0000);
    let err = lead_c.check_close(&tx::build(&p, &other).unwrap(), &agreed).unwrap_err();
    assert!(err.to_string().contains("differ"), "{err}");
    // So is one that moves Chen's transparent payout elsewhere.
    let mut moved = agreed.clone();
    moved.outputs[2] = Pay::Transparent(client_c.offer.payout, moved.outputs[2].value());
    assert!(lead_c.check_close(&tx::build(&p, &moved).unwrap(), &agreed).is_err());
}

fn decrypts(pczt: &Pczt, ivk: &PreparedIncomingViewingKey) -> bool {
    use pczt::roles::verifier::Verifier;
    use std::convert::Infallible;
    let mut found = false;
    Verifier::new(pczt.clone())
        .with_ironwood::<Infallible, _>(|b| {
            let v = *b.bundle_version();
            found = b.actions().iter().any(|a| matches!(a.decrypt_output_with_ivk(ivk, v), Ok(Some(_))));
            Ok(())
        })
        .unwrap();
    found
}

#[test]
fn per_outcome_overrides_pay_fixed_amounts() {
    let tip = 120;
    let (mut splits, _) = team("team-fixed", [true; 3]);
    // "delivered" pays Ana and Ben fixed fees and the rest back to the client.
    splits.outcomes = vec![(
        "delivered".into(),
        vec![(Payee::Beneficiary(0), 4000_0000), (Payee::Beneficiary(1), 2500_0000), (Payee::Offer, 8500_0000)],
    )];
    let bens = splits.beneficiaries.clone();
    let (_, client, ..) = (0, dev_party("team-fixed", "client", 0x21).into_party([0x21; 32], 0, 2_0000_0000), 0);
    let (_, _, _, client_c, _, _) = setup("team-fixed", splits, tip);
    assert_eq!(
        vout(&client_c.txs.cets[0].1),
        vec![(client.info.payout, 8500_0000), (bens[0].payout, 4000_0000), (bens[1].payout, 2500_0000)]
    );
}

/// The M2 gate on Zakura regtest: the team escrow settles "delivered" by CET to three
/// transparent addresses, and again by cooperative close to three Ironwood receivers.
///
/// ```sh
/// infra/zakura/scripts/up.sh
/// cargo test -p zdlc --release --test beneficiaries -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore = "needs infra/zakura"]
async fn team_escrow_on_regtest() {
    use serde_json::json;
    use zdlc::{keys, regtest::Regtest, txid_hex};
    let url = std::env::var("ZDLC_NODE_URL").unwrap_or("http://127.0.0.1:18252/".into());
    let rt = Regtest::connect(&url).await.expect("infra/zakura/scripts/up.sh");
    rt.node.mine_to(NU7_AT + 10).await.unwrap();
    let mut log = vec![];
    for (run, settle) in [("team-rt-cet", "cet"), ("team-rt-close", "close")] {
        let tag = format!("{run}-{}", rt.tip().await.unwrap());
        let d = dev_party(&tag, "client", 0x21);
        let txid = rt.pay(&[(d.change, 2_0000_0000)]).await.unwrap();
        let client = d.into_party(txid, 0, 2_0000_0000);
        let (splits, fvks) = team(&tag, [true; 3]);
        let bens = splits.beneficiaries.clone();
        let tip = rt.tip().await.unwrap();
        let (_, _, _, client_c, lead_c, mut o) = setup_with(&rt.p, &tag, client, splits, tip, tip + 100);
        let funding = FUNDING.with(|f| f.borrow_mut().take()).unwrap();
        let h_fund = rt.send_confirm(&funding).await.unwrap();
        let s = o.attest("delivered").unwrap();
        if settle == "cet" {
            let cet = lead_c.cet_outcome("delivered", &[s], rt.tip().await.unwrap() + 1).unwrap();
            let h = rt.send_confirm(&cet).await.unwrap();
            let outs = rt.outputs(&cet).await.unwrap();
            println!("[team cet] funded {h_fund}, CET {h}: {outs:?}");
            assert_eq!(outs, vec![7500_0000, 4500_0000, 3000_0000]);
            for (b, v) in bens.iter().zip(&outs) {
                let u = rt.node.utxos(&keys::encode_regtest(&b.payout)).await.unwrap();
                assert!(u.iter().any(|c| c.0 == cet.txid && c.2 == *v), "{} holds its share", b.label);
            }
            log.push(json!({"run": "team escrow by CET", "funding_height": h_fund, "cet_height": h,
                "outputs_zat": outs, "payees": ["ana 50%", "ben 30%", "chen 20%"], "txid": txid_hex(&cet.txid)}));
        } else {
            let tip = rt.tip().await.unwrap();
            let anchor = orchard::Anchor::empty_tree();
            let agreed = lead_c.close_outcome(&rt.p, tip, "delivered", anchor).unwrap();
            let pczt = tx::build(&rt.p, &client_c.close_outcome(&rt.p, tip, "delivered", anchor).unwrap()).unwrap();
            lead_c.check_close(&pczt, &agreed).unwrap();
            for f in &fvks {
                assert!(decrypts(&pczt, &PreparedIncomingViewingKey::new(&f.to_ivk(Scope::External))));
            }
            // A close paying another split is refused before anyone signs.
            let mut other = agreed.clone();
            let (a0, a1) = (other.outputs[0].value(), other.outputs[1].value());
            other.outputs[0] = Pay::Ironwood(bens[0].close.unwrap(), a0 - 1_000_0000);
            other.outputs[1] = Pay::Ironwood(bens[1].close.unwrap(), a1 + 1_000_0000);
            let refused = lead_c.check_close(&tx::build(&rt.p, &other).unwrap(), &agreed).unwrap_err();
            let close = tx::extract(lead_c.sign_close(client_c.sign_close(pczt).unwrap()).unwrap()).unwrap();
            let pool0 = rt.node.pool_zat("ironwood").await.unwrap();
            let h = rt.send_confirm(&close).await.unwrap();
            let pool1 = rt.node.pool_zat("ironwood").await.unwrap();
            let values: Vec<u64> = agreed.outputs.iter().map(Pay::value).collect();
            println!("[team close] funded {h_fund}, close {h}: {values:?}; other split refused: {refused}");
            assert_eq!((pool1 - pool0) as u64, values.iter().sum::<u64>());
            log.push(json!({"run": "team escrow by cooperative close", "funding_height": h_fund, "close_height": h,
                "ironwood_outputs_zat": values, "ironwood_pool_delta": pool1 - pool0, "each_beneficiary_decrypts": true,
                "other_split_refused": refused.to_string(), "txid": txid_hex(&close.txid)}));
        }
    }
    if let Ok(path) = std::env::var("ZDLC_RESULTS") {
        std::fs::write(path, serde_json::to_string_pretty(&log).unwrap()).unwrap();
    }
}

#[test]
fn splits_cross_the_wire_and_old_messages_decode() {
    use zdlc::wire;
    let tip = 120;
    let (splits, _) = team("team-wire", [true, false, true]);
    let (terms, client, ..) = setup("team-wire", splits, tip);
    let w = wire::offer_to_wire(&client.offer(terms.clone())).unwrap();
    let json = serde_json::to_value(&w).unwrap();
    let back = wire::offer_from_wire(&serde_json::from_value(json.clone()).unwrap()).unwrap();
    let p = params::regtest(Some(NU7_AT));
    let lead = dev_party("team-wire", "lead", 0x22).into_party([0x22; 32], 0, 1);
    let mut lead_info = lead.info.clone();
    lead_info.inputs.clear();
    let a = zdlc::contract::build_txs(&p, &terms, &client.info, &lead_info).unwrap();
    let b = zdlc::contract::build_txs(&p, &back.terms, &back.offer, &lead_info).unwrap();
    assert_eq!(a.funding_txid, b.funding_txid);
    assert!(a.cets.iter().zip(&b.cets).all(|(x, y)| x.2 == y.2), "same CETs after the round trip");

    // Default terms carry no "splits" key; a message without one decodes as the default.
    let mut plain = terms.clone();
    plain.splits = Splits::default();
    let pj = serde_json::to_value(wire::offer_to_wire(&client.offer(plain)).unwrap()).unwrap();
    assert!(pj["terms"].get("splits").is_none());
    assert!(wire::offer_from_wire(&serde_json::from_value(pj).unwrap()).unwrap().terms.splits.is_default());
    assert!(json["terms"]["splits"]["beneficiaries"].as_array().unwrap().len() == 3);
}
