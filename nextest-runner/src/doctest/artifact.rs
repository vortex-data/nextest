// Copyright (c) The nextest Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Turning compiled doctests into test artifacts.

use super::{capture::DOCTEST_SHIM_SUBCOMMAND, compile::CompiledDoctests};
use crate::{errors::DoctestError, list::RustTestArtifact};
use camino::Utf8Path;
use guppy::graph::PackageGraph;
use nextest_metadata::{BuildPlatform, RustBinaryId, RustTestBinaryKind};
use std::collections::BTreeSet;

impl<'g> RustTestArtifact<'g> {
    /// Creates test artifacts for compiled doctests.
    ///
    /// Each crate becomes a suite of kind [`RustTestBinaryKind::DOCTEST`],
    /// whose "binary" is the crate's manifest, run through `nextest_exe`'s
    /// doctest shim.
    pub fn from_doctests(
        graph: &'g PackageGraph,
        compiled: &[CompiledDoctests],
        nextest_exe: &Utf8Path,
    ) -> Result<Vec<Self>, DoctestError> {
        compiled
            .iter()
            .map(|c| {
                let manifest = &c.manifest;
                let package = graph
                    .packages()
                    .find(|package| package.manifest_path() == manifest.manifest_path)
                    .ok_or_else(|| DoctestError::PackageNotFound {
                        crate_name: manifest.crate_name.clone(),
                        manifest_path: manifest.manifest_path.clone(),
                    })?;
                let cwd = manifest
                    .manifest_path
                    .parent()
                    .expect("manifest path has a parent")
                    .to_owned();
                Ok(RustTestArtifact {
                    binary_id: RustBinaryId::from_parts(
                        package.name(),
                        &RustTestBinaryKind::DOCTEST,
                        &manifest.crate_name,
                    ),
                    package,
                    binary_path: c.manifest_path.clone(),
                    binary_name: manifest.crate_name.clone(),
                    kind: RustTestBinaryKind::DOCTEST,
                    non_test_binaries: BTreeSet::new(),
                    cwd,
                    build_platform: BuildPlatform::Target,
                    exec_prefix: vec![
                        nextest_exe.as_str().to_owned(),
                        DOCTEST_SHIM_SUBCOMMAND.to_owned(),
                    ],
                })
            })
            .collect()
    }
}
