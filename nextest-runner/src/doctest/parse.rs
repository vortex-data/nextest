// Copyright (c) The nextest Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Parsers for the artifacts rustdoc produces: generated runner source for
//! merged doctests, and libtest's human-readable output.

use regex::Regex;
use std::{collections::BTreeMap, sync::LazyLock};

/// A test entry in a merged doctest runner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct MergedTestDesc {
    pub(super) index: usize,
    pub(super) name: String,
    pub(super) ignore: bool,
    pub(super) no_run: bool,
    pub(super) should_panic: bool,
}

// Rustdoc generates, for each merged doctest (see
// `generate_mergeable_doctest` in src/librustdoc/doctest/runner.rs):
//
// mod __doctest_3 {
// pub const TEST: test::TestDescAndFn = test::TestDescAndFn::new_doctest(
// "src/lib.rs - p (line 9)", false, "src/lib.rs", 9, false, true,
//
// The arguments are: name, ignore, file, line, no_run, should_panic. Older
// versions of Rust use `pub const TEST`, and newer ones `pub static TEST`.
static MERGED_TEST_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r#"mod __doctest_(\d+) \{\s*pub (?:const|static) TEST: test::TestDescAndFn = "#,
        r#"test::TestDescAndFn::new_doctest\(\s*"#,
        r#"("(?:[^"\\]|\\.)*"),\s*(true|false),\s*"(?:[^"\\]|\\.)*",\s*\d+,\s*"#,
        r#"(true|false),\s*(true|false),"#,
    ))
    .expect("regex is valid")
});

/// The environment variable rustdoc's merged doctest runner reads to run a
/// single test in-process.
pub(super) const RUN_NB_TEST_ENV: &str = "RUSTDOC_DOCTEST_RUN_NB_TEST";

/// Parses the runner source rustdoc generates for a group of merged doctests.
pub(super) fn parse_merged_runner(source: &str) -> Result<Vec<MergedTestDesc>, String> {
    let mut tests = Vec::new();
    for captures in MERGED_TEST_RE.captures_iter(source) {
        let index = captures[1]
            .parse()
            .map_err(|err| format!("invalid test index `{}`: {err}", &captures[1]))?;
        let name = unescape_rust_str(&captures[2])?;
        tests.push(MergedTestDesc {
            index,
            name,
            ignore: &captures[3] == "true",
            no_run: &captures[4] == "true",
            should_panic: &captures[5] == "true",
        });
    }

    // Sanity checks: the runner must select tests the way we expect, rustdoc
    // numbers tests densely from 0, and the number of test descriptors must
    // match the number of tests we parsed.
    if !source.contains(RUN_NB_TEST_ENV) {
        return Err(format!("runner does not mention `{RUN_NB_TEST_ENV}`"));
    }
    let descriptor_count = source.matches("TestDescAndFn::new_doctest(").count();
    if descriptor_count == 0 {
        return Err("runner contains no test descriptors".to_owned());
    }
    if descriptor_count != tests.len() {
        return Err(format!(
            "found {descriptor_count} test descriptors but could only parse {}",
            tests.len()
        ));
    }
    tests.sort_by_key(|test| test.index);
    for (expected, test) in tests.iter().enumerate() {
        if test.index != expected {
            return Err(format!(
                "test indexes are not contiguous: expected {expected}, found {}",
                test.index
            ));
        }
    }
    Ok(tests)
}

/// Unescapes a string literal as produced by Rust's `Debug` impl for `str`.
fn unescape_rust_str(literal: &str) -> Result<String, String> {
    let inner = literal
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .ok_or_else(|| format!("not a string literal: {literal}"))?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('0') => out.push('\0'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('\'') => out.push('\''),
            Some('u') => {
                if chars.next() != Some('{') {
                    return Err(format!("invalid unicode escape in {literal}"));
                }
                let hex: String = chars.by_ref().take_while(|&c| c != '}').collect();
                let value = u32::from_str_radix(&hex, 16)
                    .ok()
                    .and_then(char::from_u32)
                    .ok_or_else(|| format!("invalid unicode escape in {literal}"))?;
                out.push(value);
            }
            other => return Err(format!("unknown escape {other:?} in {literal}")),
        }
    }
    Ok(out)
}

/// Extracts the name of the test libtest is currently running from the last,
/// unterminated line of single-threaded pretty output (`test <name> ... `).
pub(super) fn current_test_from_output(output: &str) -> Option<&str> {
    let last_line = output.rsplit('\n').next()?;
    last_line.strip_prefix("test ")?.strip_suffix(" ... ")
}

