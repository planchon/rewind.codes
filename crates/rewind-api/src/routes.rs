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

pub const WHOAMI: &str = "/v1/whoami";
pub const ORGANIZATIONS: &str = "/v1/organizations";
pub const ORGANIZATION: &str = "/v1/organization";
pub const INVITATIONS: &str = "/v1/invitations";
pub const INVITATION_ACCEPT: &str = "/v1/invitations/{id}/accept";
pub const MEMBERS: &str = "/v1/organization/members";
pub const MEMBER: &str = "/v1/organization/members/{user}";
pub const MEMBER_GRANT: &str = "/v1/organization/members/{user}/grant";
pub const ORGANIZATION_INVITATIONS: &str = "/v1/organization/invitations";
pub const ORGANIZATION_INVITATION: &str = "/v1/organization/invitations/{id}";
pub const ORGANIZATION_INVITATION_RESEND: &str = "/v1/organization/invitations/{id}/resend";

pub const PERMISSIONS: &str = "/v1/permissions";
pub const ROLES: &str = "/v1/roles";
pub const ROLE: &str = "/v1/roles/{name}";
pub const API_KEYS: &str = "/v1/api-keys";
pub const API_KEY: &str = "/v1/api-keys/{id}";

pub const NAMESPACES: &str = "/v1/namespaces";
pub const NAMESPACE: &str = "/v1/namespaces/{name}";
pub const NAMESPACE_CREDENTIALS: &str = "/v1/namespaces/{name}/toolsets/{id}/credentials";
pub const NAMESPACE_TOKENS: &str = "/v1/namespaces/{name}/tokens";
pub const TOKEN: &str = "/v1/tokens/{id}";
pub const NAMESPACE_DEFAULTS: &str = "/v1/organization/namespace-defaults";
pub const QUOTA_ALERTS: &str = "/v1/organization/quota-alerts";
pub const TOKEN_KEYS: &str = "/v1/organization/token-keys";
pub const TOKEN_KEY: &str = "/v1/organization/token-keys/{kid}";
pub const ORGANIZATION_QUOTAS: &str = "/v1/organizations/{tenant}/quotas";

pub const SERVER: &str = "/v1/server";
pub const SERVER_STOP: &str = "/v1/server/stop";

/// The dashboard's sign-in (`crate::auth`), outside `/v1`.
pub const AUTH_CONFIG: &str = "/auth/config";
pub const AUTH_GITHUB: &str = "/auth/github";
pub const AUTH_CALLBACK: &str = "/auth/callback";
pub const AUTH_MAGIC_AUTH: &str = "/auth/magic-auth";
pub const AUTH_MAGIC_AUTH_VERIFY: &str = "/auth/magic-auth/verify";
pub const AUTH_EMAIL_VERIFICATION: &str = "/auth/email-verification";
pub const AUTH_ORGANIZATION: &str = "/auth/organization";
pub const AUTH_SESSION: &str = "/auth/session";
pub const AUTH_WAITLIST: &str = "/auth/waitlist";
pub const AUTH_LOGOUT: &str = "/auth/logout";

/// Every operation of the API: its method and its path.
pub const OPERATIONS: [(&str, &str); 87] = [
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
    ("GET", WHOAMI),
    ("POST", ORGANIZATIONS),
    ("PATCH", ORGANIZATION),
    ("GET", INVITATIONS),
    ("POST", INVITATION_ACCEPT),
    ("GET", MEMBERS),
    ("PUT", MEMBER_GRANT),
    ("DELETE", MEMBER),
    ("GET", ORGANIZATION_INVITATIONS),
    ("POST", ORGANIZATION_INVITATIONS),
    ("POST", ORGANIZATION_INVITATION_RESEND),
    ("DELETE", ORGANIZATION_INVITATION),
    ("GET", PERMISSIONS),
    ("GET", ROLES),
    ("PUT", ROLE),
    ("DELETE", ROLE),
    ("GET", API_KEYS),
    ("POST", API_KEYS),
    ("DELETE", API_KEY),
    ("GET", NAMESPACES),
    ("GET", NAMESPACE),
    ("PUT", NAMESPACE),
    ("DELETE", NAMESPACE),
    ("PUT", NAMESPACE_CREDENTIALS),
    ("GET", NAMESPACE_DEFAULTS),
    ("PUT", NAMESPACE_DEFAULTS),
    ("GET", QUOTA_ALERTS),
    ("POST", NAMESPACE_TOKENS),
    ("DELETE", NAMESPACE_TOKENS),
    ("DELETE", TOKEN),
    ("GET", TOKEN_KEYS),
    ("POST", TOKEN_KEYS),
    ("DELETE", TOKEN_KEY),
    ("PUT", ORGANIZATION_QUOTAS),
    ("GET", SERVER),
    ("POST", SERVER_STOP),
    ("GET", AUTH_CONFIG),
    ("GET", AUTH_GITHUB),
    ("GET", AUTH_CALLBACK),
    ("POST", AUTH_MAGIC_AUTH),
    ("POST", AUTH_MAGIC_AUTH_VERIFY),
    ("POST", AUTH_EMAIL_VERIFICATION),
    ("POST", AUTH_ORGANIZATION),
    ("POST", AUTH_SESSION),
    ("POST", AUTH_WAITLIST),
    ("POST", AUTH_LOGOUT),
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
