//! Workflows' titles (`archi/04-tools.md`, "Script titles"): the title and description a person
//! gave a code, for the organization, else those of the comment at the top of its source.
//!
//! A source never changes, so what its comment says is read once per tenant and digest and kept;
//! the titles people give are read from the catalog, once per page for the workflows it shows.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use rewind_api::scripts::TitleSource;
use rewind_core::{CodeDoc, Digest, code_doc};
use rewind_engine::ports::{CatalogError, CatalogStore, ScriptTitle, TenantId};

/// The workflows whose text a server keeps before it forgets them all.
const KEPT: usize = 10_000;

/// A code's title as the API shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Title {
    pub(crate) title: String,
    pub(crate) description: Option<String>,
    pub(crate) source: TitleSource,
}

/// The title a code shows: the one a person gave it, with their description, else its
/// comment's.
pub(crate) fn shown(given: Option<&ScriptTitle>, doc: Option<&CodeDoc>) -> Option<Title> {
    match (given, doc) {
        (Some(given), _) => Some(Title {
            title: given.title.clone(),
            description: given.description.clone(),
            source: TitleSource::Person,
        }),
        (None, Some(doc)) => Some(Title {
            title: doc.title.clone(),
            description: doc.description.clone(),
            source: TitleSource::Comment,
        }),
        (None, None) => None,
    }
}

/// What a code's source says of it: its first line that is not blank, and its comment.
#[derive(Debug)]
pub(crate) struct CodeText {
    pub(crate) headline: String,
    pub(crate) doc: Option<CodeDoc>,
}

impl CodeText {
    pub(crate) fn of(source: &str) -> Self {
        Self {
            headline: rewind_api::scripts::headline(source),
            doc: code_doc(source),
        }
    }
}

/// The text of the workflows read so far, by tenant and digest.
#[derive(Debug, Default)]
pub(crate) struct CodeTexts {
    kept: Mutex<HashMap<(TenantId, Digest), Arc<CodeText>>>,
}

impl CodeTexts {
    /// What the code's source says of it; `None` when the tenant has no such code, or the
    /// store cannot be read (nothing is kept then).
    pub(crate) async fn text<C: CatalogStore>(
        &self,
        catalog: &C,
        tenant: &TenantId,
        digest: Digest,
    ) -> Option<Arc<CodeText>> {
        let key = (tenant.clone(), digest);
        if let Some(text) = self
            .kept
            .lock()
            .ok()
            .and_then(|kept| kept.get(&key).cloned())
        {
            return Some(text);
        }
        let text = match catalog.get_code(tenant, digest).await {
            Ok(Some(stored)) => Arc::new(CodeText::of(stored.code.source())),
            Ok(None) => return None,
            Err(error) => {
                tracing::warn!(%digest, %error, "the code's text was not read");
                return None;
            }
        };
        if let Ok(mut kept) = self.kept.lock() {
            if kept.len() > KEPT {
                kept.clear();
            }
            kept.insert(key, text.clone());
        }
        Some(text)
    }

    /// The titles of these workflows of the tenant, for those that have one: one read of the
    /// titles people gave, the comments of the sources kept.
    pub(crate) async fn titles<C: CatalogStore>(
        &self,
        catalog: &C,
        tenant: &TenantId,
        scripts: impl IntoIterator<Item = Digest>,
    ) -> Result<BTreeMap<Digest, Title>, CatalogError> {
        let scripts: Vec<Digest> = scripts
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if scripts.is_empty() {
            return Ok(BTreeMap::new());
        }
        let given: BTreeMap<Digest, ScriptTitle> = catalog
            .script_titles(tenant, &scripts)
            .await?
            .into_iter()
            .collect();
        let mut titles = BTreeMap::new();
        for code in scripts {
            let title = match given.get(&code) {
                Some(given) => shown(Some(given), None),
                None => self
                    .text(catalog, tenant, code)
                    .await
                    .and_then(|text| shown(None, text.doc.as_ref())),
            };
            if let Some(title) = title {
                titles.insert(code, title);
            }
        }
        Ok(titles)
    }
}

#[cfg(test)]
mod tests {
    use rewind_core::Timestamp;

    use super::*;

    #[test]
    fn a_title_a_person_gave_comes_before_the_comment() {
        let doc = code_doc("/** Daily digest\n *\n * Posts the day's issues. */\nexport default 1")
            .expect("a doc");
        let given = ScriptTitle {
            title: "Morning digest".into(),
            description: None,
            updated_by: "member:user_1".into(),
            updated_at: Timestamp(0),
        };
        assert_eq!(
            shown(Some(&given), Some(&doc)),
            Some(Title {
                title: "Morning digest".into(),
                description: None,
                source: TitleSource::Person,
            })
        );
        assert_eq!(
            shown(None, Some(&doc)),
            Some(Title {
                title: "Daily digest".into(),
                description: Some("Posts the day's issues.".into()),
                source: TitleSource::Comment,
            })
        );
        assert_eq!(shown(None, None), None);
    }
}
