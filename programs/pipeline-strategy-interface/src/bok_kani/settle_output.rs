//! Kani proofs for `settlement::settle_fee_on_output`, the settlement of a trade whose
//! output is the market's fee mint: Pipeline's fee is `ceil(received × 25 / 10,000)`,
//! taken from the output, and the trader's minimum applies to what they keep.
//!
//! Inputs are full-domain `u64` balances, `amount_in` and minimum, and a `u16` integrator
//! rate. Harnesses suffixed `_stubbed` replace `ceil_bps` by its contract proved in
//! `fees.rs`; the others run the real `ceil_bps`.

#[cfg(kani_full)]
use super::fees::ceil_bps_contract;
use crate::fees::pipeline_fee;
use crate::settlement::{settle, settle_fee_on_output, Measured, SettleError};

const D: u128 = 10_000;

fn any_measured() -> Measured {
    Measured {
        input_before: kani::any(),
        input_after: kani::any(),
        output_before: kani::any(),
        output_after: kani::any(),
        vault_before: kani::any(),
        vault_after: kani::any(),
    }
}

/// `pipeline_fee(r) <= r` for every `r`, so taking the fee from what arrived never
/// underflows. Real `ceil_bps`.
#[kani::proof]
#[kani::solver(cadical)]
fn verify_output_fee_never_exceeds_what_arrived() {
    let received: u64 = kani::any();
    assert!(pipeline_fee(received) <= received);
}

/// `settle_fee_on_output` never panics on any input, and refuses only for the reasons
/// `settle` does. Real `ceil_bps`.
#[kani::proof]
#[kani::solver(cadical)]
fn verify_output_settle_total() {
    let m = any_measured();
    let (amount_in, min_out, bps): (u64, u64, u16) = (kani::any(), kani::any(), kani::any());
    if let Err(SettleError::MathOverflow) = settle_fee_on_output(&m, amount_in, min_out, bps) {
        assert!(bps > 10_000);
    }
}

/// On success: the fee is exactly `ceil(received × 25 / 10,000)`, the trader keeps at
/// least the minimum, and the fee base, integrator fee and measured amounts are exactly
/// `settle`'s with no minimum (the input side is unchanged).
fn check_charges_what_arrived(m: &Measured, amount_in: u64, min_out: u64, bps: u16) {
    if let Ok(s) = settle_fee_on_output(m, amount_in, min_out, bps) {
        let fee = s.pipeline_fee as u128;
        let received = s.received as u128;
        assert!(fee * D >= received * 25 && fee * D < received * 25 + D);
        assert!(s.received - s.pipeline_fee >= min_out);
        let base = settle(m, amount_in, 0, bps).expect("same checks, no minimum");
        assert!(s.net_sent == base.net_sent);
        assert!(s.integrator_fee == base.integrator_fee);
        assert!(s.trader_spent == base.trader_spent);
        assert!(s.received == base.received);
        assert!(s.vault_kept == base.vault_kept);
    }
}

/// A refusal for the minimum happens exactly when what the trader would keep is below it.
fn check_minimum_is_on_what_is_kept(m: &Measured, amount_in: u64, min_out: u64, bps: u16) {
    if let Ok(base) = settle(m, amount_in, 0, bps) {
        let kept = base.received - pipeline_fee(base.received);
        let result = settle_fee_on_output(m, amount_in, min_out, bps);
        assert!(result.is_ok() == (kept >= min_out));
    }
}

/// Measured balances, `amount_in` and the minimum below `2^12`, so every product stays
/// small; `bps` is one of the `with_edge_rate!` constants.
fn small_inputs(check: impl Fn(&Measured, u64, u64, u16)) {
    const LIMIT: u64 = 1 << 12;
    let m = any_measured();
    for value in [
        m.input_before,
        m.input_after,
        m.output_before,
        m.output_after,
        m.vault_before,
        m.vault_after,
    ] {
        kani::assume(value < LIMIT);
    }
    let (amount_in, min_out): (u64, u64) = (kani::any(), kani::any());
    kani::assume(amount_in < LIMIT && min_out < LIMIT);
    with_edge_rate!(b => check(&m, amount_in, min_out, b as u16));
}

/// Bounded tier of the fee, minimum and input-side properties. Real `ceil_bps`.
#[kani::proof]
#[kani::solver(cadical)]
fn verify_output_settle_charges_what_arrived_and_keeps_the_minimum_bounded() {
    small_inputs(check_charges_what_arrived);
}

/// Bounded tier of the minimum-on-what-is-kept property. Real `ceil_bps`.
#[kani::proof]
#[kani::solver(cadical)]
fn verify_output_settle_minimum_is_on_what_is_kept_bounded() {
    small_inputs(check_minimum_is_on_what_is_kept);
}

/// Full domain, `ceil_bps` stubbed by its contract.
// Times out over the full domain with Kani (more than 30 minutes); run with
// `RUSTFLAGS="--cfg kani_full"`.
#[cfg(kani_full)]
#[kani::proof]
#[kani::solver(cadical)]
#[kani::stub(crate::fees::ceil_bps, ceil_bps_contract)]
fn verify_output_settle_charges_what_arrived_and_keeps_the_minimum_stubbed() {
    let m = any_measured();
    check_charges_what_arrived(&m, kani::any(), kani::any(), kani::any());
}

/// Full domain, `ceil_bps` stubbed by its contract.
// Times out over the full domain with Kani (more than 30 minutes); run with
// `RUSTFLAGS="--cfg kani_full"`.
#[cfg(kani_full)]
#[kani::proof]
#[kani::solver(cadical)]
#[kani::stub(crate::fees::ceil_bps, ceil_bps_contract)]
fn verify_output_settle_minimum_is_on_what_is_kept_stubbed() {
    let m = any_measured();
    check_minimum_is_on_what_is_kept(&m, kani::any(), kani::any(), kani::any());
}
