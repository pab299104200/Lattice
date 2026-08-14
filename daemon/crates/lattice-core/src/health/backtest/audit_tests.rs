//! Tests for the H1.2 label-quality audit.

use super::*;

fn commit(id: &str, subject: &str, paths: &[&str]) -> AuditInput {
    AuditInput {
        id: id.to_owned(),
        subject: subject.to_owned(),
        paths: paths.iter().map(|path| (*path).to_owned()).collect(),
    }
}

#[test]
fn co_modification_requires_both_a_test_and_a_production_file() {
    assert!(touches_test_and_production(&[
        "src/lib.rs".to_owned(),
        "tests/lib_test.rs".to_owned()
    ]));
    // Only production: no evidence either way.
    assert!(!touches_test_and_production(&["src/lib.rs".to_owned()]));
    // Only tests: likewise.
    assert!(!touches_test_and_production(&[
        "tests/lib_test.rs".to_owned()
    ]));
    assert!(!touches_test_and_production(&[]));
}

#[test]
fn a_perfectly_enriched_classifier_is_corroborated() {
    // Every fix-shaped commit co-modifies; no other commit does.
    let mut commits = Vec::new();
    for index in 0..10 {
        commits.push(commit(
            &format!("f{index:02}"),
            "fix: repair something",
            &["src/lib.rs", "tests/lib_test.rs"],
        ));
        commits.push(commit(
            &format!("o{index:02}"),
            "feat: build something",
            &["src/lib.rs"],
        ));
    }
    let audit = audit_labels(&commits, 50);

    assert_eq!(audit.commits_considered, 20);
    assert_eq!(audit.classified_fix, 10);
    assert_eq!(audit.both, 10);
    assert_eq!(audit.fix_only, 0);
    assert_eq!(audit.signal_only, 0);
    assert_eq!(audit.neither, 10);
    assert_eq!(audit.percent_agreement_per_mille, 1000);
    assert_eq!(audit.cohen_kappa_per_mille, 1000);
    // Half the commits co-modify; all the fix-shaped ones do.
    assert_eq!(audit.co_modification_rate_per_mille, 500);
    assert_eq!(audit.co_modification_given_fix_per_mille, 1000);
    assert_eq!(audit.enrichment_per_mille, 2000);
    assert_eq!(audit.verdict, AuditVerdict::Corroborated);
    assert!(!audit.verdict.warrants_tightening());
}

#[test]
fn a_classifier_that_tracks_nothing_is_uncorroborated_and_warrants_tightening() {
    // Co-modification is spread identically across both classes, so knowing a
    // commit is fix-shaped tells you nothing about it.
    let mut commits = Vec::new();
    for index in 0..10 {
        let co_modifying = index % 2 == 0;
        let paths: &[&str] = if co_modifying {
            &["src/lib.rs", "tests/lib_test.rs"]
        } else {
            &["src/lib.rs"]
        };
        commits.push(commit(&format!("f{index:02}"), "fix: something", paths));
        commits.push(commit(&format!("o{index:02}"), "feat: something", paths));
    }
    let audit = audit_labels(&commits, 50);

    assert_eq!(audit.co_modification_given_fix_per_mille, 500);
    assert_eq!(audit.co_modification_given_non_fix_per_mille, 500);
    assert_eq!(audit.enrichment_per_mille, 1000);
    assert_eq!(audit.verdict, AuditVerdict::Uncorroborated);
    assert!(audit.verdict.warrants_tightening());
    // Kappa is zero because the signals are independent — and note this is a
    // *different* fact from the enrichment being 1000.
    assert_eq!(audit.cohen_kappa_per_mille, 0);
}

#[test]
fn slight_enrichment_is_reported_as_weak_rather_than_rounded_to_a_verdict() {
    // Fix-shaped commits co-modify 6/10; others 5/10. Base rate 11/20 = 550;
    // enrichment 600/550 = 1091 per-mille, inside the weak band.
    let mut commits = Vec::new();
    for index in 0..10 {
        commits.push(commit(
            &format!("f{index:02}"),
            "fix: something",
            if index < 6 {
                &["src/lib.rs", "tests/lib_test.rs"]
            } else {
                &["src/lib.rs"]
            },
        ));
        commits.push(commit(
            &format!("o{index:02}"),
            "feat: something",
            if index < 5 {
                &["src/lib.rs", "tests/lib_test.rs"]
            } else {
                &["src/lib.rs"]
            },
        ));
    }
    let audit = audit_labels(&commits, 50);
    assert_eq!(audit.co_modification_rate_per_mille, 550);
    assert_eq!(audit.co_modification_given_fix_per_mille, 600);
    assert_eq!(audit.enrichment_per_mille, 1091);
    assert_eq!(audit.verdict, AuditVerdict::Weak);
    assert!(!audit.verdict.warrants_tightening());
}

