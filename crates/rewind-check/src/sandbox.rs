//! The sandbox lib: what tsgo is given next to the script (`archi/04-tools.md`,
//! "Type-checking"). The files live in `crates/rewind-runtime/lib/`, which describes what the
//! runtime has; two adapters cannot depend on each other, so they are embedded by path.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use serde_json::Value;

/// The `rewind` module.
const REWIND_D_TS: &str = include_str!("../../rewind-runtime/lib/rewind.d.ts");
/// The sandbox's globals beyond ECMAScript.
const GLOBALS_D_TS: &str = include_str!("../../rewind-runtime/lib/globals.d.ts");
/// `fetch`: only for an action set that enables the `http` action.
const FETCH_D_TS: &str = include_str!("../../rewind-runtime/lib/fetch.d.ts");
/// The old names code already written still uses (`archi/08-decisions.md`, R1): never shown.
const OLD_NAMES_D_TS: &str = include_str!("../../rewind-runtime/lib/old-names.d.ts");
/// tsgo's options: strict, the ECMAScript lib of the sandbox, every `*.ts` of the directory.
const TSCONFIG_JSON: &str = include_str!("../../rewind-runtime/lib/tsconfig.json");
/// The declared names and the hints for absent globals.
const GLOBALS_JSON: &str = include_str!("../../rewind-runtime/lib/globals.json");

/// Where the script is written, and the name diagnostics give it.
pub(crate) const SCRIPT: &str = "script.ts";
/// Where the action set's declarations are written.
pub(crate) const ACTIONS: &str = "rewind-actions.d.ts";
/// The declaration file of `fetch`, as the action set's declarations reference it.
pub(crate) const FETCH: &str = "fetch.d.ts";
/// Where a workflow's own declarations are written: its signals.
pub(crate) const WORKFLOW: &str = "rewind-workflow.d.ts";
/// Where a workflow's entry is written: its default export against its schemas.
pub(crate) const ENTRY: &str = "entry.ts";

/// The files written next to the script and the action set's declarations, by name.
pub(crate) fn files(http_action: bool) -> Vec<(&'static str, &'static str)> {
    let mut files = vec![
        ("tsconfig.json", TSCONFIG_JSON),
        ("rewind.d.ts", REWIND_D_TS),
        ("globals.d.ts", GLOBALS_D_TS),
        ("old-names.d.ts", OLD_NAMES_D_TS),
    ];
    if http_action {
        files.push((FETCH, FETCH_D_TS));
    }
    files
}

/// The globals models reach for by habit but that the sandbox does not have, with what to use
/// instead: `fetch`, `require`, `process`, `Buffer`.
pub(crate) fn absent_globals() -> &'static BTreeMap<String, String> {
    static ABSENT: LazyLock<BTreeMap<String, String>> = LazyLock::new(|| {
        let document: Value = serde_json::from_str(GLOBALS_JSON)
            .unwrap_or_else(|error| panic!("rewind-runtime/lib/globals.json: {error}"));
        document["absent"]
            .as_object()
            .map(|absent| {
                absent
                    .iter()
                    .filter_map(|(name, hint)| Some((name.clone(), hint.as_str()?.to_owned())))
                    .collect()
            })
            .unwrap_or_default()
    });
    &ABSENT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_absent_globals_have_hints() {
        let absent = absent_globals();
        let names: Vec<&str> = absent.keys().map(String::as_str).collect();
        assert_eq!(names, ["Buffer", "fetch", "process", "require"]);
        assert!(absent["fetch"].contains("http action"));
    }

    #[test]
    fn fetch_is_declared_only_with_the_http_action() {
        let names = |http_action| -> Vec<&str> {
            files(http_action)
                .into_iter()
                .map(|(name, _)| name)
                .collect()
        };
        assert_eq!(
            names(false),
            [
                "tsconfig.json",
                "rewind.d.ts",
                "globals.d.ts",
                "old-names.d.ts"
            ]
        );
        assert_eq!(
            names(true),
            [
                "tsconfig.json",
                "rewind.d.ts",
                "globals.d.ts",
                "old-names.d.ts",
                "fetch.d.ts"
            ]
        );
        let config: Value = serde_json::from_str(TSCONFIG_JSON).unwrap();
        assert_eq!(config["include"], serde_json::json!(["*.ts"]));
        assert_eq!(config["compilerOptions"]["strict"], true);
    }
}
