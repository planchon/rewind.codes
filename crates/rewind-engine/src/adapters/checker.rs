use std::collections::HashMap;

use rewind_core::{Digest, Script};

use crate::ports::{CheckerError, Diagnostic, ToolDeclarations, TypeChecker, WorkflowDeclarations};

/// `TypeChecker` that answers from a table: the diagnostics given for a script's digest, none
/// for any other script. For engine tests and wherever tsgo is not available.
#[derive(Debug, Clone, Default)]
pub struct StaticTypeChecker {
    diagnostics: HashMap<Digest, Vec<Diagnostic>>,
}

impl StaticTypeChecker {
    /// A checker that accepts every script.
    pub fn new() -> Self {
        Self::default()
    }

    /// Reports `diagnostics` for `script`.
    pub fn with(mut self, script: &Script, diagnostics: Vec<Diagnostic>) -> Self {
        self.diagnostics.insert(script.digest(), diagnostics);
        self
    }
}

impl TypeChecker for StaticTypeChecker {
    async fn check(
        &self,
        script: &Script,
        _declarations: &ToolDeclarations,
    ) -> Result<Vec<Diagnostic>, CheckerError> {
        Ok(self
            .diagnostics
            .get(&script.digest())
            .cloned()
            .unwrap_or_default())
    }

    async fn check_workflow(
        &self,
        script: &Script,
        declarations: &ToolDeclarations,
        _workflow: &WorkflowDeclarations,
    ) -> Result<Vec<Diagnostic>, CheckerError> {
        self.check(script, declarations).await
    }
}
