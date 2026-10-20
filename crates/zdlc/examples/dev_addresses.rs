//! Prints the regtest miner address that `infra/zakura/regtest.toml` pays coinbase to.
fn main() {
    let pk = zdlc::keys::pubkey(&zdlc::keys::miner());
    println!("{}", zdlc::keys::encode_regtest(&zdlc::keys::p2pkh(&pk)));
}
