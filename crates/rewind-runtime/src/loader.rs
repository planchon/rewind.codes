//! The modules of an isolate, and the import rules.
//!
//! A code may only import `rewind`, `rewind:actions` and `rewind:workflows`; `rewind:internal`
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

use crate::compile::CODE_SPECIFIER;
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
    Code {
        glue: &'static Glue,
        code: String,
        actions: String,
        workflows: String,
    },
    Mocks {
        javascript: String,
    },
}

impl Loader {
    /// The modules of a run: its entry, the glue's `rewind` modules, its `rewind:actions` and
    /// `rewind:workflows`, its code.
    pub(crate) fn code(
        glue: &'static Glue,
        code: String,
        source_map: Vec<u8>,
        actions: String,
        workflows: String,
    ) -> Self {
        Self {
            modules: Modules::Code {
                glue,
                code,
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
            Modules::Code { .. } => CODE_SPECIFIER,
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
                "dynamic import() is not available in Rewind workflows",
            ));
        }
        let main = matches!(kind, ResolutionKind::MainModule);
        let target = match (&self.modules, referrer, specifier) {
            (Modules::Code { .. }, _, ENTRY_SPECIFIER) if main => specifier,
            (Modules::Code { .. }, ENTRY_SPECIFIER, "rewind:internal" | CODE_SPECIFIER) => {
                specifier
            }
            (
                Modules::Code { .. },
                "rewind:std" | "rewind:actions" | "rewind:workflows",
                "rewind:internal",
            ) => specifier,
            (Modules::Code { .. }, CODE_SPECIFIER, "rewind") => "rewind:std",
            (Modules::Code { .. }, CODE_SPECIFIER, "rewind:actions" | OLD_ACTIONS_SPECIFIER) => {
                "rewind:actions"
            }
            (Modules::Code { .. }, CODE_SPECIFIER, "rewind:workflows") => "rewind:workflows",
            (Modules::Code { .. }, CODE_SPECIFIER, other) => {
                return Err(JsErrorBox::type_error(format!(
                    "cannot import {other:?}: workflows can only import \"rewind\", \
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
            (Modules::Code { glue, .. }, ENTRY_SPECIFIER) => glue.entry.to_owned(),
            (Modules::Code { glue, .. }, "rewind:internal") => glue.internal.to_owned(),
            (Modules::Code { glue, .. }, "rewind:std") => glue.std.to_owned(),
            (Modules::Code { actions, .. }, "rewind:actions") => actions.clone(),
            (Modules::Code { workflows, .. }, "rewind:workflows") => workflows.clone(),
            (Modules::Code { code, .. }, CODE_SPECIFIER) => code.clone(),
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
