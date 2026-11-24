//! Text the stores keep never holds a NUL character (U+0000). Postgres refuses it in `text` and
//! `jsonb` columns where Turso and memory keep it: the engine refuses it at its entry points, so
//! that every store behaves alike and a request carrying it is told why.
//!
//! Payload values are not concerned: they are stored as JSON, where U+0000 is escaped. Tenants,
//! action names and server names already refuse control characters. A diagnostic the engine
//! writes, such as a quarantine reason, has its NUL characters replaced instead.

use std::collections::BTreeMap;

use crate::ports::TraceContext;

/// What the declarations generated from an action set are made of, as errors name them.
pub const ACTION_SET_TEXT: &str = "the action set's descriptions or schemas";

/// A text to store holds a NUL character. The name says which text, for the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{0} contains a NUL character (U+0000), which cannot be stored: remove it")]
pub struct NulCharacter(pub &'static str);

/// Refuses `text` if it holds a NUL character; `what` names it in the error.
pub fn refuse_nul(what: &'static str, text: &str) -> Result<(), NulCharacter> {
    if text.contains('\0') {
        Err(NulCharacter(what))
    } else {
        Ok(())
    }
}

/// Refuses labels with a NUL character in a key or a value.
pub fn refuse_nul_in_labels(labels: &BTreeMap<String, String>) -> Result<(), NulCharacter> {
    labels.iter().try_for_each(|(key, value)| {
        refuse_nul("a label", key)?;
        refuse_nul("a label", value)
    })
}

/// Refuses a trace context with a NUL character.
pub fn refuse_nul_in_trace(trace: &TraceContext) -> Result<(), NulCharacter> {
    refuse_nul("the trace context", &trace.traceparent)?;
    trace
        .tracestate
        .as_deref()
        .map_or(Ok(()), |state| refuse_nul("the trace context", state))
}

/// A diagnostic the engine writes to the store (a quarantine reason), its NUL characters
/// replaced by U+FFFD: it may quote what a code chose, such as a signal's name.
pub(crate) fn storable(text: String) -> String {
    if text.contains('\0') {
        text.replace('\0', "\u{FFFD}")
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_text_with_a_nul_character_is_refused() {
        assert_eq!(refuse_nul("the key", "a\u{1}b é"), Ok(()));
        assert_eq!(refuse_nul("the key", "a\0b"), Err(NulCharacter("the key")));
        assert_eq!(
            NulCharacter("the key").to_string(),
            "the key contains a NUL character (U+0000), which cannot be stored: remove it"
        );
        let labels = |key: &str, value: &str| BTreeMap::from([(key.into(), value.into())]);
        assert_eq!(refuse_nul_in_labels(&labels("agent", "triage")), Ok(()));
        assert!(refuse_nul_in_labels(&labels("agent\0", "triage")).is_err());
        assert!(refuse_nul_in_labels(&labels("agent", "\0")).is_err());
        let trace = |state: Option<&str>| TraceContext {
            traceparent: "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".into(),
            tracestate: state.map(str::to_owned),
        };
        assert_eq!(refuse_nul_in_trace(&trace(None)), Ok(()));
        assert!(refuse_nul_in_trace(&trace(Some("a=\0"))).is_err());
    }

    #[test]
    fn diagnostics_are_stored_without_nul_characters() {
        assert_eq!(storable("signal \"a\0b\"".into()), "signal \"a\u{FFFD}b\"");
        assert_eq!(storable("plain".into()), "plain");
    }
}
