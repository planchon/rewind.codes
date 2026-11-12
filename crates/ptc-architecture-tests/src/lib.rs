//! The dependency rules of ai-ptc (`archi/07-code-architecture.md`, "Dependency rules"),
//! checked on the output of `cargo metadata`: tooling enforces the boundaries, not
//! discipline.
//!
//! Every crate of the workspace belongs to a [`Layer`]. A crate that is not classified fails
//! the check: adding a crate means deciding where it sits.

#![forbid(unsafe_code)]

use std::fmt;

use serde_json::Value;

/// Where a crate sits. Dependencies only point down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    /// `ptc-core`: the pure model.
    Domain,
    /// `ptc-engine`: use cases and ports.
    Application,
    /// Representations shared at the edges: the stored format (`ptc-format`), code generation
    /// (`ptc-codegen`), the HTTP API (`ptc-api`).
    AdapterSupport,
    /// Implementations of the ports.
    Adapter,
    /// The only crates that know every adapter.
    CompositionRoot,
    /// Tests and tools that wire adapters like a composition root.
    Tooling,
    /// `ptc`: the binary, which only hands the command line to a composition root.
    EntryPoint,
}

/// The layer of each crate of the workspace, current and planned.
pub fn layer(name: &str) -> Option<Layer> {
    Some(match name {
        "ptc-core" => Layer::Domain,
        "ptc-engine" => Layer::Application,
        "ptc-format" | "ptc-codegen" | "ptc-api" => Layer::AdapterSupport,
        "ptc-runtime" | "ptc-store-turso" | "ptc-store-postgres" | "ptc-blob" | "ptc-mcp"
        | "ptc-check" | "ptc-otel" => Layer::Adapter,
        "ptc-cli" | "ptc-server" => Layer::CompositionRoot,
        "ptc-architecture-tests" | "ptc-e2e" | "ptc-load" | "evals" => Layer::Tooling,
        "ptc" => Layer::EntryPoint,
        _ => return None,
    })
}

/// External crates `ptc-core` may depend on.
pub const CORE_DEPENDENCIES: &[&str] = &["ryu-js", "serde_json", "sha2", "thiserror", "uuid"];

/// One broken rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub krate: String,
    pub rule: &'static str,
    pub detail: String,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {} ({})", self.krate, self.rule, self.detail)
    }
}

/// A declared dependency of a workspace crate.
#[derive(Debug, Clone)]
struct Dependency {
    name: String,
    /// `normal`, `dev` or `build`.
    kind: String,
    features: Vec<String>,
    default_features: bool,
}

/// Checks every rule on `cargo metadata --format-version 1 --no-deps` output.
pub fn check(metadata: &Value) -> Vec<Violation> {
    let mut violations = Vec::new();
    let packages = metadata["packages"].as_array().cloned().unwrap_or_default();
    let members: Vec<String> = packages
        .iter()
        .filter_map(|package| package["name"].as_str().map(str::to_owned))
        .collect();

    for package in &packages {
        let Some(name) = package["name"].as_str() else {
            continue;
        };
        let mut violation = |rule: &'static str, detail: String| {
            violations.push(Violation {
                krate: name.to_owned(),
                rule,
                detail,
            });
        };
        let Some(own_layer) = layer(name) else {
            violation(
                "every crate belongs to a layer",
                "classify it in ptc-architecture-tests".into(),
            );
            continue;
        };
        let dependencies = dependencies(package);

        for dependency in &dependencies {
            let target = dependency.name.as_str();
            let internal = members.iter().any(|member| member == target) || layer(target).is_some();

            if internal {
                if target == name {
                    // A crate may enable its own features in its tests (the conformance suite).
                    continue;
                }
                if let Some(rule) = internal_rule(name, own_layer, target) {
                    violation(rule, format!("depends on {target} ({})", dependency.kind));
                }
                continue;
            }

            if target.starts_with("opentelemetry") && name != "ptc-otel" {
                violation(
                    "OpenTelemetry lives only in ptc-otel",
                    format!("depends on {target}"),
                );
            }
            let telemetry = target == "tracing"
                || target.starts_with("tracing-")
                || target.starts_with("opentelemetry");
            if telemetry && matches!(own_layer, Layer::Domain | Layer::AdapterSupport) {
                violation(
                    "the tracing facade is allowed in the engine and the adapters only",
                    format!("depends on {target}"),
                );
            }
            if target == "anyhow"
                && !matches!(
                    own_layer,
                    Layer::CompositionRoot | Layer::Tooling | Layer::EntryPoint
                )
                && dependency.kind != "dev"
            {
                violation(
                    "libraries have their own error types: no anyhow",
                    "depends on anyhow".into(),
                );
            }
            if own_layer == Layer::Domain && dependency.kind != "dev" {
                if !CORE_DEPENDENCIES.contains(&target) {
                    violation(
                        "ptc-core depends only on ryu-js, serde_json, sha2, thiserror and uuid",
                        format!("depends on {target}"),
                    );
                }
                if target == "uuid"
                    && (!dependency.features.is_empty() || dependency.default_features)
                {
                    violation(
                        "ptc-core uses uuid without features",
                        format!(
                            "features {:?}, default features {}",
                            dependency.features, dependency.default_features
                        ),
                    );
                }
            }
        }
    }
    violations
}

