use std::fmt;
use std::future::Future;

use ptc_core::Script;

/// What `ptc-codegen` generated from a pinned toolset, stored with it: the declarations of
/// `ptc:tools` for the type-checker, and their compact rendering for the model's prompt
/// (`archi/04-tools.md`, "Toolsets and code generation").
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolDeclarations {
    /// `ptc-tools.d.ts`.
    pub typescript: String,
    /// The compact prompt rendering: `github.list_issues({ repo, state? }) → Issue[] · …`.
    pub prompt: String,
}

/// One problem found in a script, written for a model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    /// Position in `script.ts`, 1-based.
    pub line: u32,
    pub column: u32,
    pub severity: Severity,
    pub source: DiagnosticSource,
    pub message: String,
    /// What to do instead, for the checker's own rules (`fetch` is absent: use…).
    pub hint: Option<String>,
    /// A few lines of the script around the position, the position marked.
    pub frame: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Severity {
    /// The script is refused.
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
    /// `script.ts:3:28 error TS2322: Type 'number' is not assignable to type 'string'.`, then
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
            "script.ts:{}:{} {severity} {source}: {}",
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

/// Type-checks a script against the sandbox lib and a toolset's declarations, in strict mode,
/// before anything runs (`archi/04-tools.md`, "Type-checking").
///
/// Deterministic: the same script and declarations give the same diagnostics, in the order of
/// their positions. The diagnostics include the checker's own rules. A script is refused when a
/// diagnostic is an [`Severity::Error`].
pub trait TypeChecker: Send + Sync {
    fn check(
        &self,
        script: &Script,
        declarations: &ToolDeclarations,
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
            "script.ts:3:28 error TS2322: Type 'number' is not assignable to type 'string'.\n3 | github.list_issues({ repo: 42 })\n  |                            ^"
        );
        let rule = Diagnostic {
            source: DiagnosticSource::Rule("absent-global".into()),
            hint: Some("use the http tool".into()),
            frame: String::new(),
            ..diagnostic
        };
        assert_eq!(
            rule.to_string(),
            "script.ts:3:28 error absent-global: Type 'number' is not assignable to type 'string'.\n  hint: use the http tool"
        );
    }
}
