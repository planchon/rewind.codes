//! Checking a code and compiling it: TypeScript → JavaScript with a source map (deno_ast,
//! that is swc).
//!
//! Everything that can be refused without running the code is refused here, so that a run
//! is never created for a code that cannot load (`archi/03-execution.md`, "Starting a
//! run"): syntax errors, dynamic `import()`, imports of modules other than `rewind`,
//! `rewind:actions` and `rewind:workflows`, imports of names those modules do not export, a
//! missing default export. The old names code already written uses are accepted without being
//! offered: the module `rewind:tools`, and `rewind`'s `ToolError` (`archi/08-decisions.md`, R1).

use std::collections::BTreeSet;

use deno_ast::swc::ast::{
    BindingIdent, CallExpr, Callee, ClassDecl, DefaultDecl, ExportSpecifier, Expr, FnDecl,
    ImportSpecifier, ModuleDecl, ModuleExportName, ModuleItem, Str, TsModuleRef,
};
use deno_ast::swc::common::Spanned;
use deno_ast::swc::ecma_visit::{Visit, VisitWith};
use deno_ast::{
    EmitOptions, MediaType, ModuleSpecifier, ParseParams, ParsedSource, ProgramRef,
    SourceMapOption, SourcePos, TranspileModuleOptions, TranspileOptions,
};
use rewind_core::{ActionSet, Code};
use rewind_engine::ports::{CompiledCode, InvalidCode};

use crate::epochs::Glue;
use crate::loader::OLD_ACTIONS_SPECIFIER;

/// Specifier of the model's code inside the isolate. Stack traces and callsites refer to it.
pub(crate) const CODE_SPECIFIER: &str = "file:///workflow.ts";

/// Checks a code against the modules it may import under `glue`, and compiles it.
pub(crate) fn compile(
    code: &Code,
    action_set: &ActionSet,
    glue: &Glue,
) -> Result<CompiledCode, InvalidCode> {
    let parsed = parse(code, CODE_SPECIFIER)?;
    Checker {
        parsed: &parsed,
        std: glue
            .std_exports
            .iter()
            .map(|name| (*name).to_owned())
            .collect(),
        std_aliases: glue.std_aliases,
        actions: (glue.action_exports)(action_set),
        workflows: (glue.workflow_exports)(action_set),
    }
    .check()?;
    let Transpiled {
        javascript,
        source_map,
    } = emit(parsed)?;
    Ok(CompiledCode {
        javascript,
        source_map,
    })
}

