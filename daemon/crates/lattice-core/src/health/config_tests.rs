use super::*;

#[test]
fn active_config_reports_its_version() {
    let config = active_config();
    assert_eq!(config.version, HEALTH_CONFIG_VERSION);
    assert_eq!(config.version, 1);
}

#[test]
fn version_one_thresholds_are_uniform_across_languages() {
    let config = active_config();
    let languages = [
        Language::Rust,
        Language::Python,
        Language::Go,
        Language::Java,
        Language::TypeScript,
        Language::JavaScript,
        Language::Markdown,
        Language::Unknown,
    ];
    for language in languages {
        assert_eq!(
            config.complexity_for(language),
            &config.complexity,
            "language {language:?} must read the shared v1 table"
        );
    }
}

#[test]
fn thresholds_are_the_documented_v1_values() {
    let thresholds = active_config().complexity;
    assert_eq!(thresholds.high_cyclomatic_complexity, 10);
    assert_eq!(thresholds.high_function_length, 60);
    assert_eq!(thresholds.high_nesting_depth, 4);
    assert_eq!(thresholds.high_param_count, 5);
    assert_eq!(active_config().rollup_percentile_per_mille, 900);
}
