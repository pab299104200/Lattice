use super::*;
use crate::health::config::{ComplexityThresholds, HealthConfig};

/// Look up one unit by name, failing loudly with the produced set on a miss.
fn unit<'a>(facts: &'a FileComplexityFacts, name: &str) -> &'a SymbolComplexityFacts {
    facts
        .symbols
        .iter()
        .find(|unit| unit.symbol == name)
        .unwrap_or_else(|| {
            panic!(
                "no unit named {name}; produced: {:?}",
                facts
                    .symbols
                    .iter()
                    .map(|unit| unit.symbol.as_str())
                    .collect::<Vec<_>>()
            )
        })
}

/// Assert the full complexity shape of one function unit.
fn assert_unit(
    facts: &FileComplexityFacts,
    name: &str,
    complexity: u32,
    nesting: u32,
    params: Option<u32>,
) {
    let unit = unit(facts, name);
    assert_eq!(
        unit.cyclomatic_complexity, complexity,
        "{name}: cyclomatic complexity"
    );
    assert_eq!(
        unit.branch_count,
        complexity - 1,
        "{name}: complexity must be 1 + branch_count"
    );
    assert_eq!(unit.max_nesting_depth, nesting, "{name}: nesting depth");
    assert_eq!(unit.param_count, params, "{name}: parameter count");
}

/// A file's facts must be reproducible byte-for-byte from the same input.
fn assert_deterministic(file: &str, source: &str) {
    let first = compute_file_complexity_facts(file, source);
    let second = compute_file_complexity_facts(file, source);
    assert_eq!(first, second, "{file}: facts must be deterministic");

    let encoded = serde_json::to_string(&first).expect("facts serialize");
    let decoded: FileComplexityFacts = serde_json::from_str(&encoded).expect("facts deserialize");
    assert_eq!(first, decoded, "{file}: facts must survive a round trip");
}

/// Complexity units must key to the symbols the language parser extracts.
fn assert_joins_to_symbols(file: &str, source: &str, expected: &[&str]) {
    let parsed = crate::parser::parse_file(file, source).expect("file parses");
    let facts = compute_file_complexity_facts(file, source);

    for name in expected {
        let unit = unit(&facts, name);
        let symbol = parsed
            .symbols
            .iter()
            .find(|symbol| symbol.name == *name)
            .unwrap_or_else(|| panic!("parser extracted no symbol named {name}"));
        assert_eq!(
            unit.byte_offset, symbol.id.byte_offset,
            "{name}: complexity facts must join to the symbol on (file, byte_offset)"
        );
        assert_eq!(unit.line, symbol.line, "{name}: start line");
        assert_eq!(unit.end_line, symbol.end_line, "{name}: end line");
    }
}

// ---------------------------------------------------------------------------
// Rust
// ---------------------------------------------------------------------------

const RUST_SAMPLE: &str = r#"pub fn straight_line(a: u32) -> u32 {
    a + 1
}

pub fn three_ifs(a: u32, b: u32, c: u32) -> u32 {
    if a > 0 { return 1; }
    if b > 0 { return 2; }
    if c > 0 { return 3; }
    0
}

pub fn nested(a: u32, b: u32) -> u32 {
    if a > 0 {
        for i in 0..a {
            if i > b {
                return i;
            }
        }
    } else if b > 0 {
        return b;
    }
    0
}

pub fn logic(a: bool, b: bool, c: bool) -> bool {
    if a && b || c {
        true
    } else {
        false
    }
}

pub fn matcher(a: u32, b: bool) -> u32 {
    match a {
        0 => 1,
        1 if b => 2,
        _ => 3,
    }
}

pub fn fallible(v: Result<u32, ()>) -> Result<u32, ()> {
    let x = v?;
    Ok(x)
}

pub fn loops(a: u32) -> u32 {
    let mut total = 0;
    while total < a {
        total += 1;
    }
    loop {
        break;
    }
    total
}

pub fn with_closure(values: Vec<u32>) -> Vec<u32> {
    values
        .into_iter()
        .map(|v| if v > 0 { v } else { 0 })
        .collect()
}