#[test]
fn an_audit_with_no_commit_of_one_class_reports_unavailable() {
    // Every commit is fix-shaped: there is nothing to contrast against.
    let commits: Vec<AuditInput> = (0..5)
        .map(|index| {
            commit(
                &format!("f{index:02}"),
                "fix: something",
                &["src/lib.rs", "tests/lib_test.rs"],
            )
        })
        .collect();
    assert_eq!(audit_labels(&commits, 50).verdict, AuditVerdict::Unavailable);

    // No commit co-modifies at all: the independent signal is silent, which is
    // not the same as the classifier failing.
    let silent: Vec<AuditInput> = (0..5)
        .map(|index| {
            commit(
                &format!("c{index:02}"),
                if index % 2 == 0 { "fix: a" } else { "feat: b" },
                &["src/lib.rs"],
            )
        })
        .collect();
    assert_eq!(audit_labels(&silent, 50).verdict, AuditVerdict::Unavailable);

    assert_eq!(audit_labels(&[], 50).verdict, AuditVerdict::Unavailable);
}

#[test]
fn agreement_worse_than_chance_reports_a_negative_kappa() {
    // Deliberately anti-correlated: fix-shaped commits never co-modify, and
    // every other commit does.
    let mut commits = Vec::new();
    for index in 0..10 {
        commits.push(commit(
            &format!("f{index:02}"),
            "fix: something",
            &["src/lib.rs"],
        ));
        commits.push(commit(
            &format!("o{index:02}"),
            "feat: something",
            &["src/lib.rs", "tests/lib_test.rs"],
        ));
    }
    let audit = audit_labels(&commits, 50);
    assert_eq!(audit.percent_agreement_per_mille, 0);
    assert_eq!(audit.cohen_kappa_per_mille, -1000);
    assert_eq!(audit.enrichment_per_mille, 0);
    assert_eq!(audit.verdict, AuditVerdict::Uncorroborated);
}

#[test]
fn duplicate_commits_across_windows_are_counted_once() {
    let commits = vec![
        commit("aaa", "fix: one", &["src/lib.rs", "tests/lib_test.rs"]),
        commit("aaa", "fix: one", &["src/lib.rs", "tests/lib_test.rs"]),
        commit("bbb", "feat: two", &["src/lib.rs"]),
    ];
    let audit = audit_labels(&commits, 50);
    assert_eq!(audit.commits_considered, 2);
    assert_eq!(audit.classified_fix, 1);
}

#[test]
fn the_sample_is_balanced_between_the_two_classes() {
    let mut commits = Vec::new();
    for index in 0..100 {
        commits.push(commit(
            &format!("f{index:03}"),
            &format!("fix: number {index}"),
            &["src/lib.rs"],
        ));
        commits.push(commit(
            &format!("o{index:03}"),
            &format!("feat: number {index}"),
            &["src/lib.rs", "tests/lib_test.rs"],
        ));
    }
    let audit = audit_labels(&commits, DEFAULT_SAMPLE_SIZE);

    assert_eq!(audit.sample.len(), DEFAULT_SAMPLE_SIZE);
    let fixes = audit
        .sample
        .iter()
        .filter(|entry| entry.classified_fix)
        .count();
    assert_eq!(fixes, DEFAULT_SAMPLE_SIZE / 2);
    // Distinct subjects: the sample spans the class rather than repeating one
    // commit to fill the quota.
    let mut subjects: Vec<&str> = audit
        .sample
        .iter()
        .map(|entry| entry.subject.as_str())
        .collect();
    subjects.sort_unstable();
    subjects.dedup();
    assert_eq!(subjects.len(), DEFAULT_SAMPLE_SIZE);
}

#[test]
fn a_class_smaller_than_its_quota_contributes_everything_it_has() {
    let mut commits = vec![commit("f00", "fix: the only fix", &["src/lib.rs"])];
    for index in 0..40 {
        commits.push(commit(
            &format!("o{index:03}"),
            &format!("feat: number {index}"),
            &["src/lib.rs"],
        ));
    }
    let audit = audit_labels(&commits, DEFAULT_SAMPLE_SIZE);
    let fixes = audit
        .sample
        .iter()
        .filter(|entry| entry.classified_fix)
        .count();
    assert_eq!(fixes, 1);
    assert_eq!(audit.sample.len(), 1 + DEFAULT_SAMPLE_SIZE / 2);
}

#[test]
fn the_audit_is_independent_of_input_order() {
    let mut commits = Vec::new();
    for index in 0..20 {
        commits.push(commit(
            &format!("c{index:03}"),
            if index % 3 == 0 { "fix: a" } else { "feat: b" },
            if index % 2 == 0 {
                &["src/lib.rs", "tests/lib_test.rs"]
            } else {
                &["src/lib.rs"]
            },
        ));
    }
    let forward = audit_labels(&commits, DEFAULT_SAMPLE_SIZE);
    commits.reverse();
    let reversed = audit_labels(&commits, DEFAULT_SAMPLE_SIZE);
    assert_eq!(forward, reversed);
}

#[test]
fn commit_ids_are_abbreviated_for_report_text() {
    let commits = vec![
        commit(
            "0123456789abcdef0123456789abcdef01234567",
            "fix: long id",
            &["src/lib.rs", "tests/lib_test.rs"],
        ),
        commit("fedcba9876543210", "feat: other", &["src/lib.rs"]),
    ];
    let audit = audit_labels(&commits, 2);
    assert_eq!(audit.sample[0].commit, "0123456789");
}
