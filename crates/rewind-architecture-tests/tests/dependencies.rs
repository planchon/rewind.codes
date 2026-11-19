//! The workspace follows the dependency rules of archi/07-code-architecture.md.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn metadata() -> Value {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = Command::new(env!("CARGO"))
        .args([
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--offline",
        ])
        .current_dir(workspace)
        .output()
        .expect("cargo metadata runs");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("cargo metadata prints JSON")
}

#[test]
fn the_workspace_follows_the_dependency_rules() {
    let violations = rewind_architecture_tests::check(&metadata());
    let report: Vec<String> = violations.iter().map(ToString::to_string).collect();
    assert!(
        report.is_empty(),
        "dependency rules broken:\n{}",
        report.join("\n")
    );
}

#[test]
fn every_crate_of_the_architecture_that_exists_is_checked() {
    let metadata = metadata();
    let names: Vec<&str> = metadata["packages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|package| package["name"].as_str())
        .collect();
    for expected in [
        "rewind-core",
        "rewind-engine",
        "rewind-format",
        "rewind-runtime",
        "rewind-api",
        "rewind-agent-tools",
        "rewind-mcp-server",
    ] {
        assert!(
            names.contains(&expected),
            "{expected} is missing from {names:?}"
        );
    }
}
