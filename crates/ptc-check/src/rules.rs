//! The checker's own rules (`archi/04-tools.md`, "Type-checking"). They run on the swc syntax
//! tree of the script, with scope analysis and without types: syntactic, and slightly less
//! precise than rules based on types.
//!
//! | Rule | Flags |
//! | --- | --- |
//! | `floating-promise` | A promise used as a statement, neither awaited, returned, assigned nor `void`ed: a call to a tool (through an import of `ptc:tools`, or `fetch` with the `http` tool), to `sleep`, `signal` or `webhook`, to a function of the script declared `async` or returning `Promise<…>`, to `Promise.all` and the like, or an async function called at once; also through `.then`, `.catch`, `.finally`, `,`, `?:`, `&&`, `\|\|` and `??` |
//! | `ignored-async-callback` | An async callback given to `forEach`, which ignores what it returns, or to `filter`, `some`, `every`, `find`, `findIndex`, `findLast`, `findLastIndex`, which read a promise as `true` |
//! | `absent-global` | A reference to a global the sandbox does not have but models reach for (`fetch` without the `http` tool, `require`, `process`, `Buffer`), with what to use instead |

use std::collections::HashSet;

use deno_ast::swc::ast::{
    ArrowExpr, BinaryOp, Callee, Expr, ExprOrSpread, ExprStmt, FnDecl, Function, Id, Ident,
    ImportSpecifier, MemberExpr, MemberProp, ModuleDecl, ModuleExportName, ModuleItem,
    OptChainBase, Pat, TsEntityName, TsType, TsTypeAnn, TsTypeRef, UnaryOp, VarDeclarator,
};
use deno_ast::swc::common::{Span, Spanned, SyntaxContext};
use deno_ast::swc::ecma_visit::{Visit, VisitWith};
use deno_ast::{
    MediaType, ModuleSpecifier, ParseParams, ParsedSource, ProgramRef, SourcePos, StartSourcePos,
};

use crate::sandbox::absent_globals;

pub(crate) const FLOATING_PROMISE: &str = "floating-promise";
pub(crate) const IGNORED_ASYNC_CALLBACK: &str = "ignored-async-callback";
pub(crate) const ABSENT_GLOBAL: &str = "absent-global";

/// The functions of `ptc` that return a promise.
const PTC_ASYNC: [&str; 3] = ["sleep", "signal", "webhook"];
/// The functions of `Promise` that group promises into one.
const COMBINATORS: [&str; 4] = ["all", "allSettled", "any", "race"];
/// Array methods that read what their callback returns as a boolean.
const PREDICATES: [&str; 7] = [
    "filter",
    "some",
    "every",
    "find",
    "findIndex",
    "findLast",
    "findLastIndex",
];

/// A problem one of the rules found, as byte offsets in the script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Finding {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) rule: &'static str,
    pub(crate) message: String,
    pub(crate) hint: Option<String>,
    /// The absent global this finding is about: it replaces tsgo's diagnostic about the same
    /// name at the same place, whose advice is wrong in the sandbox (`npm i @types/node`).
    pub(crate) global: Option<String>,
}

/// Runs the rules on a script. A script that does not parse gets no finding: tsgo reports
/// its syntax errors.
pub(crate) fn check(source: &str, http_tool: bool) -> Vec<Finding> {
    let Some(parsed) = parse(source) else {
        return Vec::new();
    };
    let mut scope = Scope {
        unresolved: parsed.unresolved_context(),
        http_tool,
        tools: HashSet::new(),
        ptc: HashSet::new(),
        ptc_namespaces: HashSet::new(),
        async_functions: HashSet::new(),
    };
    if let ProgramRef::Module(module) = parsed.program_ref() {
        for item in &module.body {
            if let ModuleItem::ModuleDecl(ModuleDecl::Import(import)) = item
                && !import.type_only
            {
                scope.import(&import.src.value.to_atom_lossy(), &import.specifiers);
            }
        }
    }
    parsed
        .program_ref()
        .visit_with(&mut AsyncFunctions(&mut scope.async_functions));
    let mut rules = Rules {
        scope,
        source,
        start: parsed.range().start,
        findings: Vec::new(),
    };
    parsed.program_ref().visit_with(&mut rules);
    rules.findings
}

fn parse(source: &str) -> Option<ParsedSource> {
    deno_ast::parse_module(ParseParams {
        specifier: ModuleSpecifier::parse("file:///script.ts").ok()?,
        text: source.into(),
        media_type: MediaType::TypeScript,
        capture_tokens: false,
        scope_analysis: true,
        maybe_syntax: None,
    })
    .ok()
}

