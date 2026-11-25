// Copyright (c) The nextest Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Support for running doctests with nextest.
//!
//! Stable Rust offers no supported way to compile doctests ahead of time and
//! run them later (`--persist-doctests` and `--no-run` are unstable). Nextest
//! works around that using the stable `--test-runtool` rustdoc flag:
//!
//! 1. **Capture.** `cargo test --doc` is run with `RUSTDOC` pointing at the
//!    nextest binary. Instead of running doctests, nextest records each rustdoc
//!    invocation (arguments, working directory, and environment) that Cargo
//!    would have made. See [`record_rustdoc_invocation`].
//! 2. **Compile.** For each crate, in parallel, nextest replays the recorded
//!    invocation with `--test-runtool` pointing at itself. Rustdoc compiles the
//!    doctests and hands every compiled binary to the runtool, which copies it
//!    into the target directory instead of running it. See
//!    [`DoctestCompiler`] and [`run_runtool`].
//! 3. **Run.** Each crate gets a [`DoctestManifest`] describing how to run
//!    every doctest. A small shim (see [`run_shim`]) presents the manifest to
//!    the rest of nextest as if it were a libtest binary, so each doctest
//!    becomes an ordinary nextest test that is scheduled, retried, and reported
//!    like any other.
//!
//! Rustdoc produces two kinds of binaries:
//!
//! * **Merged** binaries (Rust 2024 edition and later) contain many doctests.
//!   An individual test is selected with the `RUSTDOC_DOCTEST_RUN_NB_TEST`
//!   environment variable, which is the mechanism rustdoc itself uses to run
//!   each merged doctest in its own process. The mapping from test names to
//!   indexes, along with the `should_panic` and `no_run` attributes, is read
//!   from the runner source that rustdoc generates next to the binary.
//! * **Standalone** binaries contain one doctest each. To learn which test a
//!   binary belongs to, rustdoc is run with a single test thread, so the last
//!   line libtest has written to standard output (`test <name> ... `) names the
//!   test currently being run.
//!
//! Doctests that are never handed to the runtool (`compile_fail` tests,
//! standalone `no_run` tests, and tests that fail to compile) have their
//! outcome recorded at compile time and replayed when the test is run.
//!
//! These mechanisms rely on details of rustdoc's implementation, so every
//! step validates what it sees and fails loudly rather than guessing.

mod artifact;
mod capture;
mod compile;
mod manifest;
mod parse;
mod shim;

pub use capture::*;
pub use compile::*;
pub use manifest::*;
pub use shim::*;
