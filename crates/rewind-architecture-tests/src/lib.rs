//! The dependency rules of Rewind (`archi/07-code-architecture.md`, "Dependency rules"),
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
    /// `rewind-core`: the pure model.
    Domain,
    /// `rewind-engine`: use cases and ports.
    Application,
    /// Representations shared at the edges: the stored format (`rewind-format`), code generation
    /// (`rewind-codegen`), the HTTP API (`rewind-api`), what leaves Rewind for a tenant's servers
    /// (`rewind-outbound`); and what is built on the API alone: the agent tools
    /// (`rewind-agent-tools`) and Rewind's MCP server (`rewind-mcp-server`).
    AdapterSupport,
    /// Implementations of the ports.
    Adapter,
    /// The only crates that know every adapter.
    CompositionRoot,
    /// Tests and tools that wire adapters like a composition root.
    Tooling,
    /// `rewind`: the binary, which only hands the command line to a composition root.
    EntryPoint,
}

/// The layer of each crate of the workspace, current and planned.
pub fn layer(name: &str) -> Option<Layer> {
    Some(match name {
        "rewind-core" => Layer::Domain,
        "rewind-engine" => Layer::Application,
        "rewind-format" | "rewind-codegen" | "rewind-api" | "rewind-outbound"
        | "rewind-agent-tools" | "rewind-mcp-server" => Layer::AdapterSupport,
        "rewind-runtime"
        | "rewind-store-turso"
        | "rewind-store-postgres"
        | "rewind-store-slatedb"
        | "rewind-blob"
        | "rewind-workos"
        | "rewind-mcp"
        | "rewind-openapi"
        | "rewind-check"
        | "rewind-otel" => Layer::Adapter,
        "rewind-cli" | "rewind-server" => Layer::CompositionRoot,
        "rewind-architecture-tests" | "rewind-e2e" | "rewind-load" | "evals" => Layer::Tooling,
        "rewind" => Layer::EntryPoint,
        _ => return None,
    })
}