/// What the names of the script refer to.
struct Scope {
    /// The context of names that refer to no declaration of the script: globals.
    unresolved: SyntaxContext,
    http_tool: bool,
    /// Bindings imported from `ptc:tools`: every call through them is a tool call.
    tools: HashSet<Id>,
    /// Bindings of the functions of `ptc` that return a promise.
    ptc: HashSet<Id>,
    /// `import * as ptc from "ptc"`.
    ptc_namespaces: HashSet<Id>,
    /// Functions of the script that return a promise.
    async_functions: HashSet<Id>,
}

impl Scope {
    fn import(&mut self, module: &str, specifiers: &[ImportSpecifier]) {
        for specifier in specifiers {
            match (module, specifier) {
                ("ptc:tools", ImportSpecifier::Named(named)) if !named.is_type_only => {
                    self.tools.insert(named.local.to_id());
                }
                ("ptc:tools", ImportSpecifier::Namespace(namespace)) => {
                    self.tools.insert(namespace.local.to_id());
                }
                ("ptc", ImportSpecifier::Named(named)) if !named.is_type_only => {
                    let imported = match &named.imported {
                        Some(ModuleExportName::Ident(ident)) => ident.sym.to_string(),
                        Some(ModuleExportName::Str(name)) => name.value.to_atom_lossy().to_string(),
                        None => named.local.sym.to_string(),
                    };
                    if PTC_ASYNC.contains(&imported.as_str()) {
                        self.ptc.insert(named.local.to_id());
                    }
                }
                ("ptc", ImportSpecifier::Namespace(namespace)) => {
                    self.ptc_namespaces.insert(namespace.local.to_id());
                }
                _ => {}
            }
        }
    }

    fn is_global(&self, ident: &Ident, name: &str) -> bool {
        ident.ctxt == self.unresolved && &*ident.sym == name
    }

    /// The hint for a global the sandbox does not have.
    fn absent(&self, name: &str) -> Option<&'static String> {
        if name == "fetch" && self.http_tool {
            return None;
        }
        absent_globals().get(name)
    }

    /// What a call to `callee` is, when it certainly returns a promise.
    fn promise_callee(&self, callee: &Expr) -> Option<Origin> {
        let callee = unwrap(callee);
        match callee {
            Expr::Arrow(arrow) if is_async_arrow(arrow) => return Some(Origin::Immediate),
            Expr::Fn(function) if is_async(&function.function) => {
                return Some(Origin::Immediate);
            }
            Expr::Ident(ident) => {
                let id = ident.to_id();
                return if self.tools.contains(&id)
                    || self.http_tool && self.is_global(ident, "fetch")
                {
                    Some(Origin::Tool)
                } else if self.ptc.contains(&id) {
                    Some(Origin::Ptc)
                } else if self.async_functions.contains(&id) {
                    Some(Origin::Script)
                } else {
                    None
                };
            }
            _ => {}
        }
        let member = as_member(callee)?;
        if root(callee).is_some_and(|root| self.tools.contains(&root.to_id())) {
            return Some(Origin::Tool);
        }
        let property = property(member)?;
        let Expr::Ident(object) = unwrap(&member.obj) else {
            return None;
        };
        if self.ptc_namespaces.contains(&object.to_id()) && PTC_ASYNC.contains(&property) {
            Some(Origin::Ptc)
        } else if self.is_global(object, "Promise") && COMBINATORS.contains(&property) {
            Some(Origin::Combinator)
        } else if self.http_tool && self.is_global(object, "globalThis") && property == "fetch" {
            Some(Origin::Tool)
        } else {
            None
        }
    }

    /// When `expr` is a call that certainly returns a promise: its callee, and what it calls. A
    /// `.then`, `.catch` or `.finally` chained to such a call is one too, found at the call.
    fn promise_call<'e>(&self, expr: &'e Expr) -> Option<(&'e Expr, Origin)> {
        let call = as_call(unwrap(expr))?;
        if let Some(member) = as_member(unwrap(call.callee))
            && matches!(property(member), Some("then" | "catch" | "finally"))
            && let Some(found) = self.promise_call(&member.obj)
        {
            return Some(found);
        }
        Some((call.callee, self.promise_callee(call.callee)?))
    }

    /// Whether a callback returns a promise.
    fn is_async_callback(&self, expr: &Expr) -> bool {
        match unwrap(expr) {
            Expr::Ident(ident) => self.async_functions.contains(&ident.to_id()),
            expr => is_async_function(expr),
        }
    }
}

