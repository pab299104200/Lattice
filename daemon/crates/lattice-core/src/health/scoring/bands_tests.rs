use super::*;

#[test]
fn the_cutoffs_sit_where_the_calibration_table_steps() {
    // § "Calibration by decile", graph+git+complexity: 2.1% -> 3.8% at 0.300,
    // 12.7% -> 25.7% at 0.600, 27.2% -> 41.6% at 0.800.
    assert_eq!(MODERATE_CUTOFF_PER_MILLE, 300);
    assert_eq!(HIGH_CUTOFF_PER_MILLE, 600);
    assert_eq!(CRITICAL_CUTOFF_PER_MILLE, 800);
}

#[test]
fn every_score_lands_in_exactly_one_band() {
    assert_eq!(Band::from_score_per_mille(0), Band::Low);
    assert_eq!(Band::from_score_per_mille(299), Band::Low);
    assert_eq!(Band::from_score_per_mille(300), Band::Moderate);
    assert_eq!(Band::from_score_per_mille(599), Band::Moderate);
    assert_eq!(Band::from_score_per_mille(600), Band::High);
    assert_eq!(Band::from_score_per_mille(799), Band::High);
    assert_eq!(Band::from_score_per_mille(800), Band::Critical);
    assert_eq!(Band::from_score_per_mille(1000), Band::Critical);
}

#[test]
fn bands_order_from_low_to_critical() {
    assert!(Band::Low < Band::Moderate);
    assert!(Band::Moderate < Band::High);
    assert!(Band::High < Band::Critical);
}

#[test]
fn observed_rates_rise_with_the_band() {
    let mut previous = 0;
    for band in ALL_BANDS {
        let rate = band.observed_defect_rate_per_mille();
        assert!(
            rate > previous,
            "{}: calibration must rise monotonically",
            band.as_str()
        );
        previous = rate;
    }
}

#[test]
fn the_lowest_band_sits_below_the_corpus_prevalence() {
    // Prevalence is 2.5% (25 per-mille) in the report's provenance table.
    assert!(Band::Low.observed_defect_rate_per_mille() < 25);
    assert!(Band::Moderate.observed_defect_rate_per_mille() > 25);
}

#[test]
fn an_exact_range_renders_as_one_band() {
    let range = BandRange::exact(Band::High);

    assert!(range.is_exact());
    assert_eq!(range.label(), "high");
}

#[test]
fn a_widened_range_renders_both_ends_lowest_first() {
    let range = BandRange::new(Band::Critical, Band::Moderate);

    assert!(!range.is_exact());
    assert_eq!(range.floor, Band::Moderate);
    assert_eq!(range.ceiling, Band::Critical);
    assert_eq!(range.label(), "moderate-critical");
}

#[test]
fn band_codes_round_trip() {
    for band in ALL_BANDS {
        assert_eq!(Band::from_code(band.as_str()), Some(band));
    }
    assert_eq!(Band::from_code("extreme"), None);
}