/// The status libtest reported for a test.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LibtestStatus {
    Ok,
    Failed,
    Ignored,
}

/// The parsed results of a libtest run in pretty format.
#[derive(Debug, Default)]
pub(super) struct LibtestResults {
    /// Map of displayed test name (possibly with a mode suffix such as
    /// ` - compile fail`) to status.
    pub(super) statuses: BTreeMap<String, LibtestStatus>,

    /// Map of displayed test name to captured failure output.
    pub(super) failure_output: BTreeMap<String, String>,
}

/// Parses libtest's pretty output. Multiple `running N tests` sections (as
/// rustdoc produces) are supported.
pub(super) fn parse_libtest_output(output: &str) -> LibtestResults {
    let mut results = LibtestResults::default();
    let mut current_failure: Option<(String, String)> = None;

    let finish = |results: &mut LibtestResults, failure: Option<(String, String)>| {
        if let Some((name, text)) = failure {
            results
                .failure_output
                .insert(name, text.trim_end().to_owned());
        }
    };

    for line in output.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if let Some(rest) = trimmed
            .strip_prefix("---- ")
            .and_then(|s| s.strip_suffix(" stdout ----"))
        {
            finish(&mut results, current_failure.take());
            current_failure = Some((rest.to_owned(), String::new()));
            continue;
        }
        if trimmed == "failures:" || trimmed.starts_with("test result: ") {
            finish(&mut results, current_failure.take());
            continue;
        }
        if let Some((_, text)) = &mut current_failure {
            text.push_str(line);
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("test ") {
            for (suffix, status) in [
                (" ... ok", LibtestStatus::Ok),
                (" ... FAILED", LibtestStatus::Failed),
                (" ... ignored", LibtestStatus::Ignored),
            ] {
                if let Some(name) = rest.strip_suffix(suffix) {
                    results.statuses.insert(name.to_owned(), status);
                    break;
                }
            }
            // Ignored tests can have a message: `test foo ... ignored, reason`.
            if let Some((name, _)) = rest.split_once(" ... ignored, ") {
                results
                    .statuses
                    .insert(name.to_owned(), LibtestStatus::Ignored);
            }
        }
    }
    finish(&mut results, current_failure.take());
    results
}

/// Suffixes libtest appends to doctest names when displaying them.
pub(super) const DISPLAY_SUFFIXES: &[&str] = &[" - compile fail", " - compile", " - should panic"];