/// What a floating call calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    Tool,
    Ptc,
    Script,
    Combinator,
    /// An async function called where it is written: `(async () => { … })()`.
    Immediate,
}

/// Collects the functions of the script that return a promise: declared `async` (not async
/// generators), or annotated as returning `Promise<…>`.
struct AsyncFunctions<'a>(&'a mut HashSet<Id>);

impl Visit for AsyncFunctions<'_> {
    fn visit_fn_decl(&mut self, decl: &FnDecl) {
        if is_async(&decl.function) {
            self.0.insert(decl.ident.to_id());
        }
        decl.visit_children_with(self);
    }

    fn visit_var_declarator(&mut self, declarator: &VarDeclarator) {
        if let (Pat::Ident(binding), Some(init)) = (&declarator.name, &declarator.init)
            && is_async_function(init)
        {
            self.0.insert(binding.id.to_id());
        }
        declarator.visit_children_with(self);
    }
}

fn is_async(function: &Function) -> bool {
    function.is_async && !function.is_generator || returns_promise(function.return_type.as_deref())
}

fn is_async_arrow(arrow: &ArrowExpr) -> bool {
    arrow.is_async || returns_promise(arrow.return_type.as_deref())
}

fn is_async_function(expr: &Expr) -> bool {
    match unwrap(expr) {
        Expr::Arrow(arrow) => is_async_arrow(arrow),
        Expr::Fn(function) => is_async(&function.function),
        _ => false,
    }
}

fn returns_promise(annotation: Option<&TsTypeAnn>) -> bool {
    matches!(
        annotation.map(|annotation| &*annotation.type_ann),
        Some(TsType::TsTypeRef(TsTypeRef { type_name: TsEntityName::Ident(name), .. }))
            if &*name.sym == "Promise"
    )
}

/// An expression without the parentheses and type assertions around it.
fn unwrap(expr: &Expr) -> &Expr {
    match expr {
        Expr::Paren(inner) => unwrap(&inner.expr),
        Expr::TsAs(inner) => unwrap(&inner.expr),
        Expr::TsSatisfies(inner) => unwrap(&inner.expr),
        Expr::TsNonNull(inner) => unwrap(&inner.expr),
        Expr::TsTypeAssertion(inner) => unwrap(&inner.expr),
        Expr::TsConstAssertion(inner) => unwrap(&inner.expr),
        Expr::TsInstantiation(inner) => unwrap(&inner.expr),
        expr => expr,
    }
}

/// A call, plain or in an optional chain (`a?.b()`).
struct Call<'a> {
    callee: &'a Expr,
    args: &'a [ExprOrSpread],
}

fn as_call(expr: &Expr) -> Option<Call<'_>> {
    match expr {
        Expr::Call(call) => match &call.callee {
            Callee::Expr(callee) => Some(Call {
                callee,
                args: &call.args,
            }),
            Callee::Super(_) | Callee::Import(_) => None,
        },
        Expr::OptChain(chain) => match &*chain.base {
            OptChainBase::Call(call) => Some(Call {
                callee: &call.callee,
                args: &call.args,
            }),
            OptChainBase::Member(_) => None,
        },
        _ => None,
    }
}

/// A member access, plain or in an optional chain (`a?.b`).
fn as_member(expr: &Expr) -> Option<&MemberExpr> {
    match expr {
        Expr::Member(member) => Some(member),
        Expr::OptChain(chain) => match &*chain.base {
            OptChainBase::Member(member) => Some(member),
            OptChainBase::Call(_) => None,
        },
        _ => None,
    }
}

fn property(member: &MemberExpr) -> Option<&str> {
    match &member.prop {
        MemberProp::Ident(name) => Some(&name.sym),
        MemberProp::PrivateName(_) | MemberProp::Computed(_) => None,
    }
}

/// The name a chain of member accesses starts from: `tools` in `tools.github.list_issues`.
fn root(expr: &Expr) -> Option<&Ident> {
    match unwrap(expr) {
        Expr::Ident(ident) => Some(ident),
        expr => root(&as_member(expr)?.obj),
    }
}

struct Rules<'a> {
    scope: Scope,
    source: &'a str,
    start: StartSourcePos,
    findings: Vec<Finding>,
}

