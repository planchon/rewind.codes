//! The modules of a run, and the import rules: a script may only import `ptc` and
//! `ptc:tools`; `ptc:internal` is reserved to the runtime's own modules; there is no dynamic
//! `import()`, no npm, no relative import.

use std::borrow::Cow;

use deno_core::{
    ModuleLoadOptions, ModuleLoadReferrer, ModuleLoadResponse, ModuleLoader, ModuleSource,
    ModuleSourceCode, ModuleSpecifier, ModuleType, ResolutionKind,
};
use deno_error::JsErrorBox;

use crate::compile::SCRIPT_SPECIFIER;
use crate::epochs::Glue;

pub(crate) const ENTRY_SPECIFIER: &str = "ptc:entry";

pub(crate) struct Loader {
    pub(crate) glue: &'static Glue,
    pub(crate) script: String,
    pub(crate) source_map: Vec<u8>,
    pub(crate) tools: String,
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
                "dynamic import() is not available in ai-ptc scripts",
            ));
        }
        let target = match (referrer, specifier) {
            _ if matches!(kind, ResolutionKind::MainModule) && specifier == ENTRY_SPECIFIER => {
                specifier
            }
            (ENTRY_SPECIFIER, "ptc:internal" | SCRIPT_SPECIFIER) => specifier,
            ("ptc:std" | "ptc:tools", "ptc:internal") => specifier,
            (SCRIPT_SPECIFIER, "ptc") => "ptc:std",
            (SCRIPT_SPECIFIER, "ptc:tools") => "ptc:tools",
            (SCRIPT_SPECIFIER, other) => {
                return Err(JsErrorBox::type_error(format!(
                    "cannot import {other:?}: scripts can only import \"ptc\" and \"ptc:tools\""
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
        let code = match specifier.as_str() {
            ENTRY_SPECIFIER => self.glue.entry.to_owned(),
            "ptc:internal" => self.glue.internal.to_owned(),
            "ptc:std" => self.glue.std.to_owned(),
            "ptc:tools" => self.tools.clone(),
            SCRIPT_SPECIFIER => self.script.clone(),
            other => {
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
        (file_name == SCRIPT_SPECIFIER).then_some(Cow::Borrowed(self.source_map.as_slice()))
    }
}