/// The message rustdoc prints when a `should_panic` doctest exits successfully.
pub(super) const SHOULD_PANIC_SUCCEEDED: &str =
    "Test executable succeeded, but it's marked `should_panic`.";

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;

    #[test]
    fn parse_runner() {
        let source = indoc! {r#"
            extern crate test;
            pub const RUN_OPTION: &str = "RUSTDOC_DOCTEST_RUN_NB_TEST";

            mod __doctest_0 {
            pub const TEST: test::TestDescAndFn = test::TestDescAndFn::new_doctest(
            "src/lib.rs - add (line 1)", false, "src/lib.rs", 1, false, false,
            test::StaticTestFn(
                || {},
            ));
            }

            mod __doctest_1 {
            pub const TEST: test::TestDescAndFn = test::TestDescAndFn::new_doctest(
            "src/lib.rs - Foo<\"x\">::ig (line 17)", true, "src/lib.rs", 17, false, false,
            test::StaticTestFn(
                || {test::assert_test_result(Ok::<(), String>(()))},
            ));
            }

            mod __doctest_2 {
            pub const TEST: test::TestDescAndFn = test::TestDescAndFn::new_doctest(
            "src/lib.rs - nr (line 13)", false, "src/lib.rs", 13, true, false,
            test::StaticTestFn(|| {}));
            }

            mod __doctest_3 {
            pub static TEST: test::TestDescAndFn = test::TestDescAndFn::new_doctest(
            "src/caf\u{e9}.rs - p (line 9)", false, "src/caf\u{e9}.rs", 9, false, true,
            test::StaticTestFn(|| {}));
            }
        "#};
        let tests = parse_merged_runner(source).expect("parsed");
        let summary: Vec<_> = tests
            .iter()
            .map(|t| (t.index, t.name.as_str(), t.ignore, t.no_run, t.should_panic))
            .collect();
        assert_eq!(
            summary,
            vec![
                (0, "src/lib.rs - add (line 1)", false, false, false),
                (
                    1,
                    "src/lib.rs - Foo<\"x\">::ig (line 17)",
                    true,
                    false,
                    false
                ),
                (2, "src/lib.rs - nr (line 13)", false, true, false),
                (3, "src/café.rs - p (line 9)", false, false, true),
            ]
        );
    }

    #[test]
    fn parse_runner_rejects_unknown_shapes() {
        let source = indoc! {r#"
            RUSTDOC_DOCTEST_RUN_NB_TEST
            mod __doctest_0 {
            pub const TEST: test::TestDescAndFn = test::TestDescAndFn::new_doctest(
            "a", false, "src/lib.rs", 1, false, false,
            ));
            }
            mod __doctest_1 {
            pub const TEST: test::TestDescAndFn = test::TestDescAndFn::new_doctest(
            "b", false, "src/lib.rs", 1, 7, false, false,
            ));
            }
        "#};
        // A descriptor with an unexpected shape must not be silently skipped.
        parse_merged_runner(source).expect_err("descriptor count mismatch");

        let gap = indoc! {r#"
            RUSTDOC_DOCTEST_RUN_NB_TEST
            mod __doctest_1 {
            pub const TEST: test::TestDescAndFn = test::TestDescAndFn::new_doctest(
            "b", false, "src/lib.rs", 1, false, false,
            ));
            }
        "#};
        parse_merged_runner(gap).expect_err("non-contiguous indexes");

        let no_env = gap.replace("RUSTDOC_DOCTEST_RUN_NB_TEST", "");
        parse_merged_runner(&no_env).expect_err("runner without the selection mechanism");
        parse_merged_runner("RUSTDOC_DOCTEST_RUN_NB_TEST").expect_err("no descriptors");
    }

    #[test]
    fn unescape() {
        for (input, expected) in [
            (r#""plain""#, Some("plain")),
            (r#""a\"b""#, Some("a\"b")),
            (r#""a\\b""#, Some("a\\b")),
            (r#""tab\there""#, Some("tab\there")),
            (r#""\u{1f600}""#, Some("\u{1f600}")),
            (r#""bad\q""#, None),
            ("unquoted", None),
        ] {
            assert_eq!(
                unescape_rust_str(input).ok().as_deref(),
                expected,
                "input: {input}"
            );
        }
    }

    #[test]
    fn current_test() {
        for (output, expected) in [
            (
                "running 2 tests\ntest src/lib.rs - a (line 1) ... ",
                Some("src/lib.rs - a (line 1)"),
            ),
            (
                "running 2 tests\ntest x ... ok\ntest src/lib.rs - b (line 5) ... ",
                Some("src/lib.rs - b (line 5)"),
            ),
            ("running 2 tests\ntest x ... ok\n", None),
            ("", None),
        ] {
            assert_eq!(
                current_test_from_output(output),
                expected,
                "output: {output:?}"
            );
        }
    }

    #[test]
    fn libtest_output() {
        let output = indoc! {"

            running 5 tests
            test src/lib.rs - add (line 1) ... ok
            test src/lib.rs - cf (line 21) - compile fail ... ok
            test src/lib.rs - ig (line 17) ... ignored
            test src/lib.rs - p (line 9) ... FAILED
            test src/lib.rs - s3 (line 35) ... FAILED

            failures:

            ---- src/lib.rs - p (line 9) stdout ----
            Test executable succeeded, but it's marked `should_panic`.
            ---- src/lib.rs - s3 (line 35) stdout ----
            error[E0308]: mismatched types

            Couldn't compile the test.

            failures:
                src/lib.rs - p (line 9)
                src/lib.rs - s3 (line 35)

            test result: FAILED. 2 passed; 2 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.79s

        "};
        let results = parse_libtest_output(output);
        assert_eq!(
            results.statuses.into_iter().collect::<Vec<_>>(),
            vec![
                ("src/lib.rs - add (line 1)".to_owned(), LibtestStatus::Ok),
                (
                    "src/lib.rs - cf (line 21) - compile fail".to_owned(),
                    LibtestStatus::Ok
                ),
                (
                    "src/lib.rs - ig (line 17)".to_owned(),
                    LibtestStatus::Ignored
                ),
                ("src/lib.rs - p (line 9)".to_owned(), LibtestStatus::Failed),
                (
                    "src/lib.rs - s3 (line 35)".to_owned(),
                    LibtestStatus::Failed
                ),
            ]
        );
        assert_eq!(
            results.failure_output.into_iter().collect::<Vec<_>>(),
            vec![
                (
                    "src/lib.rs - p (line 9)".to_owned(),
                    SHOULD_PANIC_SUCCEEDED.to_owned()
                ),
                (
                    "src/lib.rs - s3 (line 35)".to_owned(),
                    "error[E0308]: mismatched types\n\nCouldn't compile the test.".to_owned()
                ),
            ]
        );
    }
}
