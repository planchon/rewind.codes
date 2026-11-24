use std::collections::HashMap;

use rewind_core::{Code, Digest};

use crate::ports::{
    ActionDeclarations, CheckerError, Diagnostic, TypeChecker, WorkflowDeclarations,
};

/// `TypeChecker` that answers from a table: the diagnostics given for a code's digest, none
/// for any other code. For engine tests and wherever tsgo is not available.
#[derive(Debug, Clone, Default)]
pub struct StaticTypeChecker {
    diagnostics: HashMap<Digest, Vec<Diagnostic>>,
}

impl StaticTypeChecker {
    /// A checker that accepts every code.
    pub fn new() -> Self {
        Self::default()
    }

    /// Reports `diagnostics` for `code`.
    pub fn with(mut self, code: &Code, diagnostics: Vec<Diagnostic>) -> Self {
        self.diagnostics.insert(code.digest(), diagnostics);
        self
    }
}

impl TypeChecker for StaticTypeChecker {
    async fn check(
        &self,
        code: &Code,
        _declarations: &ActionDeclarations,
    ) -> Result<Vec<Diagnostic>, CheckerError> {
        Ok(self
            .diagnostics
            .get(&code.digest())
            .cloned()
            .unwrap_or_default())
    }

    async fn check_workflow(
        &self,
        code: &Code,
        declarations: &ActionDeclarations,
        _workflow: &WorkflowDeclarations,
    ) -> Result<Vec<Diagnostic>, CheckerError> {
        self.check(code, declarations).await
    }
}
