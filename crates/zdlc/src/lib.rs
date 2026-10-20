//! zdlc: Discreet Log Contracts on Zcash.
//!
//! Every transaction is v6, built through the `pczt` roles. A contract is one 2-of-2
//! P2SH funding output, one CET per outcome (ECDSA adaptor-signed to the oracle's
//! attestation point), a refund locked to a height, and a cooperative close that pays
//! Ironwood. See `docs/design.md`.

pub mod adaptor;
pub mod contract;
pub mod keys;
pub mod node;
pub mod oracle;
pub mod params;
pub mod payout;
pub mod script;
pub mod tx;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("build: {0}")]
    Build(String),
    #[error("pczt: {0}")]
    Pczt(String),
    #[error("crypto: {0}")]
    Crypto(String),
    #[error("rpc: {0}")]
    Rpc(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Display-order hex of an internal-order txid (what RPCs print).
pub fn txid_hex(internal: &[u8; 32]) -> String {
    let mut b = *internal;
    b.reverse();
    hex::encode(b)
}

/// Internal-order txid from display-order hex.
pub fn txid_from_hex(display: &str) -> Result<[u8; 32]> {
    let mut b: [u8; 32] = hex::decode(display)
        .map_err(|e| Error::Rpc(format!("txid hex: {e}")))?
        .try_into()
        .map_err(|_| Error::Rpc("txid length".into()))?;
    b.reverse();
    Ok(b)
}
