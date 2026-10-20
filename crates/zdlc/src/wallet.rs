//! A seed wallet: every key a party uses in contract `i` derives from one 32-byte seed,
//! so a wallet restored from the seed alone can find its contracts (via the recovery
//! record, `recovery`) and re-sign its refunds.

use orchard::keys::{FullViewingKey, PreparedIncomingViewingKey, Scope, SpendingKey};
use secp256k1::SecretKey;
use sha2::{Digest, Sha256};
use zcash_transparent::address::TransparentAddress;

use crate::{
    contract::{Party, PartyInfo},
    keys,
    tx::Coin,
};

#[derive(Clone)]
pub struct Wallet {
    seed: [u8; 32],
}

impl Wallet {
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self { seed }
    }

    fn derive(&self, what: &str, i: u32, attempt: u8) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(b"zdlc/wallet/v1");
        h.update(self.seed);
        h.update(what.as_bytes());
        h.update(i.to_be_bytes());
        h.update([attempt]);
        h.finalize().into()
    }

    fn secp_key(&self, what: &str, i: u32) -> SecretKey {
        (0u8..=255)
            .find_map(|n| SecretKey::from_secret_bytes(self.derive(what, i, n)).ok())
            .expect("a valid key")
    }

    /// The wallet's identity on the desk: it signs RFQs, quotes and envelopes, and the
    /// mailbox encrypts to it.
    pub fn identity_key(&self) -> SecretKey {
        self.secp_key("identity", 0)
    }

    /// The key in contract `i`'s 2-of-2.
    pub fn funding_key(&self, i: u32) -> SecretKey {
        self.secp_key("funding", i)
    }

    /// The key whose P2PKH receives CET and refund payouts in contract `i`.
    pub fn payout_key(&self, i: u32) -> SecretKey {
        self.secp_key("payout", i)
    }

    /// The key holding the coins the wallet funds contract `i` and its record from.
    pub fn coin_key(&self, i: u32) -> SecretKey {
        self.secp_key("coin", i)
    }

    pub fn coin_address(&self, i: u32) -> TransparentAddress {
        keys::p2pkh(&keys::pubkey(&self.coin_key(i)))
    }

    pub fn payout_address(&self, i: u32) -> TransparentAddress {
        keys::p2pkh(&keys::pubkey(&self.payout_key(i)))
    }

    pub fn close_fvk(&self, i: u32) -> FullViewingKey {
        let sk = (0u8..=255)
            .find_map(|n| Option::<SpendingKey>::from(SpendingKey::from_bytes(self.derive("close", i, n))))
            .expect("a valid spending key");
        FullViewingKey::from(&sk)
    }

    /// Contract `i`'s Ironwood address: the cooperative close and the recovery record pay it.
    pub fn close_address(&self, i: u32) -> orchard::Address {
        self.close_fvk(i).address_at(0u32, Scope::External)
    }

    pub fn close_ivk(&self, i: u32) -> PreparedIncomingViewingKey {
        PreparedIncomingViewingKey::new(&self.close_fvk(i).to_ivk(Scope::External))
    }

    /// The party this wallet plays in contract `i`, funding from `inputs` (coins at
    /// `coin_address(i)`).
    pub fn party(&self, i: u32, inputs: Vec<Coin>) -> Party {
        let funding_sk = self.funding_key(i);
        Party {
            funding_sk,
            input_keys: vec![self.coin_key(i); inputs.len()],
            close_fvk: self.close_fvk(i),
            roll_key: None,
            info: PartyInfo {
                funding_pubkey: keys::pubkey(&funding_sk),
                inputs,
                change: self.coin_address(i),
                payout: self.payout_address(i),
                close: self.close_address(i),
            },
        }
    }
}
