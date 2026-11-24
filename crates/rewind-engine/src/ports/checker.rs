use std::fmt;
use std::future::Future;

use rewind_core::Code;

/// What `rewind-codegen` generated from a pinned action set, stored with it: the declarations of
/// `rewind:actions` for the type-checker, and their compact rendering for the model's prompt
/// (`archi/04-tools.md`, "Action sets and code generation").
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActionDeclarations {
    /// `rewind-actions.d.ts`.
    pub typescript: String,
    /// The compact prompt rendering: `github.list_issues({ repo, state? }) → Issue[] · …`.
    pub prompt: String,
}

/// What `rewind-codegen` generates from a workflow's schemas, to check its own code when it is
/// registered (`archi/04-tools.md`, "Type-checking").
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkflowDeclarations {
    /// Declarations that augment the `rewind` module with the workflow's signals: its `Signals`
    /// interface, so that `signal()` and `webhook()` take only their names and `signal()`
    /// returns their types.
    pub signals: String,
    /// `entry.ts`: a module that imports the code's default export from `./code` and
    /// assigns it to a function from the input type to the output type. A diagnostic there
    /// means the default export does not match the workflow's schemas.
    pub entry: String,
}

/// One problem found in a code, written for a model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    /// Position in `workflow.ts`, 1-based.
    pub line: u32,
    pub column: u32,
    pub severity: Severity,
    pub source: DiagnosticSource,
    pub message: String,
    /// What to do instead, for the checker's own rules (`fetch` is absent: use…).
    pub hint: Option<String>,
    /// A few lines of the code around the position, the position marked.
    pub frame: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Severity {
    /// The code is refused.
    Error,
    /// Reported, not refused.
    Warning,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum DiagnosticSource {
    /// The TypeScript compiler, with its error code (`2322` for TS2322).
    TypeScript { code: u32 },
    /// One of the checker's own rules (`floating-promise`, `ignored-async-callback`,
    /// `absent-global`).
    Rule(String),
}

impl fmt::Display for Diagnostic {
    /// `workflow.ts:3:28 error TS2322: Type 'number' is not assignable to type 'string'.`, then
    /// the hint and the frame.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let severity = match self.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        let source = match &self.source {
            DiagnosticSource::TypeScript { code } => format!("TS{code}"),
            DiagnosticSource::Rule(rule) => rule.clone(),
        };
        write!(
            f,
            "workflow.ts:{}:{} {severity} {source}: {}",
            self.line, self.column, self.message
        )?;
        if let Some(hint) = &self.hint {
            write!(f, "\n  hint: {hint}")?;
        }
        if !self.frame.is_empty() {
            write!(f, "\n{}", self.frame)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CheckerError {
    /// The checker itself failed: tsgo could not be started, crashed, timed out.
    #[error("the type-checker failed: {0}")]
    Internal(String),
}

/// Type-checks a code against the sandbox lib and an action set's declarations, in strict mode,
/// before anything runs (`archi/04-tools.md`, "Type-checking").
///
/// Deterministic: the same code and declarations give the same diagnostics, in the order of
/// their positions. The diagnostics include the checker's own rules. A code is refused when a
/// diagnostic is an [`Severity::Error`].
pub trait TypeChecker: Send + Sync {
    fn check(
        &self,
        code: &Code,
        declarations: &ActionDeclarations,
    ) -> impl Future<Output = Result<Vec<Diagnostic>, CheckerError>> + Send;

    /// Type-checks a workflow's code as [`check`](Self::check) does, with the workflow's own
    /// declarations too: its signals, and its `entry.ts`, whose diagnostics are reported at the
    /// code's default export.
    fn check_workflow(
        &self,
        code: &Code,
        declarations: &ActionDeclarations,
        workflow: &WorkflowDeclarations,
    ) -> impl Future<Output = Result<Vec<Diagnostic>, CheckerError>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_read_like_compiler_output() {
        let diagnostic = Diagnostic {
            line: 3,
            column: 28,
            severity: Severity::Error,
            source: DiagnosticSource::TypeScript { code: 2322 },
            message: "Type 'number' is not assignable to type 'string'.".into(),
            hint: None,
            frame: "3 | github.list_issues({ repo: 42 })\n  |                            ^".into(),
        };
        assert_eq!(
            diagnostic.to_string(),
            "workflow.ts:3:28 error TS2322: Type 'number' is not assignable to type 'string'.\n3 | github.list_issues({ repo: 42 })\n  |                            ^"
        );
        let rule = Diagnostic {
            source: DiagnosticSource::Rule("absent-global".into()),
            hint: Some("use the http action".into()),
            frame: String::new(),
            ..diagnostic
        };
        assert_eq!(
            rule.to_string(),
            "workflow.ts:3:28 error absent-global: Type 'number' is not assignable to type 'string'.\n  hint: use the http action"
        );
    }
}
