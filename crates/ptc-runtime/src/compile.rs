//! Checking a script and compiling it: TypeScript → JavaScript with a source map (deno_ast,
//! that is swc).
//!
//! Everything that can be refused without running the script is refused here, so that a run
//! is never created for a script that cannot load (`archi/03-execution.md`, "Starting a
//! run"): syntax errors, dynamic `import()`, imports of modules other than `ptc` and
//! `ptc:tools`, imports of names those modules do not export, a missing default export.

use std::collections::BTreeSet;

use deno_ast::swc::ast::{
    CallExpr, Callee, DefaultDecl, ExportSpecifier, Expr, ImportSpecifier, ModuleDecl,
    ModuleExportName, ModuleItem, Str, TsModuleRef,
};
use deno_ast::swc::common::Spanned;
use deno_ast::swc::ecma_visit::{Visit, VisitWith};
use deno_ast::{
    EmitOptions, MediaType, ModuleSpecifier, ParseParams, ParsedSource, ProgramRef,
    SourceMapOption, SourcePos, TranspileModuleOptions, TranspileOptions,
};
use ptc_core::{Script, Toolset};
use ptc_engine::ports::{CompiledScript, InvalidScript};

use crate::epochs::Glue;

/// Specifier of the model's script inside the isolate. Stack traces and callsites refer to it.
pub(crate) const SCRIPT_SPECIFIER: &str = "file:///script.ts";

/// Checks a script against the modules it may import under `glue`, and compiles it.
pub(crate) fn compile(
    script: &Script,
    toolset: &Toolset,
    glue: &Glue,
) -> Result<CompiledScript, InvalidScript> {
    let parsed = parse(script, SCRIPT_SPECIFIER)?;
    Checker {
        parsed: &parsed,
        std: glue
            .std_exports
            .iter()
            .map(|name| (*name).to_owned())
            .collect(),
        tools: (glue.tool_exports)(toolset),
    }
    .check()?;
    let Transpiled {
        javascript,
        source_map,
    } = emit(parsed)?;
    Ok(CompiledScript {
        javascript,
        source_map,
    })
}

/// TypeScript compiled to JavaScript, with its source map.
pub(crate) struct Transpiled {
    pub(crate) javascript: String,
    pub(crate) source_map: String,
}

/// Compiles a module without the checks of a script: a module of mock handlers.
pub(crate) fn transpile(source: &Script, specifier: &str) -> Result<Transpiled, InvalidScript> {
    emit(parse(source, specifier)?)
}

fn parse(source: &Script, specifier: &str) -> Result<ParsedSource, InvalidScript> {
    let file = specifier.trim_start_matches("file:///");
    deno_ast::parse_module(ParseParams {
        specifier: ModuleSpecifier::parse(specifier)
            .map_err(|err| invalid(format!("internal: {err}")))?,
        text: source.source().into(),
        media_type: MediaType::TypeScript,
        capture_tokens: false,
        scope_analysis: false,
        maybe_syntax: None,
    })
    // Diagnostics name the file as stack traces and callsites do.
    .map_err(|err| invalid(err.to_string().replace(specifier, file)))
}

fn emit(parsed: ParsedSource) -> Result<Transpiled, InvalidScript> {
    let emitted = parsed
        .transpile(
            &TranspileOptions::default(),
            &TranspileModuleOptions::default(),
            &EmitOptions {
                source_map: SourceMapOption::Separate,
                inline_sources: true,
                ..Default::default()
            },
        )
        .map_err(|err| invalid(err.to_string()))?
        .into_source();
    Ok(Transpiled {
        javascript: emitted.text,
        source_map: emitted.source_map.unwrap_or_default(),
    })
}

fn invalid(message: String) -> InvalidScript {
    InvalidScript { message }
}

