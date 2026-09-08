// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Fabian Schmieder

//! What a released binary is built from.
//!
//! The release used to build with `--all-features`, which also enabled
//! `testutil`. That feature exists for the integration tests and reaches them
//! through the dev-dependency on this crate; it is 394 lines of mock serial
//! port and data generator.
//!
//! Measured, the linker discards it, so this buys no bytes: no string from
//! `testutil` survives in a binary built with `--all-features`. The reason is
//! provenance rather than size. Test scaffolding has no business being
//! compiled into an artefact that is signed with a Developer ID and notarized
//! by Apple, and relying on the optimiser to drop it is a guarantee nobody
//! wrote down.
//!
//! The release now builds `--features full`. These tests keep that honest: a
//! new product feature missing from `full` would silently drop out of every
//! published archive, and nothing else would notice.

const MANIFEST: &str = include_str!("../Cargo.toml");
const RELEASE_WORKFLOW: &str = include_str!("../.github/workflows/release.yml");
const PACKAGE_METADATA: &str = include_str!("../scripts/release/package-metadata.sh");

/// The features declared in `[features]`, with what each pulls in.
fn declared_features() -> Vec<(String, String)> {
    let mut features = Vec::new();
    let mut inside = false;
    for line in MANIFEST.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            inside = line == "[features]";
            continue;
        }
        if !inside || line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((name, value)) = line.split_once('=') {
            features.push((name.trim().to_string(), value.trim().to_string()));
        }
    }
    assert!(!features.is_empty(), "no [features] section was parsed");
    features
}

#[test]
fn full_covers_every_product_feature() {
    let features = declared_features();
    let full = features
        .iter()
        .find(|(name, _)| name == "full")
        .map(|(_, value)| value.clone())
        .expect("the manifest declares a `full` feature");

    for (name, _) in &features {
        // `default` and `full` are groupings; `testutil` is deliberately out.
        if matches!(name.as_str(), "default" | "full" | "testutil") {
            continue;
        }
        assert!(
            full.contains(&format!("\"{name}\"")),
            "the feature `{name}` is not in `full`, so no published binary would carry it"
        );
    }
}

#[test]
fn full_never_carries_the_test_scaffolding() {
    let features = declared_features();
    let full = features
        .iter()
        .find(|(name, _)| name == "full")
        .map(|(_, value)| value.clone())
        .expect("the manifest declares a `full` feature");
    assert!(
        !full.contains("testutil"),
        "`full` pulls in the test scaffolding, which is the thing it exists to avoid"
    );
}

#[test]
fn the_release_builds_from_full_and_not_from_all_features() {
    for line in RELEASE_WORKFLOW.lines() {
        let line = line.trim();
        if !line.starts_with("run: cargo build --release") {
            continue;
        }
        assert!(
            !line.contains("--all-features"),
            "a release build uses --all-features, which enables `testutil`: {line}"
        );
        assert!(
            line.contains("--features full"),
            "a release build does not ask for `full`: {line}"
        );
    }
}

/// The Homebrew `desc` and the AUR `pkgdesc` come from a hand-written copy of
/// the package description in `scripts/release/package-metadata.sh`. Nothing
/// reads one from the other, so a change on one side leaves the packages
/// describing a program that no longer matches the one they install.
#[test]
fn the_packaging_description_matches_the_manifest() {
    let manifest = MANIFEST
        .lines()
        .find_map(|line| line.strip_prefix("description = \""))
        .and_then(|rest| rest.strip_suffix('"'))
        .expect("Cargo.toml has no description");

    let packaged = PACKAGE_METADATA
        .lines()
        .find_map(|line| line.strip_prefix("DESCRIPTION='"))
        .and_then(|rest| rest.strip_suffix('\''))
        .expect("package-metadata.sh has no DESCRIPTION");

    assert_eq!(
        manifest, packaged,
        "the packaged description has drifted from Cargo.toml"
    );
}