/// The names a code imports from `rewind:actions`, from its syntax tree; `None` when it may
/// reach every one: it imports or re-exports the whole module, or it does not parse. Type-only
/// imports are left out: they are erased before the code loads.
pub(crate) fn action_imports(code: &Code) -> Option<BTreeSet<String>> {
    let parsed = parse(code, CODE_SPECIFIER).ok()?;
    let ProgramRef::Module(module) = parsed.program_ref() else {
        return Some(BTreeSet::new());
    };
    let is_actions = |src: &Str| {
        matches!(
            src.value.to_atom_lossy().as_str(),
            "rewind:actions" | OLD_ACTIONS_SPECIFIER
        )
    };
    let mut names = BTreeSet::new();
    for item in &module.body {
        let ModuleItem::ModuleDecl(decl) = item else {
            continue;
        };
        match decl {
            ModuleDecl::Import(import) if !import.type_only && is_actions(&import.src) => {
                for specifier in &import.specifiers {
                    match specifier {
                        ImportSpecifier::Named(named) if !named.is_type_only => {
                            names.insert(match &named.imported {
                                Some(imported) => export_name(imported),
                                None => named.local.sym.to_string(),
                            });
                        }
                        ImportSpecifier::Named(_) => {}
                        ImportSpecifier::Namespace(_) | ImportSpecifier::Default(_) => {
                            return None;
                        }
                    }
                }
            }
            ModuleDecl::ExportAll(export) if !export.type_only && is_actions(&export.src) => {
                return None;
            }
            ModuleDecl::ExportNamed(export)
                if !export.type_only && export.src.as_deref().is_some_and(is_actions) =>
            {
                for specifier in &export.specifiers {
                    match specifier {
                        ExportSpecifier::Named(named) if !named.is_type_only => {
                            names.insert(export_name(&named.orig));
                        }
                        ExportSpecifier::Named(_) => {}
                        ExportSpecifier::Namespace(_) | ExportSpecifier::Default(_) => {
                            return None;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Some(names)
}

/// TypeScript compiled to JavaScript, with its source map.
pub(crate) struct Transpiled {
    pub(crate) javascript: String,
    pub(crate) source_map: String,
}

/// Compiles a module without the checks of a code: a module of handlers.
pub(crate) fn transpile(source: &Code, specifier: &str) -> Result<Transpiled, InvalidCode> {
    emit(parse(source, specifier)?)
}

fn parse(source: &Code, specifier: &str) -> Result<ParsedSource, InvalidCode> {
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

fn emit(parsed: ParsedSource) -> Result<Transpiled, InvalidCode> {
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

fn invalid(message: String) -> InvalidCode {
    InvalidCode { message }
}

/// The module rules, checked on the syntax tree. Type-only imports and exports are skipped:
/// they are erased before the code loads.
struct Checker<'a> {
    parsed: &'a ParsedSource,
    /// What `rewind` exports.
    std: BTreeSet<String>,
    /// The old names `rewind` still exports, never listed (R1).
    std_aliases: &'static [&'static str],
    /// What `rewind:actions` exports for the run's action set.
    actions: BTreeSet<String>,
    /// What `rewind:workflows` exports for the run's action set.
    workflows: BTreeSet<String>,
}

impl Checker<'_> {
    fn check(&self) -> Result<(), InvalidCode> {
        let mut dynamic = DynamicImports::default();
        self.parsed.program().visit_with(&mut dynamic);
        if let Some(position) = dynamic.first {
            return Err(invalid(format!(
                "dynamic import() is not available in Rewind workflows ({}): import from \"rewind\", \
                 \"rewind:actions\" and \"rewind:workflows\" at the top of the code",
                self.at(position)
            )));
        }

        let mut requires = Requires::default();
        self.parsed.program().visit_with(&mut requires);
        if let Some(position) = requires.first.filter(|_| !requires.bound) {
            return Err(invalid(format!(
                "require() is not available in Rewind workflows ({}): use `import` from \"rewind\", \
                 \"rewind:actions\" and \"rewind:workflows\"",
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
                                     `import {{ sleep }} from \"rewind\"`, or the whole module with \
                                     `import * as actions from \"rewind:actions\"`",
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
                            "require() is not available in Rewind workflows ({}): use `import` \
                             from \"rewind\", \"rewind:actions\" and \"rewind:workflows\"",
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
                "the code has no default export: it must `export default` its main function, \
                 as in `export default async function main(input) { … }`"
                    .into(),
            ))
        }
    }

    /// The names a module the code imports exports, or why it cannot be imported.
    fn module(&self, src: &Str) -> Result<&BTreeSet<String>, InvalidCode> {
        match src.value.to_atom_lossy().as_str() {
            "rewind" => Ok(&self.std),
            "rewind:actions" | OLD_ACTIONS_SPECIFIER => Ok(&self.actions),
            "rewind:workflows" => Ok(&self.workflows),
            other => Err(invalid(format!(
                "cannot import {other:?} ({}): workflows can only import \"rewind\", \"rewind:actions\" \
                 and \"rewind:workflows\"",
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
    ) -> Result<(), InvalidCode> {
        if exports.contains(name)
            || (std::ptr::eq(exports, &self.std) && self.std_aliases.contains(&name))
        {
            return Ok(());
        }
        let available = if exports.is_empty() {
            "it exports nothing for this run's action set".to_owned()
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

    fn not_a_function(&self, position: deno_ast::swc::common::BytePos) -> InvalidCode {
        invalid(format!(
            "the code must `export default` its main function ({})",
            self.at(SourcePos::unsafely_from_byte_pos(position))
        ))
    }

    fn at(&self, position: SourcePos) -> String {
        let at = self
            .parsed
            .text_info_lazy()
            .line_and_column_display(position);
        format!("workflow.ts:{}:{}", at.line_number, at.column_number)
    }
}

fn export_name(name: &ModuleExportName) -> String {
    name.atom().to_string()
}

fn quoted(src: &Str) -> String {
    format!("{:?}", src.value.to_atom_lossy().as_str())
}

/// Whether an expression may evaluate to a function. Only what is certainly not one is
/// refused; the rest is checked when the code loads.
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

/// Finds the first call of `require(…)`, the CommonJS loader the sandbox does not have
/// (`archi/03-execution.md`, "Starting a run"), unless the code binds the name `require`
/// itself, as a function or a variable it may call.
#[derive(Default)]
struct Requires {
    first: Option<SourcePos>,
    bound: bool,
}

impl Visit for Requires {
    fn visit_call_expr(&mut self, call: &CallExpr) {
        if self.first.is_none()
            && let Callee::Expr(callee) = &call.callee
            && let Expr::Ident(ident) = &**callee
            && &*ident.sym == "require"
        {
            self.first = Some(SourcePos::unsafely_from_byte_pos(call.span().lo));
        }
        call.visit_children_with(self);
    }

    fn visit_binding_ident(&mut self, ident: &BindingIdent) {
        if &*ident.id.sym == "require" {
            self.bound = true;
        }
    }

    fn visit_fn_decl(&mut self, decl: &FnDecl) {
        if &*decl.ident.sym == "require" {
            self.bound = true;
        }
        decl.visit_children_with(self);
    }

    fn visit_class_decl(&mut self, decl: &ClassDecl) {
        if &*decl.ident.sym == "require" {
            self.bound = true;
        }
        decl.visit_children_with(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::epochs::EPOCHS;

    fn check(source: &str) -> Result<CompiledCode, InvalidCode> {
        let action_set =
            ActionSet::from_harness_actions(["github.list_issues", "ask_user"]).unwrap();
        let glue = EPOCHS.last().expect("an epoch");
        compile(&Code::new(source), &action_set, glue)
    }

    fn refused(source: &str) -> String {
        check(source).expect_err(source).message
    }

    #[test]
    fn tells_what_a_script_imports_from_rewind_actions() {
        let imports = |source: &str| action_imports(&Code::new(source));
        let names = |names: &[&str]| Some(names.iter().map(|name| (*name).to_owned()).collect());
        assert_eq!(
            imports(
                "import { sleep } from \"rewind\";\nimport { github, ask_user as ask, type Issue } \
                 from \"rewind:actions\";\nimport type { Linear } from \"rewind:actions\";\nexport \
                 default async () => [sleep, github, ask];"
            ),
            names(&["ask_user", "github"]),
            "the named imports, by the name the module exports, and no type"
        );
        assert_eq!(
            imports("export default async () => 1;"),
            names(&[]),
            "nothing imported"
        );
        assert_eq!(
            imports("export { zendesk } from \"rewind:actions\";\nexport default async () => 1;"),
            names(&["zendesk"]),
            "a re-export reaches what it names"
        );
        for whole in [
            "import * as actions from \"rewind:actions\";\nexport default async () => actions;",
            "export * from \"rewind:actions\";\nexport default async () => 1;",
            "export * as actions from \"rewind:actions\";\nexport default async () => 1;",
        ] {
            assert_eq!(imports(whole), None, "{whole}");
        }
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
            "import { sleep, signal } from \"rewind\";\nimport { github, ask_user } from \"rewind:actions\";\nexport default async () => [sleep, signal, github, ask_user];",
            "import * as actions from \"rewind:actions\";\nexport default async () => actions;",
            "import { sleep as wait } from \"rewind\";\nexport default async () => wait;",
            "import type { Nothing } from \"./types\";\nimport { type Other } from \"rewind\";\nexport default async function main(input: Nothing): Promise<Other> { return input; }",
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
        assert!(message.contains(" at workflow.ts:1:"), "{message}");
    }

    #[test]
    fn refuses_dynamic_imports() {
        assert_eq!(
            refused(
                "export default async function main() {\n  return await import(\"rewind\");\n}"
            ),
            "dynamic import() is not available in Rewind workflows (workflow.ts:2:16): import from \
             \"rewind\", \"rewind:actions\" and \"rewind:workflows\" at the top of the code"
        );
    }

    #[test]
    fn refuses_other_modules() {
        assert_eq!(
            refused("import fs from \"node:fs\";\nexport default async () => fs;"),
            "cannot import \"node:fs\" (workflow.ts:1:16): workflows can only import \"rewind\", \
             \"rewind:actions\" and \"rewind:workflows\""
        );
        assert!(
            refused("import hooks from \"rewind:internal\";\nexport default async () => hooks;")
                .starts_with("cannot import \"rewind:internal\"")
        );
        assert!(
            refused("export * from \"./other\";\nexport default async () => 1;")
                .starts_with("cannot import \"./other\"")
        );
        assert!(
            refused("import fs = require(\"fs\");\nexport default async () => fs;")
                .starts_with("require() is not available")
        );
        assert_eq!(
            refused("export default async function main() {\n  return require(\"fs\");\n}"),
            "require() is not available in Rewind workflows (workflow.ts:2:10): use `import` from \
             \"rewind\", \"rewind:actions\" and \"rewind:workflows\""
        );
    }

    #[test]
    fn accepts_a_require_the_script_defines() {
        check(
            "function require(name: string) { return name; }\n\
             export default async function main() { return require(\"fs\"); }",
        )
        .unwrap();
        check(
            "export default async function main() {\n  const require = (n: string) => n.length;\n  \
             return require(\"fs\");\n}",
        )
        .unwrap();
    }

    #[test]
    fn refuses_names_the_modules_do_not_export() {
        assert_eq!(
            refused("import { fetch } from \"rewind\";\nexport default async () => fetch;"),
            "\"rewind\" has no export named \"fetch\" (workflow.ts:1:10); it exports ActionError, \
             CancelledError, WorkflowError, run, signal, sleep, webhook"
        );
        assert_eq!(
            refused("import { jira } from \"rewind:actions\";\nexport default async () => jira;"),
            "\"rewind:actions\" has no export named \"jira\" (workflow.ts:1:10); it exports ask_user, \
             github"
        );
        assert!(
            refused(
                "import { list_issues } from \"rewind:actions\";\nexport default async () => 1;"
            )
            .contains("no export named \"list_issues\"")
        );
        assert!(
            refused("import rewind from \"rewind\";\nexport default async () => rewind;")
                .starts_with("\"rewind\" has no default export (workflow.ts:1:8)")
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
                refused(source).starts_with("the code has no default export"),
                "{source}"
            );
        }
        for source in [
            "export default 42;",
            "export default { main() {} };",
            "export default class Main {}",
        ] {
            assert!(
                refused(source).starts_with("the code must `export default` its main function"),
                "{source}"
            );
        }
    }
}
