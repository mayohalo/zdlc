//! Your first zDLC app: an escrow between a contractor and a client, settled by two
//! arbiters. It runs against the local dev stack, which gives it a regtest chain, the
//! desk (the parties' mailbox) and two arbiter oracles:
//!
//! ```sh
//! just dev                                               # once, in another terminal
//! cargo run -p zdlc-kit --release --example first_app
//! ```
//!
//! Each party is a kit node with its own seed. A real app gives each user their own
//! node (or the SDK); here both live in one process to keep the example short.

use std::{error::Error, sync::Arc, time::Duration};

use zdlc_kit::{impls::*, templates::Template, Funding, State, Zdlc};

const RPC: &str = "http://127.0.0.1:18252/";
const DESK: &str = "http://127.0.0.1:7900";
const ORACLES: [&str; 2] = ["http://127.0.0.1:7801", "http://127.0.0.1:7802"];

/// One party: a seed, the chain, a database, the desk as its mailbox, and the oracles.
async fn party(name: &str) -> Result<Arc<Zdlc>, Box<dyn Error>> {
    let mut seed = [0u8; 32];
    rand10::fill(&mut seed);
    let wallet = zdlc::wallet::Wallet::from_seed(seed);
    let mut b = Zdlc::builder()
        .seed(seed)
        .chain(Arc::new(ZakuraRpc::new(RPC)))
        .storage(Arc::new(SqliteStorage::open(&format!("first-app-{name}.db"))?))
        .transport(Arc::new(DeskTransport::new(DESK, wallet.identity_key())))
        .block_secs(2);
    for o in ORACLES {
        b = b.oracle(Arc::new(HttpOracle::new(o)));
    }
    let z = Arc::new(b.build().await?);
    // The node's loop: reads the mailbox, signs, funds, watches the chain, settles.
    tokio::spawn(z.clone().run(Duration::from_secs(1)));
    Ok(z)
}

/// Waits until a party's copy of contract `id` reaches `state`.
async fn wait(z: &Zdlc, id: &str, state: State) -> Result<(), Box<dyn Error>> {
    for _ in 0..300 {
        if let Some(rec) = z.contract(id).await? {
            if rec.state == state {
                return Ok(());
            }
            if matches!(rec.state, State::Failed | State::Declined) {
                return Err(format!("contract {id}: {:?} {:?}", rec.state, rec.note).into());
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Err(format!("contract {id} did not reach {state:?}").into())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // 1. Two parties.
    let contractor = party("contractor").await?;
    let client = party("client").await?;

    // 2. Fund the client's treasury. On regtest the miner pays it; on testnet you would
    //    send it ZEC, or accept with Funding::Deposit and pay the link from any wallet.
    let rt = zdlc::regtest::Regtest::connect(RPC).await?;
    let (_, treasury) = zdlc::keys::decode(&client.treasury_address()).ok_or("not a transparent address")?;
    rt.pay(&[(treasury, 10_000_000)]).await?;
    println!("funded the client's treasury {}", client.treasury_address());

    // 3. The client's app decides: accept every escrow, funded from the treasury. An app
    //    would show the offer to its user instead (Zdlc::accept or Zdlc::decline).
    client.set_policy(Arc::new(|rec| matches!(rec.template, Template::Escrow { .. }).then_some(Funding::Treasury)));

    // 4. The contractor invoices the client: 0.01 ZEC on "delivered".
    let escrow = Template::Escrow {
        amount_zat: 1_000_000,
        question: "Logo delivered?".into(),
        payees: vec![],
        maturity_blocks: 10,
        refund_blocks: 300,
    };
    let id = contractor.offer(escrow, &client.identity()).await?;
    println!("offered escrow {id}");

    // 5. Both sides sign every payout and the refund, then the client funds the 2-of-2.
    wait(&client, &id, State::Funded).await?;
    println!("funded");

    // 6. The arbiters rule. On the dev stack the oracles take rulings from an admin token;
    //    in your app this is the arbiter's own tool (or a price oracle that needs no one).
    let token = std::fs::read_to_string(".dev/admin")?.trim().to_string();
    let event = contractor.contract(&id).await?.ok_or("no contract")?.event_id;
    for o in ORACLES {
        zdlc::oracle_client::OracleClient::new(o).resolve(&event, "delivered", &token).await?;
    }
    println!("both arbiters ruled \"delivered\"");

    // 7. The contractor asks for the cooperative close, the client checks it pays exactly
    //    the ruled outcome and co-signs, and the escrow goes out into shielded notes.
    wait(&contractor, &id, State::Settled).await?;
    let s = contractor.contract(&id).await?.and_then(|r| r.settlement).ok_or("no settlement")?;
    println!("settled by {} in block {:?}: {:?}", s.how, s.height, s.txid);
    Ok(())
}

