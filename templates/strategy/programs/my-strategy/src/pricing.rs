//! # Pricing: change this file to make the strategy your own
//!
//! [`quote`] is the only pricing function. `pipeline_quote` calls it to answer quotes and
//! `pipeline_swap` calls it to settle trades, so a quote is always exactly what a swap
//! delivers. Never price a swap any other way.
//!
//! The starting point is a constant-product pool (`x · y = k`) with a fee kept in the
//! pool. Replace the body of [`quote`] with your own formula, auction, oracle-anchored price
//! or anything else. It may read anything in [`Pool`] (add fields there) plus the Clock
//! and Rent sysvars, and it must:
//!
//! - be deterministic: the same pool and trade always give the same answer;
//! - never quote more output than the output side holds;
//! - refuse to price an empty side;
//! - for exact-in, quote `amount_in == trade.amount` (the swap refunds any difference);
//! - for exact-out, quote the least input that delivers `trade.amount`, or set
//!   [`SUPPORTS_EXACT_OUT`] to `false` and return an error.
//!
//! Amounts are measured at the pool's vaults: input is what arrived, output is what
//! leaves. Pipeline's 0.25% fee is charged by the router on top; don't include it here.
//! `cargo test` checks quotes against real swaps through the router with `pipeline-check`.

use anchor_lang::prelude::*;
use pipeline_strategy_interface::{QuoteResult, TradeMode, TradeRequest};

use crate::{Pool, StrategyError};

/// Reported to the router at market creation. `false` means traders can only swap
/// exact-in, and [`quote`] may reject [`TradeMode::ExactOut`].
pub const SUPPORTS_EXACT_OUT: bool = true;

const BPS: u128 = 10_000;

/// Prices `trade` against `pool`. Used by both `pipeline_quote` and `pipeline_swap`.
pub fn quote(pool: &Pool, trade: &TradeRequest) -> Result<QuoteResult> {
    let (reserve_in, reserve_out) = pool.reserves(trade)?;
    // A one-sided pool has no meaningful price: either side could be drained.
    require!(
        reserve_in > 0 && reserve_out > 0,
        StrategyError::InsufficientLiquidity
    );
    match trade.mode {
        TradeMode::ExactIn => Ok(QuoteResult {
            amount_in: trade.amount,
            amount_out: out_given_in(reserve_in, reserve_out, trade.amount, pool.fee_bps),
        }),
        TradeMode::ExactOut => {
            let amount_in = in_given_out(reserve_in, reserve_out, trade.amount, pool.fee_bps)
                .ok_or(StrategyError::InsufficientLiquidity)?;
            Ok(QuoteResult {
                amount_in,
                amount_out: trade.amount,
            })
        }
    }
}

/// `floor(r_out × in × (1 − fee) / (r_in + in × (1 − fee)))`. Rounds down, so the pool
/// never pays out more than the formula allows.
pub fn out_given_in(reserve_in: u64, reserve_out: u64, amount_in: u64, fee_bps: u16) -> u64 {
    let in_after_fee = amount_in as u128 * (BPS - fee_bps as u128);
    let denominator = reserve_in as u128 * BPS + in_after_fee;
    if denominator == 0 {
        return 0;
    }
    // At most `reserve_out`, so it fits in a u64.
    mul_div(reserve_out as u128, in_after_fee, denominator).0 as u64
}

/// The least input for which [`out_given_in`] returns at least `amount_out`, or `None`
/// if the pool can't provide it. Rounds up, in the pool's favour.
pub fn in_given_out(
    reserve_in: u64,
    reserve_out: u64,
    amount_out: u64,
    fee_bps: u16,
) -> Option<u64> {
    if amount_out == 0 {
        return Some(0);
    }
    if amount_out >= reserve_out {
        return None;
    }
    let denominator = (reserve_out - amount_out) as u128 * (BPS - fee_bps as u128);
    let (quotient, remainder) = mul_div(reserve_in as u128 * BPS, amount_out as u128, denominator);
    let rounded_up = quotient.checked_add((remainder > 0) as u128)?;
    u64::try_from(rounded_up).ok()
}

/// `a × b / d` as (quotient, remainder), with a 256-bit product so large pools and
/// trades can't overflow. The quotient saturates at `u128::MAX`; `d` must not be 0.
fn mul_div(a: u128, b: u128, d: u128) -> (u128, u128) {
    let mask = u64::MAX as u128;
    let (a1, a0) = (a >> 64, a & mask);
    let (b1, b0) = (b >> 64, b & mask);
    let (mid, mid_carry) = (a1 * b0).overflowing_add(a0 * b1);
    let (lo, lo_carry) = (a0 * b0).overflowing_add(mid << 64);
    let hi = a1 * b1 + (mid >> 64) + ((mid_carry as u128) << 64) + lo_carry as u128;
    if hi == 0 {
        return (lo / d, lo % d);
    }
    if hi >= d {
        return (u128::MAX, 0);
    }
    // Long division of (hi, lo) by d. The running remainder stays below 2d, so a carry
    // bit plus a wrapping subtraction keeps it exact.
    let mut remainder = hi;
    let mut quotient = 0u128;
    for bit in (0..128).rev() {
        let carry = remainder >> 127;
        remainder = (remainder << 1) | ((lo >> bit) & 1);
        if carry == 1 || remainder >= d {
            remainder = remainder.wrapping_sub(d);
            quotient |= 1 << bit;
        }
    }
    (quotient, remainder)
}

// Fast unit tests of the maths, run on your machine by `cargo test`. Replace them when
// you replace the pricing; the router tests in `tests/` keep working unchanged.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_in_matches_the_formula_and_rounds_down() {
        // 1,000 in at 0.3% against 1,000,000 : 2,000,000.
        // 2,000,000 × 997 / (1,000,000 + 997) = 1992.01..., rounded down.
        assert_eq!(out_given_in(1_000_000, 2_000_000, 1_000, 30), 1_992);
        assert_eq!(out_given_in(1_000_000, 2_000_000, 0, 30), 0);
    }

    #[test]
    fn exact_out_is_the_least_input_that_delivers() {
        for amount_out in [1, 7, 1_992, 1_993, 500_000] {
            let amount_in = in_given_out(1_000_000, 2_000_000, amount_out, 30).unwrap();
            assert!(out_given_in(1_000_000, 2_000_000, amount_in, 30) >= amount_out);
            assert!(out_given_in(1_000_000, 2_000_000, amount_in - 1, 30) < amount_out);
        }
    }

    #[test]
    fn the_pool_cannot_be_emptied() {
        assert_eq!(in_given_out(1_000_000, 2_000_000, 2_000_000, 30), None);
        assert!(out_given_in(1, 2_000_000, u64::MAX, 30) < 2_000_000);
    }
}