/// The rule an internal dependency breaks, if any.
fn internal_rule(from: &str, from_layer: Layer, to: &str) -> Option<&'static str> {
    let to_layer = layer(to)?;
    if to_layer == Layer::EntryPoint {
        return Some("nothing depends on the ptc binary");
    }
    match from_layer {
        Layer::Domain => Some("ptc-core depends on no internal crate"),
        Layer::Application => (to != "ptc-core").then_some("ptc-engine depends only on ptc-core"),
        Layer::AdapterSupport => {
            (to != "ptc-core").then_some("adapter support crates depend only on ptc-core")
        }
        Layer::Adapter => {
            let allowed = match to_layer {
                Layer::Domain | Layer::Application => true,
                // Stores write the stored journal format.
                Layer::AdapterSupport => {
                    to == "ptc-format" && from.starts_with("ptc-store-")
                        || to == "ptc-codegen" && matches!(from, "ptc-runtime" | "ptc-check")
                }
                Layer::Adapter | Layer::CompositionRoot | Layer::Tooling | Layer::EntryPoint => {
                    false
                }
            };
            (!allowed).then_some(
                "adapters depend on ptc-core and ptc-engine (stores also on ptc-format), never on each other",
            )
        }
        Layer::CompositionRoot | Layer::Tooling => {
            matches!(to_layer, Layer::CompositionRoot | Layer::Tooling)
                .then_some("nothing depends on a composition root or a tool")
        }
        Layer::EntryPoint => (to_layer != Layer::CompositionRoot)
            .then_some("the ptc binary depends only on ptc-cli and ptc-server"),
    }
}

