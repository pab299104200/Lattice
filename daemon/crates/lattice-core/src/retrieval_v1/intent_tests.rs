use super::intent::{
    classify_intent, IntentDiagnostic, IntentFeatureKind, IntentLabel, MAX_INTENT_TASK_CHARS,
};

fn assert_primary(task: &str, expected: IntentLabel) {
    let classification = classify_intent(task);
    assert_eq!(
        classification.primary_label,
        expected,
        "task `{task}` classified as {:?} with diagnostics {:?}",
        classification.primary_label,
        classification.diagnostics()
    );
}

#[test]
fn test_every_label_has_a_canonical_statement() {
    let cases = [
        (
            "Debug the login failure and inspect the stack trace",
            IntentLabel::Debug,
        ),
        (
            "Refactor the auth pipeline to simplify the branching",
            IntentLabel::Refactor,
        ),
        (
            "Explain how the refresh token flow works",
            IntentLabel::Explain,
        ),
        (
            "Add support for expiring workspace handles",
            IntentLabel::AddFeature,
        ),
        (
            "Modify the ranking weights in retrieval_v1/score.rs",
            IntentLabel::ModifyFeature,
        ),
        (
            "Add a regression test for expired-handle retries",
            IntentLabel::AddTest,
        ),
        (
            "Update docs/README.md to clarify the handle lifecycle",
            IntentLabel::UpdateDocs,
        ),
        (
            "Create a migration to backfill workspace_identity rows",
            IntentLabel::Migration,
        ),
        (
            "Optimize the candidate ranking hot path for lower latency",
            IntentLabel::Performance,
        ),
        (
            "Review this diff for regression risk and missing tests",
            IntentLabel::Review,
        ),
        ("   ", IntentLabel::Unknown),
    ];

    for (task, expected) in cases {
        assert_primary(task, expected);
    }
}

#[test]
fn test_ambiguous_tasks_return_multiple_labels_with_documented_tie_break() {
    let classification = classify_intent(
        "Debug the login failure and add a regression test for refresh-session retries",
    );

    assert_eq!(classification.primary_label, IntentLabel::Debug);
    assert!(
        classification
            .secondary_labels
            .contains(&IntentLabel::AddTest),
        "expected add_test secondary label, got {:?}",
        classification.secondary_labels
    );
}

#[test]
fn test_empty_or_whitespace_input_returns_unknown() {
    for task in ["", "   ", "\n\t"] {
        let classification = classify_intent(task);
        assert_eq!(classification.primary_label, IntentLabel::Unknown);
        assert!(classification.fired_features.is_empty());
    }
}

#[test]
fn test_long_input_truncates_tail_without_losing_leading_verb() {
    let filler = "x".repeat(MAX_INTENT_TASK_CHARS + 200);
    let task = format!("Explain the ranking diagnostics {filler}");
    let classification = classify_intent(&task);

    assert_eq!(classification.primary_label, IntentLabel::Explain);
    assert!(classification.input_was_truncated);
    assert_eq!(
        classification.inspected_text.chars().count(),
        MAX_INTENT_TASK_CHARS
    );
}

#[test]
fn test_diagnostics_round_trip_through_serde() {
    let classification =
        classify_intent("Update docs/README.md and review the migration notes for regressions");
    let diagnostics = classification.diagnostics();

    let serialized = serde_json::to_string(&diagnostics).expect("serialize diagnostics");
    let restored: Vec<IntentDiagnostic> =
        serde_json::from_str(&serialized).expect("deserialize diagnostics");

    assert_eq!(diagnostics, restored);
}

#[test]
fn test_feature_kinds_remain_inspectable() {
    let classification = classify_intent("How does loginUser() fail in src/auth.ts?");
    let fired_kinds: Vec<IntentFeatureKind> = classification
        .fired_features
        .iter()
        .map(|feature| feature.kind)
        .collect();

    assert!(fired_kinds.contains(&IntentFeatureKind::QuestionForm));
    assert!(fired_kinds.contains(&IntentFeatureKind::FileTokenShape));
    assert!(fired_kinds.contains(&IntentFeatureKind::SymbolTokenShape));
}