impl Rules<'_> {
    fn offset(&self, span: Span) -> (usize, usize) {
        let offset = |position| {
            SourcePos::unsafely_from_byte_pos(position)
                .as_byte_index(self.start)
                .min(self.source.len())
        };
        (offset(span.lo), offset(span.hi))
    }

    /// The source of a span on one line, shortened: `github.list_issues`.
    fn text(&self, span: Span) -> Option<String> {
        let (start, end) = self.offset(span);
        let text = self.source.get(start..end)?;
        let text: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        (text.chars().count() <= 40).then_some(text)
    }

    fn report(&mut self, span: Span, rule: &'static str, message: String, hint: String) {
        let (start, end) = self.offset(span);
        self.findings.push(Finding {
            start,
            end,
            rule,
            message,
            hint: Some(hint),
            global: None,
        });
    }

    /// Reports the promises an expression statement leaves floating.
    fn floating(&mut self, expr: &Expr) {
        match unwrap(expr) {
            Expr::Unary(unary) if unary.op == UnaryOp::Void => {}
            Expr::Seq(sequence) => {
                for expr in &sequence.exprs {
                    self.floating(expr);
                }
            }
            Expr::Cond(conditional) => {
                self.floating(&conditional.cons);
                self.floating(&conditional.alt);
            }
            Expr::Bin(binary)
                if matches!(
                    binary.op,
                    BinaryOp::LogicalAnd | BinaryOp::LogicalOr | BinaryOp::NullishCoalescing
                ) =>
            {
                self.floating(&binary.right);
            }
            expr => {
                let Some((callee, kind)) = self.scope.promise_call(expr) else {
                    return;
                };
                let name = self.text(callee.span());
                let call = name
                    .as_deref()
                    .map_or_else(|| "This call".to_owned(), |name| format!("`{name}(…)`"));
                let message = match kind {
                    Origin::Tool => format!(
                        "{call} calls a tool, and its promise is neither awaited, returned nor \
                         assigned: the script goes on without waiting for the call, and loses \
                         its result and its errors."
                    ),
                    Origin::Ptc => format!(
                        "{call} returns a promise that is neither awaited, returned nor \
                         assigned: the script goes on at once instead of waiting."
                    ),
                    Origin::Script => format!(
                        "{call} is async, and its promise is neither awaited, returned nor \
                         assigned: the script goes on without waiting for it, and loses its \
                         errors."
                    ),
                    Origin::Combinator => format!(
                        "{call} returns a promise that is neither awaited, returned nor \
                         assigned: the script goes on without waiting for the calls it \
                         groups, and loses their errors."
                    ),
                    Origin::Immediate => "This async function is called, and its promise is \
                                           neither awaited, returned nor assigned: the script \
                                           goes on without waiting for it, and loses its \
                                           errors."
                        .to_owned(),
                };
                let span = match kind {
                    Origin::Immediate => expr.span(),
                    _ => callee.span(),
                };
                self.report(
                    span,
                    FLOATING_PROMISE,
                    message,
                    "Write `await` before the call. If not waiting for it is intended, write \
                     `void` before it."
                        .to_owned(),
                );
            }
        }
    }

    /// Reports an async callback given to a method that does not await what it returns.
    fn callback(&mut self, call: &Call<'_>) {
        let Some(member) = as_member(unwrap(call.callee)) else {
            return;
        };
        let Some(method) = property(member) else {
            return;
        };
        if method != "forEach" && !PREDICATES.contains(&method) {
            return;
        }
        let Some(callback) = call.args.first().filter(|arg| arg.spread.is_none()) else {
            return;
        };
        if !self.scope.is_async_callback(&callback.expr) {
            return;
        }
        let items = self
            .text(member.obj.span())
            .filter(|items| items.chars().count() <= 30)
            .unwrap_or_else(|| "items".to_owned());
        let (message, hint) = if method == "forEach" {
            (
                "`forEach` ignores what its callback returns, so the promises of this async \
                 callback are never awaited: the calls start together, the script goes on \
                 before they end, and it loses their errors."
                    .to_owned(),
                format!(
                    "Use `for (const item of {items}) {{ await … }}` to make the calls one by \
                     one, or `await Promise.all({items}.map(async (item) => …))` to make them \
                     together."
                ),
            )
        } else {
            (
                format!(
                    "`{method}` reads what its callback returns as a boolean, and the promise \
                     of an async callback is always true: the awaited value is never used."
                ),
                format!(
                    "Await the values first, with `const values = await \
                     Promise.all({items}.map(async (item) => …))`, then call `{method}` with a \
                     callback that is not async."
                ),
            )
        };
        self.report(member.prop.span(), IGNORED_ASYNC_CALLBACK, message, hint);
    }

    fn absent(&mut self, name: &str, span: Span) {
        let Some(hint) = self.scope.absent(name) else {
            return;
        };
        let (start, end) = self.offset(span);
        self.findings.push(Finding {
            start,
            end,
            rule: ABSENT_GLOBAL,
            message: format!("`{name}` does not exist in ai-ptc scripts."),
            hint: Some(hint.clone()),
            global: Some(name.to_owned()),
        });
    }
}