fn dependencies(package: &Value) -> Vec<Dependency> {
    package["dependencies"]
        .as_array()
        .map(|dependencies| {
            dependencies
                .iter()
                .map(|dependency| Dependency {
                    name: dependency["name"].as_str().unwrap_or_default().to_owned(),
                    kind: dependency["kind"].as_str().unwrap_or("normal").to_owned(),
                    features: dependency["features"]
                        .as_array()
                        .map(|features| {
                            features
                                .iter()
                                .filter_map(|feature| feature.as_str().map(str::to_owned))
                                .collect()
                        })
                        .unwrap_or_default(),
                    default_features: dependency["uses_default_features"]
                        .as_bool()
                        .unwrap_or(true),
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn dependency(name: &str, kind: Option<&str>) -> Value {
        json!({ "name": name, "kind": kind, "features": [], "uses_default_features": false })
    }

    fn metadata(packages: Vec<(&str, Vec<Value>)>) -> Value {
        json!({
            "packages": packages
                .into_iter()
                .map(|(name, dependencies)| json!({ "name": name, "dependencies": dependencies }))
                .collect::<Vec<_>>()
        })
    }

    fn rules(metadata: &Value) -> Vec<&'static str> {
        check(metadata)
            .into_iter()
            .map(|violation| violation.rule)
            .collect()
    }

    #[test]
    fn a_clean_layering_passes() {
        let clean = metadata(vec![
            (
                "ptc-core",
                vec![dependency("serde_json", None), dependency("uuid", None)],
            ),
            (
                "ptc-engine",
                vec![dependency("ptc-core", None), dependency("tracing", None)],
            ),
            ("ptc-format", vec![dependency("ptc-core", None)]),
            (
                "ptc-runtime",
                vec![dependency("ptc-core", None), dependency("ptc-engine", None)],
            ),
            ("ptc-store-turso", vec![dependency("ptc-format", None)]),
            (
                "ptc-store-postgres",
                vec![dependency("ptc-format", None), dependency("sqlx", None)],
            ),
            (
                "ptc-e2e",
                vec![dependency("ptc-runtime", None), dependency("anyhow", None)],
            ),
            ("ptc-api", vec![dependency("ptc-core", None)]),
            (
                "ptc-cli",
                vec![dependency("ptc-api", None), dependency("ptc-runtime", None)],
            ),
            (
                "ptc-server",
                vec![dependency("ptc-api", None), dependency("ptc-engine", None)],
            ),
            (
                "ptc",
                vec![
                    dependency("ptc-cli", None),
                    dependency("ptc-server", None),
                    dependency("anyhow", None),
                ],
            ),
        ]);
        assert_eq!(check(&clean), []);
    }

    /// Each rule catches the violation it is about.
    #[test]
    fn every_rule_catches_its_violation() {
        let cases: Vec<(&str, Vec<Value>, &str)> = vec![
            (
                "ptc-core",
                vec![dependency("ptc-format", None)],
                "ptc-core depends on no internal crate",
            ),
            (
                "ptc-core",
                vec![dependency("serde", None)],
                "ptc-core depends only on ryu-js, serde_json, sha2, thiserror and uuid",
            ),
            (
                "ptc-core",
                vec![
                    json!({ "name": "uuid", "kind": null, "features": ["v7"], "uses_default_features": false }),
                ],
                "ptc-core uses uuid without features",
            ),
            (
                "ptc-core",
                vec![dependency("tracing", None)],
                "the tracing facade is allowed in the engine and the adapters only",
            ),
            (
                "ptc-engine",
                vec![dependency("ptc-runtime", Some("dev"))],
                "ptc-engine depends only on ptc-core",
            ),
            (
                "ptc-format",
                vec![dependency("ptc-engine", None)],
                "adapter support crates depend only on ptc-core",
            ),
            (
                "ptc-runtime",
                vec![dependency("ptc-format", None)],
                "adapters depend on ptc-core and ptc-engine (stores also on ptc-format), never on each other",
            ),
            (
                "ptc-store-turso",
                vec![dependency("ptc-blob", None)],
                "adapters depend on ptc-core and ptc-engine (stores also on ptc-format), never on each other",
            ),
            (
                "ptc-store-postgres",
                vec![dependency("ptc-store-turso", None)],
                "adapters depend on ptc-core and ptc-engine (stores also on ptc-format), never on each other",
            ),
            (
                "ptc-engine",
                vec![dependency("opentelemetry", None)],
                "OpenTelemetry lives only in ptc-otel",
            ),
            (
                "ptc-engine",
                vec![dependency("anyhow", None)],
                "libraries have their own error types: no anyhow",
            ),
            (
                "ptc-cli",
                vec![dependency("ptc-e2e", None)],
                "nothing depends on a composition root or a tool",
            ),
            ("ptc-mystery", vec![], "every crate belongs to a layer"),
            (
                "ptc-api",
                vec![dependency("ptc-engine", None)],
                "adapter support crates depend only on ptc-core",
            ),
            (
                "ptc-runtime",
                vec![dependency("ptc-api", None)],
                "adapters depend on ptc-core and ptc-engine (stores also on ptc-format), never on each other",
            ),
            (
                "ptc-engine",
                vec![dependency("ptc-api", None)],
                "ptc-engine depends only on ptc-core",
            ),
            (
                "ptc-cli",
                vec![dependency("ptc-server", None)],
                "nothing depends on a composition root or a tool",
            ),
            (
                "ptc",
                vec![dependency("ptc-runtime", None)],
                "the ptc binary depends only on ptc-cli and ptc-server",
            ),
            (
                "ptc",
                vec![dependency("ptc-api", None)],
                "the ptc binary depends only on ptc-cli and ptc-server",
            ),
            (
                "ptc-cli",
                vec![dependency("ptc", None)],
                "nothing depends on the ptc binary",
            ),
        ];
        for (krate, dependencies, rule) in cases {
            let found = rules(&metadata(vec![(krate, dependencies)]));
            assert!(
                found.contains(&rule),
                "{krate}: expected {rule:?}, found {found:?}"
            );
        }
    }
}