pub struct Widget;

impl Widget {
    pub fn method(&self, a: u32) -> u32 {
        if a > 0 {
            1
        } else {
            0
        }
    }
}
"#;

#[test]
fn rust_straight_line_function_scores_one() {
    let facts = compute_file_complexity_facts("src/sample.rs", RUST_SAMPLE);
    assert_eq!(facts.language, Language::Rust);
    assert_eq!(facts.availability, FactAvailability::Available);
    assert_unit(&facts, "straight_line", 1, 0, Some(1));
    let unit = unit(&facts, "straight_line");
    assert_eq!(unit.function_length, 3);
}

#[test]
fn rust_three_independent_ifs_score_four() {
    let facts = compute_file_complexity_facts("src/sample.rs", RUST_SAMPLE);
    assert_unit(&facts, "three_ifs", 4, 1, Some(3));
    assert_eq!(unit(&facts, "three_ifs").function_length, 6);
}

#[test]
fn rust_nesting_counts_depth_and_treats_else_if_as_one_level() {
    let facts = compute_file_complexity_facts("src/sample.rs", RUST_SAMPLE);
    // if + for + inner if + the `else if` conditional = 4 decision points.
    assert_unit(&facts, "nested", 5, 3, Some(2));
}

#[test]
fn rust_short_circuit_operators_are_decision_points() {
    let facts = compute_file_complexity_facts("src/sample.rs", RUST_SAMPLE);
    // if + && + || = 3 decision points.
    assert_unit(&facts, "logic", 4, 1, Some(3));
}

#[test]
fn rust_match_counts_arms_and_guards_but_not_the_wildcard() {
    let facts = compute_file_complexity_facts("src/sample.rs", RUST_SAMPLE);
    // `0 =>` + `1 if b =>` + its guard = 3 decision points; `_ =>` is the default.
    assert_unit(&facts, "matcher", 4, 1, Some(2));
}

#[test]
fn rust_try_operator_is_a_decision_point() {
    let facts = compute_file_complexity_facts("src/sample.rs", RUST_SAMPLE);
    assert_unit(&facts, "fallible", 2, 0, Some(1));
}

#[test]
fn rust_loops_are_decision_points() {
    let facts = compute_file_complexity_facts("src/sample.rs", RUST_SAMPLE);
    assert_unit(&facts, "loops", 3, 1, Some(1));
}

#[test]
fn rust_closure_complexity_is_attributed_to_the_enclosing_function() {
    let facts = compute_file_complexity_facts("src/sample.rs", RUST_SAMPLE);
    // The closure's `if` belongs to `with_closure`; the closure is not a unit.
    assert_unit(&facts, "with_closure", 2, 1, Some(1));
    assert!(facts
        .symbols
        .iter()
        .all(|unit| !unit.symbol.is_empty() && unit.symbol != "map"));
}

#[test]
fn rust_impl_methods_are_qualified_and_exclude_the_receiver() {
    let facts = compute_file_complexity_facts("src/sample.rs", RUST_SAMPLE);
    assert_unit(&facts, "Widget.method", 2, 1, Some(1));
}

#[test]
fn rust_units_are_ordered_and_deterministic() {
    let facts = compute_file_complexity_facts("src/sample.rs", RUST_SAMPLE);
    let offsets: Vec<usize> = facts.symbols.iter().map(|unit| unit.byte_offset).collect();
    let mut sorted = offsets.clone();
    sorted.sort_unstable();
    assert_eq!(offsets, sorted, "units must be ordered by byte offset");
    assert_deterministic("src/sample.rs", RUST_SAMPLE);
}

#[test]
fn rust_facts_join_to_extracted_symbols() {
    assert_joins_to_symbols(
        "src/sample.rs",
        RUST_SAMPLE,
        &["straight_line", "three_ifs", "matcher", "Widget.method"],
    );
}

// ---------------------------------------------------------------------------
// Python
// ---------------------------------------------------------------------------

const PYTHON_SAMPLE: &str = r#"def straight_line(a):
    return a + 1