impl Visit for Rules<'_> {
    fn visit_expr_stmt(&mut self, statement: &ExprStmt) {
        self.floating(&statement.expr);
        statement.visit_children_with(self);
    }

    fn visit_expr(&mut self, expr: &Expr) {
        if let Some(call) = as_call(expr) {
            self.callback(&call);
        }
        // `globalThis.process`.
        if let Some(member) = as_member(expr)
            && let (Expr::Ident(object), MemberProp::Ident(name)) =
                (unwrap(&member.obj), &member.prop)
            && self.scope.is_global(object, "globalThis")
        {
            self.absent(&name.sym, name.span);
        }
        expr.visit_children_with(self);
    }

    /// Names in expressions and in types; property names are not identifiers of this kind.
    fn visit_ident(&mut self, ident: &Ident) {
        if ident.ctxt == self.scope.unresolved {
            self.absent(&ident.sym, ident.span);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(rule, flagged text)` of each finding.
    fn findings(source: &str, http_tool: bool) -> Vec<(&'static str, String)> {
        check(source, http_tool)
            .into_iter()
            .map(|finding| (finding.rule, source[finding.start..finding.end].to_owned()))
            .collect()
    }

    fn floating(body: &str) -> Vec<String> {
        let source = format!(
            "import {{ github, ask_user }} from \"ptc:tools\";\nimport * as tools from \"ptc:tools\";\nimport {{ sleep, signal as wait, run }} from \"ptc\";\nimport * as ptc from \"ptc\";\nexport default async function main() {{\n{body}\n}}\n"
        );
        findings(&source, false)
            .into_iter()
            .filter(|(rule, _)| *rule == FLOATING_PROMISE)
            .map(|(_, text)| text)
            .collect()
    }

    #[test]
    fn floating_promises_are_found() {
        let cases: [(&str, &[&str]); 16] = [
            (
                "github.list_issues({ repo: \"a\" });",
                &["github.list_issues"],
            ),
            ("ask_user({ question: \"?\" });", &["ask_user"]),
            (
                "tools.github.list_issues({});",
                &["tools.github.list_issues"],
            ),
            ("sleep(\"1h\");", &["sleep"]),
            ("wait(\"approval\");", &["wait"]),
            ("ptc.webhook(\"done\");", &["ptc.webhook"]),
            ("Promise.all([1]);", &["Promise.all"]),
            (
                "github.list_issues({}).then((issues) => issues.length);",
                &["github.list_issues"],
            ),
            (
                "ask_user({}).catch(() => 1).finally(() => 2);",
                &["ask_user"],
            ),
            (
                "(github.list_issues({}) as unknown);",
                &["github.list_issues"],
            ),
            ("1, sleep(1);", &["sleep"]),
            ("true ? sleep(1) : ask_user({});", &["sleep", "ask_user"]),
            ("run.id && sleep(1);", &["sleep"]),
            ("github?.list_issues({});", &["github?.list_issues"]),
            (
                "(async () => { await sleep(1); })();",
                &["(async () => { await sleep(1); })()"],
            ),
            (
                "if (run.id) sleep(1); else { for (;;) { ask_user({}); } }",
                &["sleep", "ask_user"],
            ),
        ];
        for (body, expected) in cases {
            assert_eq!(floating(body), expected.to_vec(), "{body}");
        }
    }

    #[test]
    fn awaited_returned_assigned_or_voided_promises_are_not_floating() {
        for body in [
            "await github.list_issues({});",
            "return github.list_issues({});",
            "const issues = github.list_issues({});",
            "let p; p = sleep(1);",
            "void sleep(1);",
            "void github.list_issues({}).then(() => 1);",
            "const all = await Promise.all([1, 2].map((n) => ask_user({ n })));",
            "const f = () => sleep(1);",
            "run.id;",
            "console.log(run.id);",
            "Promise.resolve(1);",
            "[1].map(async (n) => n);",
            "github.list_issues;",
        ] {
            assert_eq!(floating(body), Vec::<String>::new(), "{body}");
        }
    }

    #[test]
    fn async_functions_of_the_script_float_too() {
        let source = "import { sleep } from \"ptc\";\nasync function load() { await sleep(1); }\nconst save = async () => {};\nconst keep = function (): Promise<void> { return sleep(1); };\nfunction plain() { return 1; }\nasync function* items() {}\nexport default async function main() {\n  load();\n  save();\n  keep();\n  plain();\n  items();\n  {\n    const load = () => 1;\n    load();\n  }\n}\n";
        let floating: Vec<String> = findings(source, false)
            .into_iter()
            .map(|(_, text)| text)
            .collect();
        assert_eq!(floating, ["load", "save", "keep"]);
    }

    #[test]
    fn shadowed_imports_are_not_tools() {
        let source = "import { github } from \"ptc:tools\";\nimport { sleep } from \"ptc\";\nexport default async function main(sleep: (n: number) => void) {\n  const github = { list_issues() {} };\n  github.list_issues();\n  sleep(1);\n}\n";
        assert_eq!(findings(source, false), []);
    }

    #[test]
    fn fetch_floats_only_with_the_http_tool() {
        let source = "export default async function main() {\n  fetch(\"https://example.com\");\n  globalThis.fetch(\"https://example.com\");\n}\n";
        assert_eq!(
            findings(source, true),
            [
                (FLOATING_PROMISE, "fetch".to_owned()),
                (FLOATING_PROMISE, "globalThis.fetch".to_owned())
            ]
        );
        // Without it, fetch is an absent global.
        assert_eq!(
            findings(source, false),
            [
                (ABSENT_GLOBAL, "fetch".to_owned()),
                (ABSENT_GLOBAL, "fetch".to_owned())
            ]
        );
    }

    #[test]
    fn async_callbacks_given_to_for_each_and_predicates_are_found() {
        let source = "import { github } from \"ptc:tools\";\nasync function close(n: number) { await github.close_issue({ n }); }\nexport default async function main(input: { numbers: number[] }) {\n  input.numbers.forEach(async (n) => { await close(n); });\n  input.numbers.forEach(close);\n  new Map<string, number>()?.forEach(async function (n) { await close(n); });\n  const open = input.numbers.filter(async (n) => (await github.get({ n })).open);\n  input.numbers.forEach((n) => console.log(n));\n  const all = await Promise.all(input.numbers.map(async (n) => close(n)));\n  input.numbers.reduce(async (total, n) => (await total) + n, Promise.resolve(0));\n}\n";
        let found = findings(source, false);
        assert_eq!(
            found,
            [
                (IGNORED_ASYNC_CALLBACK, "forEach".to_owned()),
                (IGNORED_ASYNC_CALLBACK, "forEach".to_owned()),
                (IGNORED_ASYNC_CALLBACK, "forEach".to_owned()),
                (IGNORED_ASYNC_CALLBACK, "filter".to_owned())
            ]
        );
        let first = &check(source, false)[0];
        assert!(
            first
                .hint
                .as_deref()
                .unwrap()
                .contains("for (const item of input.numbers)"),
            "{first:?}"
        );
    }

    #[test]
    fn absent_globals_are_found_where_they_refer_to_the_global() {
        let source = "export default async function main(input: { b: Buffer }) {\n  const env = process.env;\n  const fs = require(\"fs\");\n  const b = Buffer.from(\"x\");\n  if (typeof process !== \"undefined\") {}\n  const x = globalThis.process;\n  const y = { process: 1 }.process;\n  return { env, fs, b, x, y };\n}\nfunction local(process: number, require: () => void) { require(); return process; }\n";
        let found: Vec<String> = findings(source, false)
            .into_iter()
            .map(|(rule, text)| {
                assert_eq!(rule, ABSENT_GLOBAL);
                text
            })
            .collect();
        assert_eq!(
            found,
            [
                "Buffer", "process", "require", "Buffer", "process", "process"
            ]
        );
        let first = &check(source, false)[0];
        assert_eq!(first.global.as_deref(), Some("Buffer"));
        assert_eq!(first.message, "`Buffer` does not exist in ai-ptc scripts.");
        assert!(first.hint.as_deref().unwrap().contains("TextEncoder"));
    }

    #[test]
    fn a_script_that_does_not_parse_has_no_findings() {
        assert_eq!(check("export default function (", false), []);
    }
}