/// The module rules, checked on the syntax tree. Type-only imports and exports are skipped:
/// they are erased before the script loads.
struct Checker<'a> {
    parsed: &'a ParsedSource,
    /// What `ptc` exports.
    std: BTreeSet<String>,
    /// What `ptc:tools` exports for the run's toolset.
    tools: BTreeSet<String>,
}

impl Checker<'_> {
    fn check(&self) -> Result<(), InvalidScript> {
        let mut dynamic = DynamicImports::default();
        self.parsed.program().visit_with(&mut dynamic);
        if let Some(position) = dynamic.first {
            return Err(invalid(format!(
                "dynamic import() is not available in ai-ptc scripts ({}): import from \"ptc\" \
                 and \"ptc:tools\" at the top of the script",
                self.at(position)
            )));
        }

        let body = match self.parsed.program_ref() {
            ProgramRef::Module(module) => module.body.as_slice(),
            ProgramRef::Script(_) => &[],
        };
        let mut has_default = false;
        for item in body {
            let ModuleItem::ModuleDecl(decl) = item else {
                continue;
            };
            match decl {
                ModuleDecl::Import(import) if !import.type_only => {
                    let exports = self.module(&import.src)?;
                    for specifier in &import.specifiers {
                        match specifier {
                            ImportSpecifier::Named(named) if !named.is_type_only => {
                                let name = match &named.imported {
                                    Some(imported) => export_name(imported),
                                    None => named.local.sym.to_string(),
                                };
                                self.exported(&import.src, exports, &name, named.span.lo)?;
                            }
                            ImportSpecifier::Default(default) => {
                                return Err(invalid(format!(
                                    "{} has no default export ({}): import names from it, as in \
                                     `import {{ sleep }} from \"ptc\"`, or the whole module with \
                                     `import * as tools from \"ptc:tools\"`",
                                    quoted(&import.src),
                                    self.at(SourcePos::unsafely_from_byte_pos(default.span.lo))
                                )));
                            }
                            _ => {}
                        }
                    }
                }
                ModuleDecl::ExportAll(export) if !export.type_only => {
                    self.module(&export.src)?;
                }
                ModuleDecl::ExportNamed(export) if !export.type_only => {
                    let exports = match &export.src {
                        Some(src) => Some(self.module(src)?),
                        None => None,
                    };
                    for specifier in &export.specifiers {
                        let ExportSpecifier::Named(named) = specifier else {
                            continue;
                        };
                        if named.is_type_only {
                            continue;
                        }
                        if let (Some(src), Some(exports)) = (&export.src, exports) {
                            let name = export_name(&named.orig);
                            self.exported(src, exports, &name, named.span.lo)?;
                        }
                        let exported = named.exported.as_ref().unwrap_or(&named.orig);
                        has_default |= export_name(exported) == "default";
                    }
                }
                ModuleDecl::ExportDefaultDecl(export) => match &export.decl {
                    DefaultDecl::Fn(_) => has_default = true,
                    DefaultDecl::Class(_) => return Err(self.not_a_function(export.span.lo)),
                    DefaultDecl::TsInterfaceDecl(_) => {}
                },
                ModuleDecl::ExportDefaultExpr(export) => {
                    if !may_be_a_function(&export.expr) {
                        return Err(self.not_a_function(export.span.lo));
                    }
                    has_default = true;
                }
                ModuleDecl::TsImportEquals(import) => {
                    if let TsModuleRef::TsExternalModuleRef(require) = &import.module_ref {
                        return Err(invalid(format!(
                            "require() is not available in ai-ptc scripts ({}): use `import` \
                             from \"ptc\" and \"ptc:tools\"",
                            self.at(SourcePos::unsafely_from_byte_pos(require.span.lo))
                        )));
                    }
                }
                ModuleDecl::TsExportAssignment(export) => {
                    return Err(invalid(format!(
                        "`export =` is not supported ({}): use `export default` for the main \
                         function",
                        self.at(SourcePos::unsafely_from_byte_pos(export.span.lo))
                    )));
                }
                _ => {}
            }
        }
        if has_default {
            Ok(())
        } else {
            Err(invalid(
                "the script has no default export: it must `export default` its main function, \
                 as in `export default async function main(input) { … }`"
                    .into(),
            ))
        }
    }

    /// The names a module the script imports exports, or why it cannot be imported.
    fn module(&self, src: &Str) -> Result<&BTreeSet<String>, InvalidScript> {
        match src.value.to_atom_lossy().as_str() {
            "ptc" => Ok(&self.std),
            "ptc:tools" => Ok(&self.tools),
            other => Err(invalid(format!(
                "cannot import {other:?} ({}): scripts can only import \"ptc\" and \"ptc:tools\"",
                self.at(SourcePos::unsafely_from_byte_pos(src.span.lo))
            ))),
        }
    }

    fn exported(
        &self,
        src: &Str,
        exports: &BTreeSet<String>,
        name: &str,
        position: deno_ast::swc::common::BytePos,
    ) -> Result<(), InvalidScript> {
        if exports.contains(name) {
            return Ok(());
        }
        let available = if exports.is_empty() {
            "it exports nothing for this run's toolset".to_owned()
        } else {
            let names: Vec<&str> = exports.iter().map(String::as_str).collect();
            format!("it exports {}", names.join(", "))
        };
        Err(invalid(format!(
            "{} has no export named {name:?} ({}); {available}",
            quoted(src),
            self.at(SourcePos::unsafely_from_byte_pos(position))
        )))
    }

    fn not_a_function(&self, position: deno_ast::swc::common::BytePos) -> InvalidScript {
        invalid(format!(
            "the script must `export default` its main function ({})",
            self.at(SourcePos::unsafely_from_byte_pos(position))
        ))
    }

    fn at(&self, position: SourcePos) -> String {
        let at = self
            .parsed
            .text_info_lazy()
            .line_and_column_display(position);
        format!("script.ts:{}:{}", at.line_number, at.column_number)
    }
}