/// External crates `rewind-core` may depend on.
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
                "classify it in rewind-architecture-tests".into(),
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

            if target.starts_with("opentelemetry") && name != "rewind-otel" {
                violation(
                    "OpenTelemetry lives only in rewind-otel",
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
                        "rewind-core depends only on ryu-js, serde_json, sha2, thiserror and uuid",
                        format!("depends on {target}"),
                    );
                }
                if target == "uuid"
                    && (!dependency.features.is_empty() || dependency.default_features)
                {
                    violation(
                        "rewind-core uses uuid without features",
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
        return Some("nothing depends on the rewind binary");
    }
    match from_layer {
        Layer::Domain => Some("rewind-core depends on no internal crate"),
        Layer::Application => {
            (to != "rewind-core").then_some("rewind-engine depends only on rewind-core")
        }
        Layer::AdapterSupport => {
            let allowed = match from {
                // The agent tools reach Rewind through the operations of `/v1`.
                "rewind-agent-tools" => matches!(to, "rewind-core" | "rewind-api"),
                "rewind-mcp-server" => {
                    matches!(to, "rewind-core" | "rewind-api" | "rewind-agent-tools")
                }
                _ => to == "rewind-core",
            };
            (!allowed).then_some(
                "adapter support crates depend only on rewind-core (the agent tools also on \
                 rewind-api, the MCP server also on them)",
            )
        }
        Layer::Adapter => {
            let allowed = match to_layer {
                Layer::Domain | Layer::Application => true,
                // Stores and the archive (rewind-blob) write the stored journal format; the
                // tool dispatchers send through rewind-outbound.
                Layer::AdapterSupport => {
                    to == "rewind-format"
                        && (from.starts_with("rewind-store-") || from == "rewind-blob")
                        || to == "rewind-codegen"
                            && matches!(from, "rewind-runtime" | "rewind-check")
                        || to == "rewind-outbound"
                            && matches!(from, "rewind-mcp" | "rewind-openapi")
                }
                Layer::Adapter | Layer::CompositionRoot | Layer::Tooling | Layer::EntryPoint => {
                    false
                }
            };
            (!allowed).then_some(
                "adapters depend on rewind-core and rewind-engine (stores also on rewind-format, the tool dispatchers also on rewind-outbound), never on each other",
            )
        }
        Layer::CompositionRoot | Layer::Tooling => {
            matches!(to_layer, Layer::CompositionRoot | Layer::Tooling)
                .then_some("nothing depends on a composition root or a tool")
        }
        Layer::EntryPoint => (to_layer != Layer::CompositionRoot)
            .then_some("the rewind binary depends only on rewind-cli and rewind-server"),
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
                "rewind-core",
                vec![dependency("serde_json", None), dependency("uuid", None)],
            ),
            (
                "rewind-engine",
                vec![dependency("rewind-core", None), dependency("tracing", None)],
            ),
            ("rewind-format", vec![dependency("rewind-core", None)]),
            (
                "rewind-runtime",
                vec![
                    dependency("rewind-core", None),
                    dependency("rewind-engine", None),
                ],
            ),
            (
                "rewind-store-turso",
                vec![dependency("rewind-format", None)],
            ),
            (
                "rewind-blob",
                vec![
                    dependency("rewind-engine", None),
                    dependency("rewind-format", None),
                ],
            ),
            ("rewind-outbound", vec![dependency("rewind-core", None)]),
            (
                "rewind-openapi",
                vec![
                    dependency("rewind-core", None),
                    dependency("rewind-engine", None),
                    dependency("rewind-outbound", None),
                ],
            ),
            ("rewind-mcp", vec![dependency("rewind-outbound", None)]),
            (
                "rewind-store-postgres",
                vec![dependency("rewind-format", None), dependency("sqlx", None)],
            ),
            (
                "rewind-store-slatedb",
                vec![
                    dependency("rewind-core", None),
                    dependency("rewind-engine", None),
                    dependency("rewind-format", None),
                    dependency("slatedb", None),
                ],
            ),
            (
                "rewind-e2e",
                vec![
                    dependency("rewind-runtime", None),
                    dependency("anyhow", None),
                ],
            ),
            ("rewind-api", vec![dependency("rewind-core", None)]),
            (
                "rewind-agent-tools",
                vec![
                    dependency("rewind-core", None),
                    dependency("rewind-api", None),
                ],
            ),
            (
                "rewind-mcp-server",
                vec![
                    dependency("rewind-core", None),
                    dependency("rewind-api", None),
                    dependency("rewind-agent-tools", None),
                ],
            ),
            (
                "rewind-cli",
                vec![
                    dependency("rewind-api", None),
                    dependency("rewind-runtime", None),
                    dependency("rewind-mcp-server", None),
                ],
            ),
            ("evals", vec![dependency("rewind-agent-tools", None)]),
            (
                "rewind-server",
                vec![
                    dependency("rewind-api", None),
                    dependency("rewind-engine", None),
                ],
            ),
            (
                "rewind",
                vec![
                    dependency("rewind-cli", None),
                    dependency("rewind-server", None),
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
                "rewind-core",
                vec![dependency("rewind-format", None)],
                "rewind-core depends on no internal crate",
            ),
            (
                "rewind-core",
                vec![dependency("serde", None)],
                "rewind-core depends only on ryu-js, serde_json, sha2, thiserror and uuid",
            ),
            (
                "rewind-core",
                vec![
                    json!({ "name": "uuid", "kind": null, "features": ["v7"], "uses_default_features": false }),
                ],
                "rewind-core uses uuid without features",
            ),
            (
                "rewind-core",
                vec![dependency("tracing", None)],
                "the tracing facade is allowed in the engine and the adapters only",
            ),
            (
                "rewind-engine",
                vec![dependency("rewind-runtime", Some("dev"))],
                "rewind-engine depends only on rewind-core",
            ),
            (
                "rewind-format",
                vec![dependency("rewind-engine", None)],
                "adapter support crates depend only on rewind-core (the agent tools also on rewind-api, the MCP server also on them)",
            ),
            (
                "rewind-runtime",
                vec![dependency("rewind-format", None)],
                "adapters depend on rewind-core and rewind-engine (stores also on rewind-format, the tool dispatchers also on rewind-outbound), never on each other",
            ),
            (
                "rewind-store-turso",
                vec![dependency("rewind-blob", None)],
                "adapters depend on rewind-core and rewind-engine (stores also on rewind-format, the tool dispatchers also on rewind-outbound), never on each other",
            ),
            (
                "rewind-blob",
                vec![dependency("rewind-codegen", None)],
                "adapters depend on rewind-core and rewind-engine (stores also on rewind-format, the tool dispatchers also on rewind-outbound), never on each other",
            ),
            (
                "rewind-store-postgres",
                vec![dependency("rewind-store-turso", None)],
                "adapters depend on rewind-core and rewind-engine (stores also on rewind-format, the tool dispatchers also on rewind-outbound), never on each other",
            ),
            (
                "rewind-store-slatedb",
                vec![dependency("rewind-blob", None)],
                "adapters depend on rewind-core and rewind-engine (stores also on rewind-format, the tool dispatchers also on rewind-outbound), never on each other",
            ),
            (
                "rewind-engine",
                vec![dependency("opentelemetry", None)],
                "OpenTelemetry lives only in rewind-otel",
            ),
            (
                "rewind-engine",
                vec![dependency("anyhow", None)],
                "libraries have their own error types: no anyhow",
            ),
            (
                "rewind-cli",
                vec![dependency("rewind-e2e", None)],
                "nothing depends on a composition root or a tool",
            ),
            ("rewind-mystery", vec![], "every crate belongs to a layer"),
            (
                "rewind-api",
                vec![dependency("rewind-engine", None)],
                "adapter support crates depend only on rewind-core (the agent tools also on rewind-api, the MCP server also on them)",
            ),
            (
                "rewind-agent-tools",
                vec![dependency("rewind-engine", None)],
                "adapter support crates depend only on rewind-core (the agent tools also on rewind-api, the MCP server also on them)",
            ),
            (
                "rewind-agent-tools",
                vec![dependency("rewind-mcp-server", None)],
                "adapter support crates depend only on rewind-core (the agent tools also on rewind-api, the MCP server also on them)",
            ),
            (
                "rewind-mcp-server",
                vec![dependency("rewind-engine", None)],
                "adapter support crates depend only on rewind-core (the agent tools also on rewind-api, the MCP server also on them)",
            ),
            (
                "rewind-api",
                vec![dependency("rewind-agent-tools", None)],
                "adapter support crates depend only on rewind-core (the agent tools also on rewind-api, the MCP server also on them)",
            ),
            (
                "rewind-mcp",
                vec![dependency("rewind-agent-tools", None)],
                "adapters depend on rewind-core and rewind-engine (stores also on rewind-format, the tool dispatchers also on rewind-outbound), never on each other",
            ),
            (
                "rewind-outbound",
                vec![dependency("rewind-engine", None)],
                "adapter support crates depend only on rewind-core (the agent tools also on rewind-api, the MCP server also on them)",
            ),
            (
                "rewind-runtime",
                vec![dependency("rewind-outbound", None)],
                "adapters depend on rewind-core and rewind-engine (stores also on rewind-format, the tool dispatchers also on rewind-outbound), never on each other",
            ),
            (
                "rewind-openapi",
                vec![dependency("rewind-mcp", None)],
                "adapters depend on rewind-core and rewind-engine (stores also on rewind-format, the tool dispatchers also on rewind-outbound), never on each other",
            ),
            (
                "rewind-runtime",
                vec![dependency("rewind-api", None)],
                "adapters depend on rewind-core and rewind-engine (stores also on rewind-format, the tool dispatchers also on rewind-outbound), never on each other",
            ),
            (
                "rewind-engine",
                vec![dependency("rewind-api", None)],
                "rewind-engine depends only on rewind-core",
            ),
            (
                "rewind-cli",
                vec![dependency("rewind-server", None)],
                "nothing depends on a composition root or a tool",
            ),
            (
                "rewind",
                vec![dependency("rewind-runtime", None)],
                "the rewind binary depends only on rewind-cli and rewind-server",
            ),
            (
                "rewind",
                vec![dependency("rewind-api", None)],
                "the rewind binary depends only on rewind-cli and rewind-server",
            ),
            (
                "rewind-cli",
                vec![dependency("rewind", None)],
                "nothing depends on the rewind binary",
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
