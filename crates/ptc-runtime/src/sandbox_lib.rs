//! The sandbox lib: what the type-checker declares to scripts, the contract between this
//! runtime and `ptc-check` (`archi/04-tools.md`, "Type-checking"). **What type-checks is what
//! exists**: the files declare exactly the globals the isolate has and the exports of the `ptc`
//! module, for the latest epoch, which every new run starts under.
//!
//! The files live in `crates/ptc-runtime/lib/`. `ptc-check` cannot depend on this crate (two
//! adapters), so it embeds the same files by path, with `include_str!`; this module embeds them
//! here, and reads the list of declared names for the runtime's own tests.
//!
//! | File | What it is |
//! | --- | --- |
//! | `ptc.d.ts` | The `ptc` module |
//! | `globals.d.ts` | The sandbox's globals beyond ECMAScript |
//! | `fetch.d.ts` | `fetch`, only for a toolset that enables the `http` tool |
//! | `tsconfig.json` | tsgo's options: strict, ES2025 and the ESNext pieces V8 ships (`Temporal`, explicit resource management…), without shared memory, weak references and decorators, no DOM |
//! | `globals.json` | The declared global names, the `ptc` exports, and a hint for each absent global models reach for |
//!
//! The checker writes the script as `script.ts` next to these files and the toolset's
//! `ptc-tools.d.ts`; `tsconfig.json` includes every `*.ts` of its directory.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use serde_json::Value;

/// The `ptc` module.
pub const PTC_D_TS: &str = include_str!("../lib/ptc.d.ts");
/// The sandbox's globals beyond ECMAScript.
pub const GLOBALS_D_TS: &str = include_str!("../lib/globals.d.ts");
/// `fetch`: only for a toolset that enables the `http` tool.
pub const FETCH_D_TS: &str = include_str!("../lib/fetch.d.ts");
/// tsgo's configuration.
pub const TSCONFIG_JSON: &str = include_str!("../lib/tsconfig.json");
/// The declared names, machine-readable.
pub const GLOBALS_JSON: &str = include_str!("../lib/globals.json");

/// The files the checker writes next to `script.ts` and `ptc-tools.d.ts`, by file name.
pub fn files(http_tool: bool) -> Vec<(&'static str, &'static str)> {
    let mut files = vec![
        ("tsconfig.json", TSCONFIG_JSON),
        ("ptc.d.ts", PTC_D_TS),
        ("globals.d.ts", GLOBALS_D_TS),
    ];
    if http_tool {
        files.push(("fetch.d.ts", FETCH_D_TS));
    }
    files
}

/// The names `globals.json` declares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeclaredGlobals {
    /// The ECMAScript built-ins the lib declares (ES2025 and the ESNext pieces V8 ships, without
    /// `SharedArrayBuffer`, `Atomics`, `WeakRef`, `FinalizationRegistry` and `WebAssembly`).
    pub ecmascript: BTreeSet<String>,
    /// The sandbox's own globals (`archi/01-product.md`, "What exists in the sandbox").
    pub sandbox: BTreeSet<String>,
    /// Globals that exist only when the toolset enables the `http` tool.
    pub http_tool: BTreeSet<String>,
    /// The names the `ptc` module exports (values, not types).
    pub ptc: BTreeSet<String>,
    /// Globals models reach for by habit but that are absent, with what to use instead.
    pub absent: Vec<AbsentGlobal>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AbsentGlobal {
    pub name: String,
    /// Written for the model, shown with the checker's diagnostic.
    pub hint: String,
}

impl DeclaredGlobals {
    /// Every global name of a script's `globalThis`: what the isolate must have, exactly.
    pub fn names(&self, http_tool: bool) -> BTreeSet<&str> {
        let mut names: BTreeSet<&str> = self
            .ecmascript
            .iter()
            .chain(&self.sandbox)
            .map(String::as_str)
            .collect();
        if http_tool {
            names.extend(self.http_tool.iter().map(String::as_str));
        }
        names
    }

    /// The hint for an absent global, if models are known to reach for it.
    pub fn hint(&self, name: &str) -> Option<&str> {
        self.absent
            .iter()
            .find(|absent| absent.name == name)
            .map(|absent| absent.hint.as_str())
    }
}

/// The names `globals.json` declares.
pub fn declared_globals() -> &'static DeclaredGlobals {
    static DECLARED: LazyLock<DeclaredGlobals> = LazyLock::new(|| {
        parse(GLOBALS_JSON).unwrap_or_else(|error| panic!("lib/globals.json: {error}"))
    });
    &DECLARED
}

