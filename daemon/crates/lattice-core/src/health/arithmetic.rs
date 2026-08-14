//! Integer arithmetic shared by every health module.
//!
//! The health engine states fractions in per-mille and never in floating point
//! (spec "Design decisions" 3), so that a fact, a percentile, and a score are
//! byte-identical on every platform. The rounding rule lives here rather than
//! in each module so that the backtest harness (H1), the fact producers (H2),
//! and the score engine (H3) cannot drift apart by a unit in the last place —
//! which would silently invalidate the weights H1 derived for H3.

/// Round `numerator / denominator` to the nearest integer, halves away from
/// zero (both operands are unsigned, so halves up).
///
/// A zero denominator yields zero: callers that must distinguish "no
/// population" from "zero" check that before calling, because an absent value
/// is never a zero value in this engine.
pub fn round_div(numerator: u128, denominator: u128) -> u128 {
    if denominator == 0 {
        return 0;
    }
    (numerator * 2 + denominator) / (denominator * 2)
}

/// `numerator / denominator` expressed in per-mille, rounded to nearest.
///
/// A zero denominator yields zero for the same reason as [`round_div`].
pub fn per_mille(numerator: u64, denominator: u64) -> u32 {
    round_div(u128::from(numerator) * 1000, u128::from(denominator)) as u32
}

#[cfg(test)]
#[path = "arithmetic_tests.rs"]
mod tests;