def three_ifs(a, b, c):
    if a:
        return 1
    if b:
        return 2
    if c:
        return 3
    return 0


def branches(a, b):
    if a and b:
        for i in range(a):
            while i > b:
                return i
    elif a or b:
        return b
    else:
        return 0
    return -1


def handler(a):
    try:
        return a
    except ValueError:
        return 1
    except Exception:
        return 2
    finally:
        pass


def comprehension(items):
    return [x for x in items if x]


def ternary(a):
    return 1 if a else 2


def matcher(a):
    match a:
        case 1:
            return 1
        case _:
            return 0


class Service:
    def method(self, a, b=1):
        return a + b

    @staticmethod
    def helper(a, *args, **kwargs):
        return a
"#;

#[test]
fn python_straight_line_function_scores_one() {
    let facts = compute_file_complexity_facts("service.py", PYTHON_SAMPLE);
    assert_eq!(facts.language, Language::Python);
    assert_eq!(facts.availability, FactAvailability::Available);
    assert_unit(&facts, "straight_line", 1, 0, Some(1));
    assert_eq!(unit(&facts, "straight_line").function_length, 2);
}

#[test]
fn python_three_independent_ifs_score_four() {
    let facts = compute_file_complexity_facts("service.py", PYTHON_SAMPLE);
    assert_unit(&facts, "three_ifs", 4, 1, Some(3));
}

#[test]
fn python_counts_elif_loops_and_boolean_operators() {
    let facts = compute_file_complexity_facts("service.py", PYTHON_SAMPLE);
    // if + `and` + for + while + elif + `or` = 6 decision points; `else` is free.
    assert_unit(&facts, "branches", 7, 3, Some(2));
}

#[test]
fn python_counts_each_except_handler_but_not_finally() {
    let facts = compute_file_complexity_facts("service.py", PYTHON_SAMPLE);
    assert_unit(&facts, "handler", 3, 1, Some(1));
}

#[test]
fn python_counts_comprehension_guards_and_ternaries() {
    let facts = compute_file_complexity_facts("service.py", PYTHON_SAMPLE);
    assert_unit(&facts, "comprehension", 2, 0, Some(1));
    assert_unit(&facts, "ternary", 2, 0, Some(1));
}

#[test]
fn python_match_counts_cases_but_not_the_wildcard() {
    let facts = compute_file_complexity_facts("service.py", PYTHON_SAMPLE);
    assert_unit(&facts, "matcher", 2, 1, Some(1));
}

#[test]
fn python_methods_are_qualified_and_exclude_the_receiver() {
    let facts = compute_file_complexity_facts("service.py", PYTHON_SAMPLE);
    assert_unit(&facts, "Service.method", 1, 0, Some(2));
    // A static method declares no receiver, so every parameter counts.
    assert_unit(&facts, "Service.helper", 1, 0, Some(3));
}

#[test]
fn python_facts_are_deterministic_and_join_to_symbols() {
    assert_deterministic("service.py", PYTHON_SAMPLE);
    assert_joins_to_symbols(
        "service.py",
        PYTHON_SAMPLE,
        &["three_ifs", "matcher", "Service.method", "Service.helper"],
    );
}

// ---------------------------------------------------------------------------
// Go
// ---------------------------------------------------------------------------

const GO_SAMPLE: &str = r#"package sample

func straightLine(a int) int {
	return a + 1
}

func threeIfs(a, b, c int) int {
	if a > 0 {
		return 1
	}
	if b > 0 {
		return 2
	}
	if c > 0 {
		return 3
	}
	return 0
}

func branches(a int, b string) int {
	if a > 0 && b != "" {
		for i := 0; i < a; i++ {
			if i > 3 {
				return i
			}
		}
	} else if a < 0 {
		return -1
	} else {
		return 0
	}
	return 1
}

func switching(a int) int {
	switch a {
	case 1:
		return 1
	case 2, 3:
		return 2
	default:
		return 0
	}
}

func (s *Service) Handle(a int, b, c string) int {
	for range []int{} {
	}
	return a
}
"#;

