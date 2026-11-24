//! The modules of an isolate, and the import rules.
//!
//! A code may only import `rewind`, `rewind:actions` and `rewind:workflows`; `rewind:internal`
//! is reserved to the runtime's own modules; there is no dynamic `import()`, no npm, no relative
//! import. A module of handlers imports nothing.
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

use crate::epochs::Glue;

pub(crate) const ENTRY_SPECIFIER: &str = "rewind:event";
/// The name `rewind:actions` had, which code already written imports (R1).
pub(crate) const OLD_ACTIONS_SPECIFIER: &str = "rewind:tools";
/// Specifier of a module of handlers.
pub(crate) const HANDLERS_SPECIFIER: &str = "file:///handlers.ts";

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
    Handlers {
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
            source_map: source_map_of(glue, source_map),
        }
    }

    /// One module of handlers.
    pub(crate) fn handlers(javascript: String, source_map: Vec<u8>) -> Self {
        Self {
            modules: Modules::Handlers { javascript },
            source_map,
        }
    }

    fn file_name(&self) -> &'static str {
        match self.modules {
            Modules::Code { glue, .. } => glue.code_specifier,
            Modules::Handlers { .. } => HANDLERS_SPECIFIER,
        }
    }
}

/// The source map of a code, naming the module its epoch loads it as: a code compiled under one
/// epoch's name (`workflow.ts`, `script.ts` before the rename release) shows the name of the
/// epoch its run is of in its stacks.
fn source_map_of(glue: &Glue, source_map: Vec<u8>) -> Vec<u8> {
    let Ok(text) = String::from_utf8(source_map) else {
        return Vec::new();
    };
    let mut text = text;
    for name in CODE_FILES {
        if name != glue.code_specifier {
            text = text.replace(
                &format!("\"{name}\""),
                &format!("\"{}\"", glue.code_specifier),
            );
        }
    }
    text.into_bytes()
}

/// The modules a code has been loaded as, every epoch's.
const CODE_FILES: [&str; 2] = ["file:///script.ts", "file:///workflow.ts"];

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
        let code_specifier = self.file_name();
        let target = match (&self.modules, referrer, specifier) {
            (Modules::Code { .. }, _, ENTRY_SPECIFIER) if main => specifier,
            (Modules::Code { .. }, ENTRY_SPECIFIER, "rewind:internal") => specifier,
            (Modules::Code { .. }, ENTRY_SPECIFIER, code) if code == code_specifier => specifier,
            (
                Modules::Code { .. },
                "rewind:std" | "rewind:actions" | "rewind:workflows",
                "rewind:internal",
            ) => specifier,
            (Modules::Code { .. }, code, "rewind") if code == code_specifier => "rewind:std",
            (Modules::Code { .. }, code, "rewind:actions" | OLD_ACTIONS_SPECIFIER)
                if code == code_specifier =>
            {
                "rewind:actions"
            }
            (Modules::Code { .. }, code, "rewind:workflows") if code == code_specifier => {
                "rewind:workflows"
            }
            (Modules::Code { .. }, code, other) if code == code_specifier => {
                return Err(JsErrorBox::type_error(format!(
                    "cannot import {other:?}: workflows can only import \"rewind\", \
                     \"rewind:actions\" and \"rewind:workflows\""
                )));
            }
            (Modules::Handlers { .. }, _, HANDLERS_SPECIFIER) if main => specifier,
            (Modules::Handlers { .. }, HANDLERS_SPECIFIER, other) => {
                return Err(JsErrorBox::type_error(format!(
                    "cannot import {other:?}: handlers cannot import modules"
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
            (Modules::Code { code, glue, .. }, specifier) if specifier == glue.code_specifier => {
                code.clone()
            }
            (Modules::Handlers { javascript }, HANDLERS_SPECIFIER) => javascript.clone(),
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
