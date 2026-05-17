use crate::retrieval_v1::AnchorResolution;

use super::test_support::{
    build_fixture, execute_case, golden_case, golden_cases, is_stale_memory, stale_memory_rank,
    top_identity_matches, verified_memory_rank, MEMORY_AUTH_VERIFIED,
};

#[tokio::test]
async fn every_anchor_type_has_three_golden_tasks() {
    let corpus = golden_cases();

    assert_eq!(
        count_kind(&corpus, crate::retrieval_v1::AnchorKind::Path),
        3
    );
    assert_eq!(
        count_kind(&corpus, crate::retrieval_v1::AnchorKind::Symbol),
        4
    );
    assert_eq!(
        count_kind(&corpus, crate::retrieval_v1::AnchorKind::Error),
        3
    );
    assert_eq!(
        count_kind(&corpus, crate::retrieval_v1::AnchorKind::Command),
        3
    );
    assert_eq!(count_kind(&corpus, crate::retrieval_v1::AnchorKind::Api), 3);
    assert_eq!(
        count_kind(&corpus, crate::retrieval_v1::AnchorKind::ConfigKey),
        3
    );
}

#[tokio::test]
async fn stale_memory_ranks_below_trusted_result() {
    let fixture = build_fixture();
    let run = execute_case(
        &golden_case("symbol_login_user_anchor_returns_login_user"),
        &fixture,
    )
    .await;

    assert!(
        stale_memory_rank(&run).unwrap()
            > verified_memory_rank(&run, MEMORY_AUTH_VERIFIED).unwrap()
    );
    assert!(stale_memory_rank(&run).unwrap() > 1);
}

#[tokio::test]
async fn budget_truncation_preserves_pinned_result_and_token_cap() {
    let fixture = build_fixture();
    let case = golden_case("budget_login_user_anchor_keeps_pinned_symbol_under_truncation");
    let run = execute_case(&case, &fixture).await;

    assert!(run.bundle.budget_report.truncated);
    assert!(top_identity_matches(&case, &run));
    assert!(run.bundle.budget_report.estimated_tokens <= case.shaper_token_budget);
}

macro_rules! golden_case_test {
    ($test_name:ident, $case_name:literal) => {
        #[tokio::test]
        async fn $test_name() {
            assert_case_contract($case_name).await;
        }
    };
}

golden_case_test!(
    path_auth_file_anchor_returns_auth_file,
    "path_auth_file_anchor_returns_auth_file"
);
golden_case_test!(
    path_session_file_anchor_returns_session_file,
    "path_session_file_anchor_returns_session_file"
);
golden_case_test!(
    path_section_anchor_returns_retrieval_engine_section,
    "path_section_anchor_returns_retrieval_engine_section"
);
golden_case_test!(
    symbol_login_user_anchor_returns_login_user,
    "symbol_login_user_anchor_returns_login_user"
);
golden_case_test!(
    symbol_refresh_session_anchor_returns_refresh_session,
    "symbol_refresh_session_anchor_returns_refresh_session"
);
golden_case_test!(
    symbol_search_logic_flow_anchor_returns_search_logic_flow,
    "symbol_search_logic_flow_anchor_returns_search_logic_flow"
);
golden_case_test!(
    error_panic_path_anchor_returns_auth_file,
    "error_panic_path_anchor_returns_auth_file"
);
golden_case_test!(
    error_missing_symbol_anchor_returns_login_user,
    "error_missing_symbol_anchor_returns_login_user"
);
golden_case_test!(
    error_traceback_anchor_returns_cli_file,
    "error_traceback_anchor_returns_cli_file"
);
golden_case_test!(
    command_rg_anchor_returns_login_user,
    "command_rg_anchor_returns_login_user"
);
golden_case_test!(
    command_cargo_test_anchor_returns_session_file,
    "command_cargo_test_anchor_returns_session_file"
);
golden_case_test!(
    command_python_anchor_returns_cli_file,
    "command_python_anchor_returns_cli_file"
);
golden_case_test!(
    api_prepare_change_anchor_returns_prepare_change,
    "api_prepare_change_anchor_returns_prepare_change"
);
golden_case_test!(
    api_diagnose_failure_anchor_returns_diagnose_failure,
    "api_diagnose_failure_anchor_returns_diagnose_failure"
);
golden_case_test!(
    api_search_logic_flow_anchor_returns_search_logic_flow,
    "api_search_logic_flow_anchor_returns_search_logic_flow"
);
golden_case_test!(
    config_index_root_anchor_returns_index_root_symbol,
    "config_index_root_anchor_returns_index_root_symbol"
);
golden_case_test!(
    config_event_log_anchor_returns_event_log_symbol,
    "config_event_log_anchor_returns_event_log_symbol"
);
golden_case_test!(
    config_workflow_cache_ttl_anchor_returns_workflow_cache_ttl,
    "config_workflow_cache_ttl_anchor_returns_workflow_cache_ttl"
);
golden_case_test!(
    budget_login_user_anchor_keeps_pinned_symbol_under_truncation,
    "budget_login_user_anchor_keeps_pinned_symbol_under_truncation"
);

async fn assert_case_contract(case_name: &str) {
    let fixture = build_fixture();
    let case = golden_case(case_name);
    let run = execute_case(&case, &fixture).await;
    let anchor = run
        .anchors
        .iter()
        .find(|anchor| {
            anchor.kind == case.anchor_kind && anchor.anchor_text == case.expected_anchor_text
        })
        .unwrap_or_else(|| panic!("missing anchor `{}`", case.expected_anchor_text));

    match &anchor.resolution {
        AnchorResolution::Resolved(identity) => {
            assert!(
                case.expected_resolved_identity.matches(identity),
                "expected anchor `{}` to resolve to {}, got {}",
                case.expected_anchor_text,
                case.expected_resolved_identity.label(),
                identity
            );
        }
        other => panic!("expected resolved anchor, got {other:?}"),
    }
    assert!(
        top_identity_matches(&case, &run),
        "expected top result {}, got {}",
        case.expected_top_identity.label(),
        run.ranked
            .first()
            .map(|candidate| candidate.candidate.identity.to_string())
            .unwrap_or_else(|| "<none>".to_string())
    );
    assert!(!run.bundle.results[0].inclusion_reason.contains('\n'));
    assert!(run.bundle.results[0]
        .inclusion_reason
        .contains(case.dominant_signal));
    assert_runner_up_is_not_stale(&run);
}

fn assert_runner_up_is_not_stale(run: &super::test_support::GoldenRun) {
    if let Some(candidate) = run.ranked.get(1) {
        assert!(!is_stale_memory(&candidate.candidate.identity));
    }
}

fn count_kind(
    corpus: &[super::test_support::GoldenCase],
    kind: crate::retrieval_v1::AnchorKind,
) -> usize {
    corpus
        .iter()
        .filter(|case| case.anchor_kind == kind)
        .count()
}