fn parse(json: &str) -> Result<DeclaredGlobals, String> {
    let document: Value = serde_json::from_str(json).map_err(|error| error.to_string())?;
    let names = |key: &str| -> Result<BTreeSet<String>, String> {
        let list = document[key]
            .as_array()
            .ok_or_else(|| format!("{key:?} is not a list"))?;
        let names: Vec<String> = list
            .iter()
            .map(|name| {
                name.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| format!("{key:?} holds a value that is not a name"))
            })
            .collect::<Result<_, _>>()?;
        let set: BTreeSet<String> = names.iter().cloned().collect();
        if set.len() != names.len() {
            return Err(format!("{key:?} lists a name twice"));
        }
        Ok(set)
    };
    let absent = document["absent"]
        .as_object()
        .ok_or("\"absent\" is not an object")?
        .iter()
        .map(|(name, hint)| {
            Ok(AbsentGlobal {
                name: name.clone(),
                hint: hint
                    .as_str()
                    .ok_or_else(|| format!("the hint of {name} is not a string"))?
                    .to_owned(),
            })
        })
        .collect::<Result<_, String>>()?;
    Ok(DeclaredGlobals {
        ecmascript: names("ecmascript")?,
        sandbox: names("sandbox")?,
        http_tool: names("httpTool")?,
        ptc: names("ptc")?,
        absent,
    })
}

#[cfg(test)]
mod tests {
    use deno_ast::swc::ast::{Decl, ModuleDecl, ModuleItem, Stmt, TsModuleName, TsNamespaceBody};
    use deno_ast::{MediaType, ModuleSpecifier, ParseParams, ProgramRef};

    use super::*;
    use crate::epochs::EPOCHS;

    /// Top-level statements of a declaration file.
    fn statements(name: &str, text: &str) -> Vec<Stmt> {
        let parsed = deno_ast::parse_program(ParseParams {
            specifier: ModuleSpecifier::parse(&format!("file:///{name}")).unwrap(),
            text: text.into(),
            media_type: MediaType::Dts,
            capture_tokens: false,
            scope_analysis: false,
            maybe_syntax: None,
        })
        .unwrap_or_else(|error| panic!("{name}: {error}"));
        match parsed.program_ref() {
            ProgramRef::Script(script) => script.body.clone(),
            ProgramRef::Module(_) => panic!("{name} must be a script: its declarations are global"),
        }
    }

    /// The values a declaration introduces (not the types).
    fn value_names(decl: &Decl) -> Vec<String> {
        match decl {
            Decl::Var(var) => var
                .decls
                .iter()
                .filter_map(|declarator| declarator.name.as_ident().map(|id| id.sym.to_string()))
                .collect(),
            Decl::Fn(function) => vec![function.ident.sym.to_string()],
            Decl::Class(class) => vec![class.ident.sym.to_string()],
            Decl::TsModule(module) => match &module.id {
                TsModuleName::Ident(ident) => vec![ident.sym.to_string()],
                TsModuleName::Str(_) => Vec::new(),
            },
            Decl::TsInterface(_) | Decl::TsTypeAlias(_) | Decl::TsEnum(_) | Decl::Using(_) => {
                Vec::new()
            }
        }
    }

    fn global_values(name: &str, text: &str) -> BTreeSet<String> {
        statements(name, text)
            .iter()
            .filter_map(|statement| match statement {
                Stmt::Decl(decl) => Some(value_names(decl)),
                _ => None,
            })
            .flatten()
            .collect()
    }

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn globals_json_is_well_formed() {
        let declared = declared_globals();
        assert!(declared.ecmascript.len() > 50);
        assert!(declared.ecmascript.is_disjoint(&declared.sandbox));
        assert!(declared.ecmascript.is_disjoint(&declared.http_tool));
        assert!(declared.sandbox.is_disjoint(&declared.http_tool));
        for removed in [
            "SharedArrayBuffer",
            "Atomics",
            "WeakRef",
            "FinalizationRegistry",
            "WebAssembly",
        ] {
            assert!(!declared.names(true).contains(removed), "{removed}");
        }
        // The four names of the checker's rule "Absent global" (archi/04-tools.md).
        let absent: BTreeSet<String> = declared.absent.iter().map(|a| a.name.clone()).collect();
        assert_eq!(absent, set(&["Buffer", "fetch", "process", "require"]));
        assert!(declared.hint("fetch").unwrap().contains("http tool"));
        for name in &absent {
            assert!(!declared.names(false).contains(name.as_str()), "{name}");
        }
        assert!(declared.names(true).contains("fetch"));
        assert!(!declared.names(false).contains("fetch"));
    }

