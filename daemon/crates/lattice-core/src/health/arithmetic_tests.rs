use super::{per_mille, round_div};

#[test]
fn round_div_rounds_halves_up() {
    assert_eq!(round_div(1, 2), 1);
    assert_eq!(round_div(3, 2), 2);
    assert_eq!(round_div(5, 2), 3);
}

#[test]
fn round_div_truncates_below_half() {
    assert_eq!(round_div(4, 3), 1);
    assert_eq!(round_div(2, 3), 1);
    assert_eq!(round_div(1, 3), 0);
}

#[test]
fn round_div_by_zero_is_zero_not_a_panic() {
    assert_eq!(round_div(17, 0), 0);
}

#[test]
fn per_mille_matches_the_expected_scale() {
    assert_eq!(per_mille(1, 1), 1000);
    assert_eq!(per_mille(0, 7), 0);
    assert_eq!(per_mille(1, 3), 333);
    assert_eq!(per_mille(2, 3), 667);
}

#[test]
fn per_mille_does_not_overflow_on_large_populations() {
    assert_eq!(per_mille(u64::MAX, u64::MAX), 1000);
}

#[test]
fn per_mille_by_zero_is_zero_not_a_panic() {
    assert_eq!(per_mille(9, 0), 0);
}