fn export_name(name: &ModuleExportName) -> String {
    name.atom().to_string()
}

fn quoted(src: &Str) -> String {
    format!("{:?}", src.value.to_atom_lossy().as_str())
}

/// Whether an expression may evaluate to a function. Only what is certainly not one is
/// refused; the rest is checked when the script loads.
fn may_be_a_function(expr: &Expr) -> bool {
    match expr {
        Expr::Paren(inner) => may_be_a_function(&inner.expr),
        Expr::TsAs(inner) => may_be_a_function(&inner.expr),
        Expr::TsSatisfies(inner) => may_be_a_function(&inner.expr),
        Expr::TsNonNull(inner) => may_be_a_function(&inner.expr),
        Expr::TsTypeAssertion(inner) => may_be_a_function(&inner.expr),
        Expr::TsConstAssertion(inner) => may_be_a_function(&inner.expr),
        Expr::Lit(_) | Expr::Object(_) | Expr::Array(_) | Expr::Tpl(_) | Expr::Class(_) => false,
        _ => true,
    }
}

/// Finds the first `import(…)` call.
#[derive(Default)]
struct DynamicImports {
    first: Option<SourcePos>,
}

impl Visit for DynamicImports {
    fn visit_call_expr(&mut self, call: &CallExpr) {
        if matches!(call.callee, Callee::Import(_)) && self.first.is_none() {
            self.first = Some(SourcePos::unsafely_from_byte_pos(call.span().lo));
        }
        call.visit_children_with(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::epochs::EPOCHS;

    fn check(source: &str) -> Result<CompiledScript, InvalidScript> {
        let toolset = Toolset::from_host_tools(["github.list_issues", "ask_user"]).unwrap();
        let glue = EPOCHS.last().expect("an epoch");
        compile(&Script::new(source), &toolset, glue)
    }

    fn refused(source: &str) -> String {
        check(source).expect_err(source).message
    }

    #[test]
    fn strips_types_and_maps_sources() {
        let compiled = check(
            "export default async function main(input: { n: number }): Promise<number> { return input.n * 2; }",
        )
        .unwrap();
        assert!(
            compiled
                .javascript
                .contains("export default async function main(input)"),
            "{}",
            compiled.javascript
        );
        assert!(compiled.source_map.starts_with('{'));
    }

    #[test]
    fn accepts_what_the_modules_export() {
        for source in [
            "import { sleep, signal } from \"ptc\";\nimport { github, ask_user } from \"ptc:tools\";\nexport default async () => [sleep, signal, github, ask_user];",
            "import * as tools from \"ptc:tools\";\nexport default async () => tools;",
            "import { sleep as wait } from \"ptc\";\nexport default async () => wait;",
            "import type { Nothing } from \"./types\";\nimport { type Other } from \"ptc\";\nexport default async function main(input: Nothing): Promise<Other> { return input; }",
            "async function main() { return 1; }\nexport { main as default };",
            "const main = async () => 1;\nexport default main;",
            "export default async () => import.meta.url;",
        ] {
            assert!(check(source).is_ok(), "{source}: {:?}", check(source));
        }
    }

    #[test]
    fn reports_syntax_errors_with_their_location() {
        let message = refused("export default function (");
        assert!(message.starts_with("SyntaxError: "), "{message}");
        assert!(message.contains(" at script.ts:1:"), "{message}");
    }

    #[test]
    fn refuses_dynamic_imports() {
        assert_eq!(
            refused("export default async function main() {\n  return await import(\"ptc\");\n}"),
            "dynamic import() is not available in ai-ptc scripts (script.ts:2:16): import from \
             \"ptc\" and \"ptc:tools\" at the top of the script"
        );
    }

    #[test]
    fn refuses_other_modules() {
        assert_eq!(
            refused("import fs from \"node:fs\";\nexport default async () => fs;"),
            "cannot import \"node:fs\" (script.ts:1:16): scripts can only import \"ptc\" and \
             \"ptc:tools\""
        );
        assert!(
            refused("import hooks from \"ptc:internal\";\nexport default async () => hooks;")
                .starts_with("cannot import \"ptc:internal\"")
        );
        assert!(
            refused("export * from \"./other\";\nexport default async () => 1;")
                .starts_with("cannot import \"./other\"")
        );
        assert!(
            refused("import fs = require(\"fs\");\nexport default async () => fs;")
                .starts_with("require() is not available")
        );
    }

    #[test]
    fn refuses_names_the_modules_do_not_export() {
        assert_eq!(
            refused("import { fetch } from \"ptc\";\nexport default async () => fetch;"),
            "\"ptc\" has no export named \"fetch\" (script.ts:1:10); it exports CancelledError, \
             ToolError, run, signal, sleep, webhook"
        );
        assert_eq!(
            refused("import { jira } from \"ptc:tools\";\nexport default async () => jira;"),
            "\"ptc:tools\" has no export named \"jira\" (script.ts:1:10); it exports ask_user, \
             github"
        );
        assert!(
            refused("import { list_issues } from \"ptc:tools\";\nexport default async () => 1;")
                .contains("no export named \"list_issues\"")
        );
        assert!(
            refused("import ptc from \"ptc\";\nexport default async () => ptc;")
                .starts_with("\"ptc\" has no default export (script.ts:1:8)")
        );
    }

    #[test]
    fn refuses_a_script_without_a_main_function() {
        for source in [
            "export async function main() { return 1; }",
            "const x = 1;",
            "export type Default = number;",
        ] {
            assert!(
                refused(source).starts_with("the script has no default export"),
                "{source}"
            );
        }
        for source in [
            "export default 42;",
            "export default { main() {} };",
            "export default class Main {}",
        ] {
            assert!(
                refused(source).starts_with("the script must `export default` its main function"),
                "{source}"
            );
        }
    }
}