#[test]
fn go_straight_line_function_scores_one() {
    let facts = compute_file_complexity_facts("sample.go", GO_SAMPLE);
    assert_eq!(facts.language, Language::Go);
    assert_eq!(facts.availability, FactAvailability::Available);
    assert_unit(&facts, "straightLine", 1, 0, Some(1));
}

#[test]
fn go_grouped_parameter_declarations_count_each_name() {
    let facts = compute_file_complexity_facts("sample.go", GO_SAMPLE);
    // `a, b, c int` is one declaration naming three parameters.
    assert_unit(&facts, "threeIfs", 4, 1, Some(3));
}

#[test]
fn go_counts_short_circuits_loops_and_else_if() {
    let facts = compute_file_complexity_facts("sample.go", GO_SAMPLE);
    // if + && + for + inner if + the `else if` conditional = 5 decision points.
    assert_unit(&facts, "branches", 6, 3, Some(2));
}

#[test]
fn go_switch_counts_cases_but_not_default() {
    let facts = compute_file_complexity_facts("sample.go", GO_SAMPLE);
    assert_unit(&facts, "switching", 3, 1, Some(1));
}

#[test]
fn go_methods_are_qualified_and_exclude_the_receiver() {
    let facts = compute_file_complexity_facts("sample.go", GO_SAMPLE);
    // `a int, b, c string` declares three parameters; the receiver is not one.
    assert_unit(&facts, "Service.Handle", 2, 1, Some(3));
}

#[test]
fn go_facts_are_deterministic_and_join_to_symbols() {
    assert_deterministic("sample.go", GO_SAMPLE);
    assert_joins_to_symbols(
        "sample.go",
        GO_SAMPLE,
        &["threeIfs", "switching", "Service.Handle"],
    );
}

// ---------------------------------------------------------------------------
// Availability
// ---------------------------------------------------------------------------

#[test]
fn unsupported_language_is_unavailable_not_zero() {
    let facts = compute_file_complexity_facts("notes.txt", "anything at all");
    assert_eq!(facts.availability, FactAvailability::Unavailable);
    assert_eq!(
        facts.unavailable_reason,
        Some(ComplexityUnavailableReason::UnsupportedLanguage)
    );
    assert!(facts.rollup.is_none(), "no rollup may be fabricated");
    assert!(facts.symbols.is_empty());
}

#[test]
fn broken_source_degrades_rather_than_reporting_clean_facts() {
    let source = "pub fn broken(a: u32) -> u32 {\n    if a > 0 { return 1;\n";
    let facts = compute_file_complexity_facts("src/broken.rs", source);
    assert_eq!(facts.availability, FactAvailability::Degraded);
    assert_eq!(
        facts.unavailable_reason,
        Some(ComplexityUnavailableReason::PartialParse)
    );
}

#[test]
fn file_without_functions_reports_no_extrema() {
    let facts = compute_file_complexity_facts("src/types.rs", "pub struct A;\npub struct B;\n");
    assert_eq!(facts.availability, FactAvailability::Available);
    let rollup = facts.rollup.expect("parsed file has a rollup");
    assert_eq!(rollup.function_count, 0);
    assert_eq!(rollup.max_cyclomatic_complexity, None);
    assert_eq!(rollup.p90_cyclomatic_complexity, None);
    assert_eq!(rollup.max_function_length, None);
    assert_eq!(rollup.max_nesting_depth, None);
}

// ---------------------------------------------------------------------------
// Rollups
// ---------------------------------------------------------------------------

fn synthetic_unit(name: &str, complexity: u32, length: u32, nesting: u32, params: Option<u32>) -> SymbolComplexityFacts {
    SymbolComplexityFacts {
        file: "src/synthetic.rs".to_string(),
        symbol: name.to_string(),
        byte_offset: 0,
        line: 1,
        end_line: length as usize,
        function_length: length,
        branch_count: complexity - 1,
        cyclomatic_complexity: complexity,
        max_nesting_depth: nesting,
        param_count: params,
        availability: FactAvailability::Available,
    }
}

