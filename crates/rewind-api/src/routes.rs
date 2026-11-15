//! The paths of the API, as OpenAPI and axum write them (`{param}`), and how to fill them.

use reqwest::Url;

use crate::Error;

pub const TOOLSETS: &str = "/v1/toolsets";
pub const TOOLSET_DECLARATIONS: &str = "/v1/toolsets/{id}/declarations";
pub const TOOLSET_SEARCH: &str = "/v1/toolsets/{id}/search";
pub const TOOLSET_CREDENTIALS: &str = "/v1/toolsets/{id}/credentials";

pub const WORKFLOWS: &str = "/v1/workflows";
pub const WORKFLOW: &str = "/v1/workflows/{name}";

pub const CHECK: &str = "/v1/check";
pub const TEST: &str = "/v1/test";
pub const SCRIPTS: &str = "/v1/scripts";
pub const SCRIPT: &str = "/v1/scripts/{digest}";

pub const RUNS: &str = "/v1/runs";
pub const RUN_STATS: &str = "/v1/runs/stats";
pub const RUN_FAILURES: &str = "/v1/runs/failures";
pub const RUN: &str = "/v1/runs/{id}";
pub const RUN_TRACE: &str = "/v1/runs/{id}/trace";
pub const RUN_ENTRY: &str = "/v1/runs/{id}/entries/{idx}";
pub const RUN_EVENTS: &str = "/v1/runs/{id}/events";
pub const RUN_SIGNAL: &str = "/v1/runs/{id}/signals/{name}";
pub const RUN_CANCEL: &str = "/v1/runs/{id}/cancel";
pub const RUN_FORK: &str = "/v1/runs/{id}/fork";
pub const RUN_JOURNAL: &str = "/v1/runs/{id}/journal";
pub const RUN_RELEASE: &str = "/v1/runs/{id}/release";
pub const RUN_GIVE_UP: &str = "/v1/runs/{id}/give-up";
pub const RUN_CREDENTIALS: &str = "/v1/runs/{id}/credentials";

pub const TASKS: &str = "/v1/tasks";
pub const TASK_HEARTBEAT: &str = "/v1/tasks/{token}/heartbeat";
pub const TASK_DEFER: &str = "/v1/tasks/{token}/defer";
pub const TASK_COMPLETE: &str = "/v1/tasks/{token}/complete";
pub const TASK_FAIL: &str = "/v1/tasks/{token}/fail";
pub const TASK_APPROVE: &str = "/v1/tasks/{id}/approve";
pub const TASK_REJECT: &str = "/v1/tasks/{id}/reject";

pub const HOOK: &str = "/hooks/{token}";
pub const SUBSCRIPTIONS: &str = "/v1/subscriptions";
pub const SUBSCRIPTION: &str = "/v1/subscriptions/{id}";

pub const TOOL_STATS: &str = "/v1/tools/stats";
pub const PAYLOAD: &str = "/v1/payloads/{digest}";

pub const API_KEYS: &str = "/v1/api-keys";
pub const API_KEY: &str = "/v1/api-keys/{id}";
pub const WHOAMI: &str = "/v1/whoami";
pub const SERVER: &str = "/v1/server";
pub const SERVER_STOP: &str = "/v1/server/stop";

