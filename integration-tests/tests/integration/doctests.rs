// Copyright (c) The nextest Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tests for running doctests with `--doctests`.

use camino::Utf8Path;
use fs_err as fs;
use integration_tests::{env::set_env_vars_for_test, nextest_cli::CargoNextestCli};
use nextest_metadata::{NextestExitCode, RustTestBinaryKind};
use regex::Regex;
use std::collections::BTreeMap;

const LIB_RS: &str = r#"
/// ```
/// assert_eq!(doctest_crate::add(1, 2), 3);
/// ```
pub fn pass() {}

/// ```
/// assert_eq!(1, 2, "deliberate failure");
/// ```
pub fn fail() {}

/// ```should_panic
/// panic!("expected panic");
/// ```
pub fn should_panic_pass() {}

/// ```should_panic
/// let _ = 1;
/// ```
pub fn should_panic_fail() {}

/// ```no_run
/// loop {}
/// ```
pub fn no_run() {}

/// ```compile_fail
/// let x: i32 = "not an integer";
/// ```
pub fn compile_fail() {}

/// ```ignore
/// this is not valid rust
/// ```
pub fn ignored() {}

/// Crate-level attributes make this doctest standalone even with the 2024
/// edition.
///
/// ```
/// #![allow(unused)]
/// assert!(std::path::Path::new("Cargo.toml").exists(), "runs in the package dir");
/// assert_eq!(std::env::var("CARGO_PKG_NAME").unwrap(), "doctest-crate");
/// ```
pub fn standalone_pass() {}

/// ```
/// #![allow(unused)]
/// std::process::exit(3);
/// ```
pub fn standalone_fail() {}

/// ```
/// let x: i32 = "does not compile";
/// ```
pub fn compile_error() {}

pub fn add(a: i32, b: i32) -> i32 {
    a + b
}
"#;

/// The expected outcome of each doctest, keyed by the function it documents.
const EXPECTED: &[(&str, &str)] = &[
    ("pass", "PASS"),
    ("fail", "FAIL"),
    ("should_panic_pass", "PASS"),
    ("should_panic_fail", "FAIL"),
    ("no_run", "PASS"),
    ("compile_fail", "PASS"),
    ("standalone_pass", "PASS"),
    ("standalone_fail", "FAIL"),
    ("compile_error", "FAIL"),
];

fn write_crate(dir: &Utf8Path, edition: &str) {
    fs::write(
        dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"doctest-crate\"\nversion = \"0.1.0\"\nedition = \"{edition}\"\n\n\
             [workspace]\n"
        ),
    )
    .unwrap();
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(dir.join("src/lib.rs"), LIB_RS).unwrap();
}

/// Extracts the item name from a doctest name like `src/lib.rs - pass (line 2)`.
fn item_name(test_name: &str) -> &str {
    test_name
        .split(" - ")
        .nth(1)
        .and_then(|rest| rest.split(" (line").next())
        .unwrap_or_else(|| panic!("unexpected doctest name: {test_name}"))
}

#[test]
fn test_doctests() {
    let env_info = set_env_vars_for_test();
    let status_re =
        Regex::new(r"(?m)^\s+(PASS|FAIL) \[[^\]]*\] \(\s*\d+/\d+\) (\S+) (.+)$").unwrap();

    // The 2021 edition only produces standalone doctests, while the 2024
    // edition merges doctests where possible.
    for edition in ["2021", "2024"] {
        let temp = camino_tempfile::Builder::new()
            .prefix("nextest-doctests-")
            .tempdir()
            .unwrap();
        write_crate(temp.path(), edition);
        let manifest_path = temp.path().join("Cargo.toml");

        let output = CargoNextestCli::for_test(&env_info)
            .args([
                "--manifest-path",
                manifest_path.as_str(),
                "list",
                "--doctests",
                "--message-format",
                "json",
            ])
            .env_remove("RUSTFLAGS")
            .output();
        let list = output.decode_test_list_json().unwrap();
        let suite = list
            .rust_suites
            .values()
            .find(|suite| suite.binary.kind == RustTestBinaryKind::DOCTEST)
            .unwrap_or_else(|| panic!("[{edition}] no doctest suite in list"));
        assert_eq!(
            suite.binary.binary_id.as_str(),
            "doctest-crate::doctest/doctest_crate",
            "[{edition}] binary ID"
        );
        let listed: BTreeMap<&str, bool> = suite
            .test_cases
            .iter()
            .map(|(name, case)| (item_name(name.as_str()), case.ignored))
            .collect();
        let mut expected_listed: BTreeMap<&str, bool> =
            EXPECTED.iter().map(|(name, _)| (*name, false)).collect();
        expected_listed.insert("ignored", true);
        assert_eq!(listed, expected_listed, "[{edition}] listed doctests");
        assert!(
            list.rust_suites
                .values()
                .any(|suite| suite.binary.kind == RustTestBinaryKind::LIB),
            "[{edition}] --doctests also lists unit tests"
        );

        let output = CargoNextestCli::for_test(&env_info)
            .args([
                "--manifest-path",
                manifest_path.as_str(),
                "list",
                "--doc",
                "--message-format",
                "json",
            ])
            .env_remove("RUSTFLAGS")
            .output();
        let list = output.decode_test_list_json().unwrap();
        let kinds: Vec<_> = list
            .rust_suites
            .values()
            .map(|suite| suite.binary.kind.clone())
            .collect();
        assert_eq!(
            kinds,
            vec![RustTestBinaryKind::DOCTEST],
            "[{edition}] --doc lists only doctests"
        );

        let output = CargoNextestCli::for_test(&env_info)
            .args([
                "--manifest-path",
                manifest_path.as_str(),
                "run",
                // `--doc` runs only doctests, without building other tests.
                "--doc",
                "--no-fail-fast",
            ])
            .env_remove("RUSTFLAGS")
            .unchecked(true)
            .output();
        assert_eq!(
            output.exit_status.code(),
            Some(NextestExitCode::TEST_RUN_FAILED),
            "[{edition}] exit code\n{}",
            output.to_snapshot()
        );

        let stderr = output.stderr_as_str();
        let mut outcomes = BTreeMap::new();
        for captures in status_re.captures_iter(&stderr) {
            let binary_id = captures.get(2).unwrap().as_str();
            assert_eq!(binary_id, "doctest-crate::doctest/doctest_crate");
            let item = item_name(captures.get(3).unwrap().as_str());
            // The final summary repeats failures, so only record the first
            // occurrence.
            outcomes
                .entry(item.to_owned())
                .or_insert_with(|| captures.get(1).unwrap().as_str().to_owned());
        }
        let expected: BTreeMap<String, String> = EXPECTED
            .iter()
            .map(|(name, status)| ((*name).to_owned(), (*status).to_owned()))
            .collect();
        assert_eq!(
            outcomes,
            expected,
            "[{edition}] doctest outcomes\n{}",
            output.to_snapshot()
        );
        assert!(
            stderr.contains("deliberate failure"),
            "[{edition}] failure output is shown\n{stderr}"
        );
    }
}
