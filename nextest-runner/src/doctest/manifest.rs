// Copyright (c) The nextest Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The per-crate doctest manifest.

use crate::errors::DoctestError;
use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use std::fs;

/// The current version of the manifest format.
///
/// Manifests are produced and consumed by the same nextest binary within a
/// single run, so there is no compatibility story: a mismatch is an error.
pub const DOCTEST_MANIFEST_VERSION: u32 = 1;

/// Describes how to run each doctest in a crate.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct DoctestManifest {
    /// The format version; see [`DOCTEST_MANIFEST_VERSION`].
    pub version: u32,

    /// The name of the crate the doctests belong to.
    pub crate_name: String,

    /// The path to the `Cargo.toml` of the package.
    pub manifest_path: Utf8PathBuf,

    /// The doctests, sorted by name.
    pub tests: Vec<DoctestEntry>,
}

impl DoctestManifest {
    /// Reads a manifest from disk.
    pub fn read(path: &Utf8Path) -> Result<Self, DoctestError> {
        let contents = fs::read(path).map_err(|error| DoctestError::Io {
            path: path.to_owned(),
            error,
        })?;
        let manifest: Self =
            serde_json::from_slice(&contents).map_err(|error| DoctestError::Json {
                path: path.to_owned(),
                error,
            })?;
        if manifest.version != DOCTEST_MANIFEST_VERSION {
            return Err(DoctestError::ShimArgs {
                reason: format!(
                    "manifest `{path}` has version {}, expected {DOCTEST_MANIFEST_VERSION} \
                     (was it produced by a different nextest?)",
                    manifest.version
                ),
            });
        }
        Ok(manifest)
    }

    /// Writes this manifest to disk.
    pub fn write(&self, path: &Utf8Path) -> Result<(), DoctestError> {
        let json = serde_json::to_vec_pretty(self).map_err(|error| DoctestError::Json {
            path: path.to_owned(),
            error,
        })?;
        fs::write(path, json).map_err(|error| DoctestError::Io {
            path: path.to_owned(),
            error,
        })
    }

    /// Looks up a test by name.
    pub fn get(&self, name: &str) -> Option<&DoctestEntry> {
        self.tests
            .binary_search_by(|entry| entry.name.as_str().cmp(name))
            .ok()
            .map(|index| &self.tests[index])
    }
}

/// A single doctest.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct DoctestEntry {
    /// The test name, as reported by rustdoc (e.g. `src/lib.rs - foo (line 3)`).
    pub name: String,

    /// Whether the test is marked `ignore`.
    pub ignored: bool,

    /// How to run the test.
    pub action: DoctestAction,
}

/// How to run a doctest.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum DoctestAction {
    /// Run test number `index` of a merged doctest binary.
    Merged {
        /// The merged binary.
        binary: Utf8PathBuf,

        /// The index of the test within the binary.
        index: usize,

        /// Whether the test is expected to panic.
        should_panic: bool,

        /// Whether the test is compile-only.
        no_run: bool,
    },

    /// Run a binary containing just this test.
    Standalone {
        /// The binary.
        binary: Utf8PathBuf,

        /// Whether the test is expected to panic.
        should_panic: bool,
    },

    /// The test was never run as a binary (for example, `compile_fail` tests
    /// or tests that failed to compile); replay the outcome rustdoc reported.
    Recorded {
        /// Whether rustdoc reported success.
        passed: bool,

        /// The output rustdoc reported for this test.
        output: String,
    },

    /// Nextest could not determine how to run this test.
    Unavailable {
        /// Why the test is unavailable.
        reason: String,
    },
}