/// Every operation of the API: its method and its path.
pub const OPERATIONS: [(&str, &str); 46] = [
    ("POST", TOOLSETS),
    ("GET", TOOLSETS),
    ("GET", TOOLSET_DECLARATIONS),
    ("GET", TOOLSET_SEARCH),
    ("PUT", TOOLSET_CREDENTIALS),
    ("POST", WORKFLOWS),
    ("GET", WORKFLOWS),
    ("GET", WORKFLOW),
    ("POST", CHECK),
    ("POST", TEST),
    ("GET", SCRIPTS),
    ("GET", SCRIPT),
    ("POST", RUNS),
    ("GET", RUNS),
    ("GET", RUN_STATS),
    ("GET", RUN_FAILURES),
    ("GET", RUN),
    ("DELETE", RUN),
    ("GET", RUN_TRACE),
    ("GET", RUN_ENTRY),
    ("GET", RUN_EVENTS),
    ("POST", RUN_SIGNAL),
    ("POST", RUN_CANCEL),
    ("POST", RUN_FORK),
    ("GET", RUN_JOURNAL),
    ("POST", RUN_RELEASE),
    ("POST", RUN_GIVE_UP),
    ("PUT", RUN_CREDENTIALS),
    ("GET", TASKS),
    ("POST", TASK_HEARTBEAT),
    ("POST", TASK_DEFER),
    ("POST", TASK_COMPLETE),
    ("POST", TASK_FAIL),
    ("POST", TASK_APPROVE),
    ("POST", TASK_REJECT),
    ("POST", HOOK),
    ("POST", SUBSCRIPTIONS),
    ("GET", SUBSCRIPTIONS),
    ("DELETE", SUBSCRIPTION),
    ("GET", TOOL_STATS),
    ("GET", PAYLOAD),
    ("POST", API_KEYS),
    ("DELETE", API_KEY),
    ("GET", WHOAMI),
    ("GET", SERVER),
    ("POST", SERVER_STOP),
];

/// Fills a path's parameters in order, each percent-encoded as one segment:
/// `fill(RUN_SIGNAL, &["r1", "a/b"])` is `/v1/runs/r1/signals/a%2Fb`.
pub fn fill(template: &str, params: &[&str]) -> Result<String, Error> {
    let mut url =
        Url::parse("http://rewind.invalid").map_err(|error| Error::Config(error.to_string()))?;
    push(&mut url, template, params)?;
    Ok(url.path().to_owned())
}

/// Appends a filled path to a base URL, keeping the base's own path as a prefix.
pub(crate) fn push(url: &mut Url, template: &str, params: &[&str]) -> Result<(), Error> {
    let mut params = params.iter();
    let base = url.to_string();
    let mut segments = url
        .path_segments_mut()
        .map_err(|()| Error::Config(format!("{base} cannot be a base URL")))?;
    segments.pop_if_empty();
    for segment in template.trim_start_matches('/').split('/') {
        if segment.starts_with('{') && segment.ends_with('}') {
            let value = params.next().ok_or_else(|| {
                Error::invalid("path", format!("{template}: missing a value for {segment}"))
            })?;
            if value.is_empty() {
                return Err(Error::invalid(
                    "path",
                    format!("{template}: {segment} is empty"),
                ));
            }
            segments.push(value);
        } else {
            segments.push(segment);
        }
    }
    drop(segments);
    if params.next().is_some() {
        return Err(Error::invalid(
            "path",
            format!("{template}: too many values"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameters_are_encoded_as_one_segment() {
        assert_eq!(
            fill(RUN_SIGNAL, &["r1", "pr merged/#1"]).unwrap(),
            "/v1/runs/r1/signals/pr%20merged%2F%231"
        );
        assert_eq!(fill(RUNS, &[]).unwrap(), "/v1/runs");
        assert!(fill(RUN, &[]).is_err());
        assert!(fill(RUN, &["a", "b"]).is_err());
        assert!(fill(RUN, &[""]).is_err());
    }

    #[test]
    fn a_base_path_is_kept_as_a_prefix() {
        let mut url = Url::parse("https://example.com/rewind/").unwrap();
        push(&mut url, RUN_ENTRY, &["r1", "7"]).unwrap();
        assert_eq!(
            url.as_str(),
            "https://example.com/rewind/v1/runs/r1/entries/7"
        );
        let mut url = Url::parse("http://localhost").unwrap();
        push(&mut url, HOOK, &["tok"]).unwrap();
        assert_eq!(url.as_str(), "http://localhost/hooks/tok");
    }

    #[test]
    fn every_operation_is_listed_once() {
        let mut all: Vec<_> = OPERATIONS.iter().collect();
        all.sort_unstable();
        let count = all.len();
        all.dedup();
        assert_eq!(all.len(), count);
    }
}
