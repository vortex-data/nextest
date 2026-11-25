---
icon: material/file-document-check-outline
sidebar_icon: false
description: Design document describing how nextest runs doctests on stable Rust.
---

# Running doctests

!!! abstract "Design document"

    This is a design document intended for nextest contributors and curious readers.

With `--doctests`, nextest runs each doctest as its own test, in parallel with other doctests and other tests. This page explains how that works on stable Rust, where rustdoc doesn't offer a supported way to compile doctests ahead of time.

## Background

`cargo test --doc` runs rustdoc once per crate, one crate after another. Within a crate, rustdoc compiles and runs doctests itself:

* With the Rust 2024 edition and later, compatible doctests are _merged_: compiled into a single binary per crate (per edition and set of crate attributes). The merged binary runs each test in a separate process by re-executing itself with the `RUSTDOC_DOCTEST_RUN_NB_TEST` environment variable set to the index of the test.
* Other doctests are _standalone_: compiled into one binary each, in parallel across libtest's test threads.

In a large workspace, the time is dominated by compiling merged binaries, which happens serially across crates. The flags that would let a tool compile doctests and run them later (`--persist-doctests`, `--no-run`, and `--merge-doctests`) are unstable.

## Design principles

* **Stable Rust only.** Nextest doesn't use `RUSTC_BOOTSTRAP` or unstable flags to make this work.
* **One test per doctest.** Doctests are scheduled, retried, timed out, filtered, and reported exactly like other tests. They live in suites of kind `doctest`, with binary IDs like `my-crate::doctest/my_crate`.
* **Fail loudly.** The mechanism relies on rustdoc implementation details. Every step validates what it observes, and nextest reports an error instead of guessing.

## How it works

The implementation lives in `nextest_runner::doctest`. There are three phases.

### Capture

Nextest runs `cargo test --doc`, with the same package, feature, and build options as the main build, and with `RUSTDOC` set to the nextest binary itself. Cargo builds each library and its dev-dependencies, then invokes "rustdoc" for each crate. Nextest records the arguments, working directory, and environment of each invocation to a JSON file and exits successfully without doing anything else.

The environment matters: Cargo sets `CARGO_PKG_*`, `CARGO_MANIFEST_DIR`, `OUT_DIR`, and similar variables, which doctests can read at compile time with `env!`. Replaying the recorded environment preserves that.

### Compile

For each crate, in parallel (up to `--build-jobs` crates at a time, defaulting to the number of CPUs), nextest replays the recorded invocation with two additions:

* `--test-runtool <nextest> --test-runtool-arg __doctest-runtool`: the stable `--test-runtool` flag (intended for running cross-compiled doctests under an emulator) makes rustdoc hand each compiled binary to nextest rather than running it. Nextest hard links the binary into `target/nextest/doctests/compiled` and returns.
* `--test-args --test-threads=1`: this makes standalone binaries attributable, as described below.

To find out which tests a captured binary contains:

* **Merged binaries:** Rustdoc writes the generated runner source, `doctest_runner_<edition>.rs`, next to the binary. Nextest copies it and parses each `TestDescAndFn::new_doctest(name, ignore, file, line, no_run, should_panic, ...)` descriptor, along with the `__doctest_<index>` module it's in. The parser checks that the runner uses `RUSTDOC_DOCTEST_RUN_NB_TEST`, that every descriptor was parsed, and that indexes are contiguous from 0.
* **Standalone binaries:** With a single test thread, libtest prints `test <name> ... ` and flushes standard output before running each test. Nextest points rustdoc's standard output at a file, so when the runtool is invoked, the last line of that file names the test being run.

Some doctests are never handed to the runtool: `compile_fail` tests, standalone `no_run` tests, tests that fail to compile, and ignored tests. For those, nextest records the result and output that libtest printed. Because the runtool doesn't run anything, rustdoc reports standalone `should_panic` tests as failing with a known message ("Test executable succeeded, but it's marked `should_panic`"), which is how nextest learns which standalone tests are `should_panic`.

The result is a manifest per crate describing how to run each doctest.

### Run

A doctest suite's "binary" is its manifest, run through a hidden `__doctest-shim` subcommand. The shim accepts the subset of libtest arguments that nextest uses (`--list --format terse [--ignored]` and `--exact <name> --nocapture`), so the rest of nextest treats the suite like any other test binary. To run a test, the shim:

* For merged tests, runs the merged binary with `RUSTDOC_DOCTEST_RUN_NB_TEST=<index>`. This is the same path rustdoc's own runner uses for each test.
* For standalone tests, runs the binary.
* For recorded tests, prints the recorded output and exits with the recorded result.

On Unix, the shim `exec`s the doctest binary, so nextest observes the doctest's own exit status, signals, and resource usage. For `should_panic` tests, the shim instead waits for the binary and inverts its result, matching rustdoc: such a test passes if the process exits unsuccessfully for any reason.

## Limitations

* Doctests can't be [archived](../../ci-features/archiving.md) or used with reused builds.
* Target runners aren't supported: if Cargo passes `--test-runtool` to rustdoc, nextest reports an error.
* Ignored doctests are listed, but rustdoc doesn't compile them, so running them with `--run-ignored` reports a failure.
* Standalone doctests within a single crate are compiled one at a time, because of the single test thread. Merged doctests (the common case with the 2024 edition) are unaffected, and crates are still compiled in parallel.
* A `build.rustdoc` setting in Cargo configuration is overridden while capturing. The `RUSTDOC` environment variable is honored.
* The mechanism depends on rustdoc implementation details: the runner source format, `RUSTDOC_DOCTEST_RUN_NB_TEST`, and libtest's single-threaded output format. These have been stable for a while, and nextest validates them as described above, but a future Rust release could change them.