    /// Every sandbox global of `globals.json` is declared by `globals.d.ts`, except `Temporal`,
    /// which comes from tsgo's own lib; and nothing else is.
    #[test]
    fn the_declaration_files_declare_exactly_the_listed_globals() {
        let declared = declared_globals();
        let mut sandbox = declared.sandbox.clone();
        assert!(sandbox.remove("Temporal"));
        assert_eq!(global_values("globals.d.ts", GLOBALS_D_TS), sandbox);
        assert_eq!(global_values("fetch.d.ts", FETCH_D_TS), declared.http_tool);
        assert!(global_values("ptc.d.ts", PTC_D_TS).is_empty());
    }

    /// `ptc.d.ts` declares the module `ptc`, exporting exactly the listed values.
    #[test]
    fn ptc_d_ts_declares_the_ptc_module() {
        let modules: Vec<_> = statements("ptc.d.ts", PTC_D_TS)
            .into_iter()
            .filter_map(|statement| match statement {
                Stmt::Decl(Decl::TsModule(module)) => Some(module),
                _ => None,
            })
            .collect();
        assert_eq!(modules.len(), 1);
        let module = &modules[0];
        assert!(matches!(&module.id, TsModuleName::Str(name) if name.value == *"ptc"));
        let Some(TsNamespaceBody::TsModuleBlock(block)) = &module.body else {
            panic!("module ptc has no body");
        };
        let mut values = BTreeSet::new();
        let mut types = BTreeSet::new();
        for item in &block.body {
            let ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(export)) = item else {
                panic!("module ptc holds a declaration that is not exported");
            };
            match &export.decl {
                Decl::TsTypeAlias(alias) => {
                    types.insert(alias.id.sym.to_string());
                }
                decl => values.extend(value_names(decl)),
            }
        }
        assert_eq!(values, declared_globals().ptc);
        assert_eq!(types, set(&["Duration"]));
    }

    /// Everything the runtime's `ptc` exports is declared.
    #[test]
    fn every_export_of_the_runtime_is_declared() {
        let latest = EPOCHS.last().unwrap();
        for name in latest.std_exports {
            assert!(declared_globals().ptc.contains(*name), "{name}");
        }
    }

    /// The ESNext pieces of TypeScript's lib whose every declaration V8 ships (a test of the
    /// sandbox checks each one exists). Not `esnext.decorators` (no `Symbol.metadata`), nor
    /// `esnext.sharedmemory` (removed).
    const ESNEXT: [&str; 8] = [
        "esnext.array",
        "esnext.collection",
        "esnext.date",
        "esnext.disposable",
        "esnext.error",
        "esnext.intl",
        "esnext.temporal",
        "esnext.typedarrays",
    ];

    #[test]
    fn tsconfig_is_strict_es2025_without_dom() {
        let config: Value = serde_json::from_str(TSCONFIG_JSON).unwrap();
        let options = &config["compilerOptions"];
        assert_eq!(options["strict"], true);
        assert_eq!(options["noEmit"], true);
        assert_eq!(options["target"], "es2025");
        assert_eq!(options["types"], serde_json::json!([]));
        let libs: Vec<&str> = options["lib"]
            .as_array()
            .unwrap()
            .iter()
            .map(|lib| lib.as_str().unwrap())
            .collect();
        assert!(libs.contains(&"es2024.promise"));
        for lib in [
            "es2025.collection",
            "es2025.float16",
            "es2025.intl",
            "es2025.iterator",
            "es2025.promise",
            "es2025.regexp",
        ] {
            assert!(libs.contains(&lib), "{lib}");
        }
        for lib in ESNEXT {
            assert!(libs.contains(&lib), "{lib}");
        }
        for lib in &libs {
            assert!(
                lib.starts_with("es") && !lib.contains("dom") && !lib.contains("webworker"),
                "{lib}"
            );
            assert!(
                !lib.contains("sharedmemory") && !lib.contains("weakref"),
                "{lib} declares globals the sandbox removes"
            );
            assert!(
                !lib.starts_with("esnext") || ESNEXT.contains(lib),
                "{lib} declares what V8 may not ship"
            );
        }
        assert_eq!(config["include"], serde_json::json!(["*.ts"]));
    }

    #[test]
    fn the_checker_gets_fetch_only_with_the_http_tool() {
        let names = |http_tool| -> Vec<&str> {
            files(http_tool).into_iter().map(|(name, _)| name).collect()
        };
        assert_eq!(names(false), ["tsconfig.json", "ptc.d.ts", "globals.d.ts"]);
        assert_eq!(
            names(true),
            ["tsconfig.json", "ptc.d.ts", "globals.d.ts", "fetch.d.ts"]
        );
    }
}
