//! The modules of an isolate, and the import rules.
//!
//! A script may only import `rewind`, `rewind:actions` and `rewind:workflows`; `rewind:internal`
//! is reserved to the runtime's own modules; there is no dynamic `import()`, no npm, no relative
//! import. A module of mock handlers imports nothing.
//!
//! Code written before actions were named imports `rewind:tools`: it resolves to the module
//! `rewind:actions` itself, so that both names give the same exports, and nothing declares it
//! (`archi/08-decisions.md`, R1).

use std::borrow::Cow;

use deno_core::{
    ModuleLoadOptions, ModuleLoadReferrer, ModuleLoadResponse, ModuleLoader, ModuleSource,
    ModuleSourceCode, ModuleSpecifier, ModuleType, ResolutionKind,
};
use deno_error::JsErrorBox;

use crate::compile::SCRIPT_SPECIFIER;
use crate::epochs::Glue;

pub(crate) const ENTRY_SPECIFIER: &str = "rewind:entry";
/// The name `rewind:actions` had, which code already written imports (R1).
pub(crate) const OLD_ACTIONS_SPECIFIER: &str = "rewind:tools";
/// Specifier of a module of mock handlers.
pub(crate) const MOCKS_SPECIFIER: &str = "file:///mocks.ts";

pub(crate) struct Loader {
    modules: Modules,
    source_map: Vec<u8>,
}

enum Modules {
    Script {
        glue: &'static Glue,
        script: String,
        actions: String,
        workflows: String,
    },
    Mocks {
        javascript: String,
    },
}

impl Loader {
    /// The modules of a run: its entry, the glue's `rewind` modules, its `rewind:actions` and
    /// `rewind:workflows`, its script.
    pub(crate) fn script(
        glue: &'static Glue,
        script: String,
        source_map: Vec<u8>,
        actions: String,
        workflows: String,
    ) -> Self {
        Self {
            modules: Modules::Script {
                glue,
                script,
                actions,
                workflows,
            },
            source_map,
        }
    }

    /// One module of mock handlers.
    pub(crate) fn mocks(javascript: String, source_map: Vec<u8>) -> Self {
        Self {
            modules: Modules::Mocks { javascript },
            source_map,
        }
    }

    fn file_name(&self) -> &'static str {
        match self.modules {
            Modules::Script { .. } => SCRIPT_SPECIFIER,
            Modules::Mocks { .. } => MOCKS_SPECIFIER,
        }
    }
}

impl ModuleLoader for Loader {
    fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        kind: ResolutionKind,
    ) -> Result<ModuleSpecifier, JsErrorBox> {
        if matches!(kind, ResolutionKind::DynamicImport) {
            return Err(JsErrorBox::type_error(
                "dynamic import() is not available in Rewind scripts",
            ));
        }
        let main = matches!(kind, ResolutionKind::MainModule);
        let target = match (&self.modules, referrer, specifier) {
            (Modules::Script { .. }, _, ENTRY_SPECIFIER) if main => specifier,
            (Modules::Script { .. }, ENTRY_SPECIFIER, "rewind:internal" | SCRIPT_SPECIFIER) => {
                specifier
            }
            (
                Modules::Script { .. },
                "rewind:std" | "rewind:actions" | "rewind:workflows",
                "rewind:internal",
            ) => specifier,
            (Modules::Script { .. }, SCRIPT_SPECIFIER, "rewind") => "rewind:std",
            (
                Modules::Script { .. },
                SCRIPT_SPECIFIER,
                "rewind:actions" | OLD_ACTIONS_SPECIFIER,
            ) => "rewind:actions",
            (Modules::Script { .. }, SCRIPT_SPECIFIER, "rewind:workflows") => "rewind:workflows",
            (Modules::Script { .. }, SCRIPT_SPECIFIER, other) => {
                return Err(JsErrorBox::type_error(format!(
                    "cannot import {other:?}: scripts can only import \"rewind\", \
                     \"rewind:actions\" and \"rewind:workflows\""
                )));
            }
            (Modules::Mocks { .. }, _, MOCKS_SPECIFIER) if main => specifier,
            (Modules::Mocks { .. }, MOCKS_SPECIFIER, other) => {
                return Err(JsErrorBox::type_error(format!(
                    "cannot import {other:?}: mock handlers cannot import modules"
                )));
            }
            _ => {
                return Err(JsErrorBox::type_error(format!(
                    "cannot import {specifier:?} from {referrer:?}"
                )));
            }
        };
        ModuleSpecifier::parse(target).map_err(JsErrorBox::from_err)
    }

    fn load(
        &self,
        specifier: &ModuleSpecifier,
        _referrer: Option<&ModuleLoadReferrer>,
        _options: ModuleLoadOptions,
    ) -> ModuleLoadResponse {
        let code = match (&self.modules, specifier.as_str()) {
            (Modules::Script { glue, .. }, ENTRY_SPECIFIER) => glue.entry.to_owned(),
            (Modules::Script { glue, .. }, "rewind:internal") => glue.internal.to_owned(),
            (Modules::Script { glue, .. }, "rewind:std") => glue.std.to_owned(),
            (Modules::Script { actions, .. }, "rewind:actions") => actions.clone(),
            (Modules::Script { workflows, .. }, "rewind:workflows") => workflows.clone(),
            (Modules::Script { script, .. }, SCRIPT_SPECIFIER) => script.clone(),
            (Modules::Mocks { javascript }, MOCKS_SPECIFIER) => javascript.clone(),
            (_, other) => {
                return ModuleLoadResponse::Sync(Err(JsErrorBox::generic(format!(
                    "module not found: {other}"
                ))));
            }
        };
        ModuleLoadResponse::Sync(Ok(ModuleSource::new(
            ModuleType::JavaScript,
            ModuleSourceCode::String(code.into()),
            specifier,
            None,
        )))
    }

    fn get_source_map(&self, file_name: &str) -> Option<Cow<'_, [u8]>> {
        (file_name == self.file_name()).then_some(Cow::Borrowed(self.source_map.as_slice()))
    }
}
