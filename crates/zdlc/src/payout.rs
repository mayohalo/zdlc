//! The stable-value payout curve, discretized into ranges of equal payout and covered by
//! digit prefixes (one adaptor signature per prefix).
//!
//! The recipient's ZEC `q_r` and a hedger's `q_h` fund the contract. At price `p` the
//! recipient receives ZEC worth `D` dollars, capped by the pot:
//! `recipient(p) = min(q_r + q_h, D / p)`; the hedger receives the rest.

use dlc_trie::digit_decomposition::group_by_ignoring_digits;

/// Binary digits only: base 2 keeps one nonce per bit.
pub const BASE: usize = 2;

#[derive(Clone, Debug)]
pub struct StableValue {
    /// The dollar amount the recipient is owed, in cents.
    pub d_cents: u64,
    pub q_recipient: u64,
    pub q_hedger: u64,
    /// Price per oracle unit, in cents: outcome `v` means price `v * unit_cents`.
    pub unit_cents: u64,
    pub nb_digits: usize,
    /// Payouts are rounded to this many zatoshis; wider rounding, fewer CETs.
    pub rounding: u64,
}

/// Outcomes `start..=end` all pay the recipient `recipient` zatoshis.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Range {
    pub start: usize,
    pub end: usize,
    pub recipient: u64,
}

impl StableValue {
    pub fn total(&self) -> u64 {
        self.q_recipient + self.q_hedger
    }

    pub fn max_outcome(&self) -> usize {
        (1 << self.nb_digits) - 1
    }

    /// The exact (unrounded) recipient payout at outcome `v`.
    pub fn exact(&self, v: usize) -> u64 {
        let price = v as u128 * self.unit_cents as u128;
        if price == 0 {
            return self.total();
        }
        let zat = self.d_cents as u128 * 100_000_000 / price;
        zat.min(self.total() as u128) as u64
    }

    /// The payout the contract pays at outcome `v`: rounded to `rounding`, capped.
    pub fn at(&self, v: usize) -> u64 {
        let e = self.exact(v.min(self.max_outcome()));
        let r = self.rounding.max(1);
        (((e + r / 2) / r) * r).min(self.total())
    }

    /// Consecutive outcomes with equal payouts, covering `0..=max_outcome`.
    pub fn ranges(&self) -> Vec<Range> {
        let mut out: Vec<Range> = vec![];
        for v in 0..=self.max_outcome() {
            let p = self.at(v);
            match out.last_mut() {
                Some(r) if r.recipient == p => r.end = v,
                _ => out.push(Range {
                    start: v,
                    end: v,
                    recipient: p,
                }),
            }
        }
        out
    }

    /// The protection floor: the lowest price, in cents, at which the recipient is still
    /// paid in full (`D / pot`).
    pub fn floor_cents(&self) -> u64 {
        self.d_cents * 100_000_000 / self.total()
    }
}

/// The digit prefixes that cover a range: each becomes one adaptor signature.
pub fn prefixes(r: &Range, nb_digits: usize) -> Vec<Vec<usize>> {
    group_by_ignoring_digits(r.start, r.end, BASE, nb_digits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn curve() -> StableValue {
        StableValue {
            d_cents: 100_00,
            q_recipient: 2_0000_0000,
            q_hedger: 2_0000_0000,
            unit_cents: 10,
            nb_digits: 12,
            rounding: 800_0000,
        }
    }

    #[test]
    fn shape_and_coverage() {
        let c = curve();
        // $50: 2 ZEC. $40: 2.5 ZEC. $20 is below the $25 floor: the whole 4 ZEC pot.
        assert_eq!(c.at(500), 2_0000_0000);
        assert_eq!(c.at(400), 2_4800_0000); // 2.5 rounded to the 0.08 grid
        assert_eq!(c.at(200), 4_0000_0000);
        assert_eq!(c.floor_cents(), 25_00);
        let ranges = c.ranges();
        assert_eq!(ranges.first().unwrap().start, 0);
        assert_eq!(ranges.last().unwrap().end, c.max_outcome());
        let mut covered = vec![0u8; c.max_outcome() + 1];
        let mut sigs = 0;
        for r in &ranges {
            for p in prefixes(r, c.nb_digits) {
                sigs += 1;
                let shift = c.nb_digits - p.len();
                let base = p.iter().fold(0usize, |a, d| a * 2 + d) << shift;
                for v in base..base + (1 << shift) {
                    assert_eq!(c.at(v), r.recipient);
                    covered[v] += 1;
                }
            }
        }
        assert!(covered.iter().all(|&n| n == 1), "every outcome covered exactly once");
        println!("{} CETs, {} adaptor signatures", ranges.len(), sigs);
    }
}