fn test_config() -> HealthConfig {
    HealthConfig {
        version: 99,
        complexity: ComplexityThresholds {
            high_cyclomatic_complexity: 10,
            high_function_length: 60,
            high_nesting_depth: 4,
            high_param_count: 5,
        },
        rollup_percentile_per_mille: 900,
    }
}

#[test]
fn rollup_percentile_uses_nearest_rank() {
    let units: Vec<SymbolComplexityFacts> = (1..=10)
        .map(|value| synthetic_unit(&format!("f{value}"), value, value, 0, Some(1)))
        .collect();
    let rollup = roll_up(&units, Language::Rust, &test_config());
    assert_eq!(rollup.function_count, 10);
    assert_eq!(rollup.max_cyclomatic_complexity, Some(10));
    // Nearest-rank p90 of 1..=10 is the 9th value.
    assert_eq!(rollup.p90_cyclomatic_complexity, Some(9));
    assert_eq!(rollup.p90_function_length, Some(9));
}

#[test]
fn rollup_percentile_of_single_function_is_that_function() {
    let units = vec![synthetic_unit("only", 7, 12, 2, Some(2))];
    let rollup = roll_up(&units, Language::Rust, &test_config());
    assert_eq!(rollup.p90_cyclomatic_complexity, Some(7));
    assert_eq!(rollup.p90_function_length, Some(12));
}

#[test]
fn rollup_threshold_edges_admit_the_threshold_value() {
    let units = vec![
        synthetic_unit("at_limit", 10, 60, 4, Some(5)),
        synthetic_unit("over_limit", 11, 61, 5, Some(6)),
    ];
    let rollup = roll_up(&units, Language::Rust, &test_config());
    assert_eq!(rollup.functions_over_complexity_threshold, 1);
    assert_eq!(rollup.functions_over_length_threshold, 1);
    assert_eq!(rollup.functions_over_nesting_threshold, 1);
    assert_eq!(rollup.functions_over_param_threshold, 1);
    assert_eq!(rollup.over_threshold_share_per_mille, 500);
}

#[test]
fn rollup_per_mille_arithmetic_truncates_deterministically() {
    // One flagged function in three: 1000/3 truncates to 333 per-mille.
    let units = vec![
        synthetic_unit("hot", 11, 10, 0, Some(1)),
        synthetic_unit("cool_a", 1, 10, 0, Some(1)),
        synthetic_unit("cool_b", 2, 10, 0, Some(1)),
    ];
    let rollup = roll_up(&units, Language::Rust, &test_config());
    assert_eq!(rollup.over_threshold_share_per_mille, 333);
    // Mean complexity (11 + 1 + 2) / 3 = 4.666… -> 4666 per-mille.
    assert_eq!(rollup.mean_cyclomatic_complexity_per_mille, 4666);
    assert_eq!(rollup.percentile_per_mille, 900);
}

#[test]
fn rollup_reports_unknown_parameter_counts_instead_of_zero() {
    let units = vec![
        synthetic_unit("known", 1, 10, 0, Some(9)),
        synthetic_unit("unknown", 1, 10, 0, None),
    ];
    let rollup = roll_up(&units, Language::Rust, &test_config());
    assert_eq!(rollup.functions_with_unknown_param_count, 1);
    // The unknown function is not counted as under the threshold either.
    assert_eq!(rollup.functions_over_param_threshold, 1);
}

#[test]
fn rollup_of_empty_input_is_all_absent() {
    let rollup = roll_up(&[], Language::Rust, &test_config());
    assert_eq!(rollup.function_count, 0);
    assert_eq!(rollup.over_threshold_share_per_mille, 0);
    assert_eq!(rollup.mean_cyclomatic_complexity_per_mille, 0);
    assert_eq!(rollup.max_cyclomatic_complexity, None);
}

#[test]
fn facts_echo_the_config_version_they_were_produced_under() {
    let config = test_config();
    let facts = compute_file_complexity_facts_with("src/sample.rs", RUST_SAMPLE, &config);
    assert_eq!(facts.config_version, 99);
}
