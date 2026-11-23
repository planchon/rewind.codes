//! Scripts' titles (`archi/04-tools.md`, "Script titles"): the title and description a person
//! gave a script, for the organization, else those of the comment at the top of its source.
//!
//! A source never changes, so what its comment says is read once per tenant and digest and kept;
//! the titles people give are read from the store, once per page for the scripts it shows.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use rewind_api::scripts::TitleSource;
use rewind_core::{Digest, ScriptDoc, script_doc};
use rewind_engine::ports::{JournalStore, ScriptTitle, StoreError, TenantId};

use crate::wiring::store::Store;

/// The scripts whose text a server keeps before it forgets them all.
const KEPT: usize = 10_000;

/// A script's title as the API shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Title {
    pub(crate) title: String,
    pub(crate) description: Option<String>,
    pub(crate) source: TitleSource,
}

/// The title a script shows: the one a person gave it, with their description, else its
/// comment's.
pub(crate) fn shown(given: Option<&ScriptTitle>, doc: Option<&ScriptDoc>) -> Option<Title> {
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

/// What a script's source says of it: its first line that is not blank, and its comment.
#[derive(Debug)]
pub(crate) struct ScriptText {
    pub(crate) headline: String,
    pub(crate) doc: Option<ScriptDoc>,
}

impl ScriptText {
    pub(crate) fn of(source: &str) -> Self {
        Self {
            headline: rewind_api::scripts::headline(source),
            doc: script_doc(source),
        }
    }
}

/// The text of the scripts read so far, by tenant and digest.
#[derive(Debug, Default)]
pub(crate) struct ScriptTexts {
    kept: Mutex<HashMap<(TenantId, Digest), Arc<ScriptText>>>,
}

impl ScriptTexts {
    /// What the script's source says of it; `None` when the tenant has no such script, or the
    /// store cannot be read (nothing is kept then).
    pub(crate) async fn text(
        &self,
        store: &Store,
        tenant: &TenantId,
        digest: Digest,
    ) -> Option<Arc<ScriptText>> {
        let key = (tenant.clone(), digest);
        if let Some(text) = self
            .kept
            .lock()
            .ok()
            .and_then(|kept| kept.get(&key).cloned())
        {
            return Some(text);
        }
        let text = match store.get_script(tenant, digest).await {
            Ok(Some(stored)) => Arc::new(ScriptText::of(stored.script.source())),
            Ok(None) => return None,
            Err(error) => {
                tracing::warn!(%digest, %error, "the script's text was not read");
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

    /// The titles of these scripts of the tenant, for those that have one: one read of the
    /// titles people gave, the comments of the sources kept.
    pub(crate) async fn titles(
        &self,
        store: &Store,
        tenant: &TenantId,
        scripts: impl IntoIterator<Item = Digest>,
    ) -> Result<BTreeMap<Digest, Title>, StoreError> {
        let scripts: Vec<Digest> = scripts
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if scripts.is_empty() {
            return Ok(BTreeMap::new());
        }
        let given: BTreeMap<Digest, ScriptTitle> = store
            .script_titles(tenant, &scripts)
            .await?
            .into_iter()
            .collect();
        let mut titles = BTreeMap::new();
        for script in scripts {
            let title = match given.get(&script) {
                Some(given) => shown(Some(given), None),
                None => self
                    .text(store, tenant, script)
                    .await
                    .and_then(|text| shown(None, text.doc.as_ref())),
            };
            if let Some(title) = title {
                titles.insert(script, title);
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
        let doc =
            script_doc("/** Daily digest\n *\n * Posts the day's issues. */\nexport default 1")
                .expect("a doc");
        let given = ScriptTitle {
            title: "Morning digest".into(),
            description: None,
            updated_by: "user:user_1".into(),
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
