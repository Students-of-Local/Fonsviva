use std::path::{Path, PathBuf};

use fonsviva_core::plan::{BenchResult, load_results, ratchet};

fn results(dir: &Path) -> Vec<BenchResult> {
    load_results(dir)
        .unwrap()
        .into_iter()
        .map(|(_, r)| r)
        .collect()
}

fn fixture(case: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/ratchet")
        .join(case)
}

#[test]
fn the_recorded_results_hold_the_ratchet() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../benchmarks/results/topology");
    let problems = ratchet(&results(&dir));
    assert!(problems.is_empty(), "{problems:#?}");
}

#[test]
fn a_fleet_that_follows_every_rule_passes() {
    let problems = ratchet(&results(&fixture("good")));
    assert!(problems.is_empty(), "{problems:#?}");
}

#[test]
fn each_broken_rule_fails_with_its_reason() {
    for (case, reason) in [
        (
            "slower-plan",
            "mac alone was recorded as the plan at 40.0 tok/s, but parish1 alone measured 60.0",
        ),
        (
            "regression",
            "below 90% of the best earlier 60.0; add a regression_note",
        ),
        (
            "split-loses",
            "the split mac+parish1 over Cable was planned at 50.0 tok/s but one machine measured 60.0",
        ),
        (
            "split-without-single",
            "no single-machine result exists to beat",
        ),
        ("bad-schema", "log_sha256 is not a SHA-256"),
        (
            "bad-schema",
            "the split's link is not among the measured links",
        ),
    ] {
        let problems = ratchet(&results(&fixture(case)));
        assert!(
            problems.iter().any(|p| p.contains(reason)),
            "{case}: expected {reason:?} in {problems:#?}"
        );
    }
}

#[test]
fn a_result_with_a_field_the_schema_does_not_know_is_refused() {
    let err = load_results(&fixture("unknown-field")).unwrap_err();
    assert!(format!("{err:#}").contains("unknown field"), "{err:#}");
}
