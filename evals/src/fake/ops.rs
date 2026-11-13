//! `ops`: deployments that take time, CI builds that call a webhook back, audit events and
//! metrics. Deployments and builds move with the episode's virtual time.

use std::sync::Mutex;

use rewind_core::Timestamp;
use serde::Serialize;
use serde_json::{Map, Value, json};

use super::{
    Args, CallContext, Fixed, Outbound, Outbox, Reply, Service, ToolSpec, lock, object_schema, page,
};
use crate::clock::{DAY, MINUTE, parse_date, rfc3339};

/// A deployment is `pending` for 5 minutes, then `running`, and has `succeeded` 50 minutes
/// after it started.
pub const DEPLOYMENT_PENDING: u64 = 5 * MINUTE;
pub const DEPLOYMENT_DURATION: u64 = 50 * MINUTE;
/// A build ends 20 minutes after it was triggered.
pub const BUILD_DURATION: u64 = 20 * MINUTE;
/// The commit whose build passes; every other commit fails.
pub const GREEN_COMMIT: &str = "3f2a9c1";
/// Services and their metrics.
pub const SERVICES: [&str; 4] = ["api", "web", "worker", "billing"];

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Deployment {
    pub deployment_id: String,
    pub service: String,
    pub version: String,
    pub started_at: String,
    #[serde(skip)]
    pub started: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Build {
    pub build_id: String,
    pub repo: String,
    pub commit: String,
    pub callback_url: Option<String>,
    pub triggered_at: String,
    #[serde(skip)]
    pub triggered: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AuditEvent {
    pub id: String,
    pub at: String,
    pub actor: String,
    pub action: String,
    pub target: String,
    pub ip: String,
}

/// 2,400 audit events over February 2026, oldest first.
pub fn audit_events() -> Vec<AuditEvent> {
    let mut fixed = Fixed::new(41);
    let february = parse_date("2026-02-01").unwrap_or(Timestamp(0));
    let step = 28 * DAY / 2_400;
    (0..2_400u64)
        .map(|n| AuditEvent {
            id: format!("evt_{:05}", n + 1),
            at: rfc3339(Timestamp(february.0 + n * step + fixed.below(step))),
            actor: fixed
                .pick(&["dana", "eli", "farah", "gus", "hana", "deploy-bot"])
                .to_string(),
            action: fixed
                .pick(&[
                    "login",
                    "deploy",
                    "update_role",
                    "export",
                    "delete",
                    "create_key",
                ])
                .to_string(),
            target: format!("res_{}", fixed.below(500)),
            ip: format!(
                "10.{}.{}.{}",
                fixed.below(4),
                fixed.below(256),
                fixed.below(256)
            ),
        })
        .collect()
}

/// The p95 latency (ms), error rate (%) and requests per minute of each service.
pub fn metric(service: &str, metric: &str) -> Option<(f64, &'static str)> {
    let index = SERVICES.iter().position(|known| *known == service)?;
    let index = index as f64;
    match metric {
        "latency_p95" => Some((120.0 + 37.5 * index, "ms")),
        "error_rate" => Some((0.2 + 0.15 * index, "%")),
        "requests" => Some((1_800.0 - 300.0 * index, "rpm")),
        _ => None,
    }
}

#[derive(Debug, Default)]
struct State {
    deployments: Vec<Deployment>,
    builds: Vec<Build>,
}

#[derive(Debug)]
pub struct Ops {
    audit: Vec<AuditEvent>,
    state: Mutex<State>,
    outbox: Outbox,
}

impl Default for Ops {
    fn default() -> Self {
        Self::new()
    }
}

impl Ops {
    pub fn new() -> Self {
        Self {
            audit: audit_events(),
            state: Mutex::new(State::default()),
            outbox: Outbox::default(),
        }
    }
}

fn deployment_view(deployment: &Deployment, now: Timestamp) -> Value {
    let elapsed = now.0.saturating_sub(deployment.started.0);
    let (status, progress) = if elapsed >= DEPLOYMENT_DURATION {
        ("succeeded", 100)
    } else if elapsed >= DEPLOYMENT_PENDING {
        ("running", 100 * elapsed / DEPLOYMENT_DURATION)
    } else {
        ("pending", 0)
    };
    let mut view = json!({
        "deployment_id": deployment.deployment_id,
        "service": deployment.service,
        "version": deployment.version,
        "status": status,
        "progress_percent": progress,
        "started_at": deployment.started_at,
    });
    if status == "succeeded" {
        view["finished_at"] = json!(rfc3339(Timestamp(
            deployment.started.0 + DEPLOYMENT_DURATION
        )));
    }
    view
}

fn build_status(build: &Build) -> &'static str {
    if build.commit.starts_with(GREEN_COMMIT) || GREEN_COMMIT.starts_with(&build.commit) {
        "passed"
    } else {
        "failed"
    }
}

fn build_view(build: &Build, now: Timestamp) -> Value {
    let done = now.0 >= build.triggered.0 + BUILD_DURATION;
    json!({
        "build_id": build.build_id,
        "repo": build.repo,
        "commit": build.commit,
        "status": if done { build_status(build) } else { "running" },
        "triggered_at": build.triggered_at,
    })
}

impl Service for Ops {
    fn name(&self) -> &'static str {
        "ops"
    }

    fn tools(&self) -> Vec<ToolSpec> {
        let service = json!({ "type": "string", "enum": SERVICES });
        vec![
            ToolSpec::new(
                "start_deployment",
                "Start deploying a version of a service. Deployments take up to an hour: follow them with get_deployment.",
                object_schema(json!({ "service": service, "version": { "type": "string" } }), &["service", "version"]),
            ),
            ToolSpec::new(
                "get_deployment",
                "The state of a deployment: status is pending, running or succeeded, with progress_percent.",
                object_schema(json!({ "deployment_id": { "type": "string" } }), &["deployment_id"]),
            ),
            ToolSpec::new(
                "trigger_build",
                "Start a CI build of a commit. Builds take about 20 minutes. With `callback_url`, the CI POSTs { build_id, repo, commit, status } to that URL when the build ends (status passed or failed).",
                object_schema(
                    json!({
                        "repo": { "type": "string" },
                        "commit": { "type": "string" },
                        "callback_url": { "type": "string", "format": "uri" }
                    }),
                    &["repo", "commit"],
                ),
            ),
            ToolSpec::new(
                "get_build",
                "The state of a build: status is running, passed or failed.",
                object_schema(json!({ "build_id": { "type": "string" } }), &["build_id"]),
            ),
            ToolSpec::new(
                "list_audit_events",
                "The audit events of a month (YYYY-MM), oldest first, up to `limit` per page (100 by default, 500 at most). Returns { events, next_cursor? }.",
                object_schema(
                    json!({
                        "month": { "type": "string", "pattern": "^[0-9]{4}-[0-9]{2}$" },
                        "cursor": { "type": "string" },
                        "limit": { "type": "integer", "minimum": 1, "maximum": 500 }
                    }),
                    &["month"],
                ),
            ),
            ToolSpec::new(
                "get_metric",
                "The current value of a service's metric.",
                object_schema(
                    json!({ "service": service, "metric": { "type": "string", "enum": ["latency_p95", "error_rate", "requests"] } }),
                    &["service", "metric"],
                ),
            )
            .output(object_schema(
                json!({
                    "service": { "type": "string" },
                    "metric": { "type": "string" },
                    "value": { "type": "number" },
                    "unit": { "type": "string", "enum": ["ms", "%", "rpm"] }
                }),
                &["service", "metric", "value", "unit"],
            )),
        ]
    }

    fn call(&self, context: &CallContext, tool: &str, args: &Map<String, Value>) -> Reply {
        let args = Args(args);
        let now = context.now;
        let mut state = lock(&self.state);
        match tool {
            "start_deployment" => {
                let (service, version) = match (args.str("service"), args.str("version")) {
                    (Ok(service), Ok(version)) => (service.to_owned(), version.to_owned()),
                    (Err(reply), _) | (_, Err(reply)) => return reply,
                };
                let deployment = Deployment {
                    deployment_id: format!("dep_{}", state.deployments.len() + 1),
                    service,
                    version,
                    started_at: rfc3339(now),
                    started: now,
                };
                let view = deployment_view(&deployment, now);
                state.deployments.push(deployment);
                Reply::Json(view)
            }
            "get_deployment" => {
                let id = match args.str("deployment_id") {
                    Ok(id) => id,
                    Err(reply) => return reply,
                };
                match state.deployments.iter().find(|d| d.deployment_id == id) {
                    Some(deployment) => Reply::Json(deployment_view(deployment, now)),
                    None => Reply::error(format!("not_found: no deployment {id}")),
                }
            }
            "trigger_build" => {
                let (repo, commit) = match (args.str("repo"), args.str("commit")) {
                    (Ok(repo), Ok(commit)) => (repo.to_owned(), commit.to_owned()),
                    (Err(reply), _) | (_, Err(reply)) => return reply,
                };
                let build = Build {
                    build_id: format!("build_{}", state.builds.len() + 1),
                    repo,
                    commit,
                    callback_url: args.opt_str("callback_url").map(str::to_owned),
                    triggered_at: rfc3339(now),
                    triggered: now,
                };
                if let Some(url) = &build.callback_url {
                    self.outbox.push(Outbound {
                        id: format!("delivery_{}", build.build_id),
                        url: url.clone(),
                        payload: json!({
                            "build_id": build.build_id,
                            "repo": build.repo,
                            "commit": build.commit,
                            "status": build_status(&build),
                        }),
                        at: Timestamp(now.0 + BUILD_DURATION),
                    });
                }
                let view = build_view(&build, now);
                state.builds.push(build);
                Reply::Json(view)
            }
            "get_build" => {
                let id = match args.str("build_id") {
                    Ok(id) => id,
                    Err(reply) => return reply,
                };
                match state.builds.iter().find(|build| build.build_id == id) {
                    Some(build) => Reply::Json(build_view(build, now)),
                    None => Reply::error(format!("not_found: no build {id}")),
                }
            }
            "list_audit_events" => {
                let month = match args.str("month") {
                    Ok(month) => month,
                    Err(reply) => return reply,
                };
                let selected: Vec<&AuditEvent> = self
                    .audit
                    .iter()
                    .filter(|event| event.at.starts_with(month))
                    .collect();
                let limit = args.opt_u64("limit").unwrap_or(100).clamp(1, 500) as usize;
                match page(&selected, args.opt_str("cursor"), limit) {
                    Ok((events, next)) => {
                        let mut result = json!({ "events": events });
                        if let Some(next) = next {
                            result["next_cursor"] = json!(next);
                        }
                        Reply::Json(result)
                    }
                    Err(reply) => reply,
                }
            }
            "get_metric" => {
                let (service, name) = match (args.str("service"), args.str("metric")) {
                    (Ok(service), Ok(name)) => (service, name),
                    (Err(reply), _) | (_, Err(reply)) => return reply,
                };
                match metric(service, name) {
                    Some((value, unit)) => Reply::Structured(json!({
                        "service": service,
                        "metric": name,
                        "value": value,
                        "unit": unit,
                    })),
                    None => Reply::error(format!("not_found: no metric {name} for {service}")),
                }
            }
            other => Reply::error(format!("unknown tool {other}")),
        }
    }

    fn snapshot(&self) -> Value {
        let state = lock(&self.state);
        let deployments: Vec<Value> = state
            .deployments
            .iter()
            .map(|deployment| {
                json!({
                    "deployment_id": deployment.deployment_id,
                    "service": deployment.service,
                    "version": deployment.version,
                    "started_at": deployment.started_at,
                    "finished_at": rfc3339(Timestamp(deployment.started.0 + DEPLOYMENT_DURATION)),
                })
            })
            .collect();
        let builds: Vec<Value> = state
            .builds
            .iter()
            .map(|build| {
                json!({
                    "build_id": build.build_id,
                    "repo": build.repo,
                    "commit": build.commit,
                    "callback_url": build.callback_url,
                    "triggered_at": build.triggered_at,
                    "finished_at": rfc3339(Timestamp(build.triggered.0 + BUILD_DURATION)),
                    "status": build_status(build),
                })
            })
            .collect();
        json!({ "deployments": deployments, "builds": builds })
    }

    fn outbox(&self) -> Option<&Outbox> {
        Some(&self.outbox)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::T0;

    fn call(ops: &Ops, at: u64, tool: &str, args: Value) -> Reply {
        let context = CallContext {
            now: Timestamp(T0.0 + at),
            idempotency_key: None,
        };
        ops.call(&context, tool, args.as_object().unwrap())
    }

    #[test]
    fn a_deployment_succeeds_with_time() {
        let ops = Ops::new();
        call(
            &ops,
            0,
            "start_deployment",
            json!({ "service": "api", "version": "2.4.1" }),
        );
        let status = |at| match call(
            &ops,
            at,
            "get_deployment",
            json!({ "deployment_id": "dep_1" }),
        ) {
            Reply::Json(view) => view["status"].as_str().unwrap().to_owned(),
            other => panic!("{other:?}"),
        };
        assert_eq!(status(0), "pending");
        assert_eq!(status(10 * MINUTE), "running");
        assert_eq!(status(DEPLOYMENT_DURATION), "succeeded");
    }

    #[test]
    fn a_build_calls_its_callback_when_it_ends() {
        let ops = Ops::new();
        call(
            &ops,
            0,
            "trigger_build",
            json!({ "repo": "acme/api", "commit": GREEN_COMMIT, "callback_url": "https://rewind.example/hooks/t" }),
        );
        let outbox = ops.outbox().unwrap();
        assert_eq!(outbox.next(), Some(Timestamp(T0.0 + BUILD_DURATION)));
        let due = outbox.take_due(Timestamp(T0.0 + BUILD_DURATION));
        assert_eq!(due[0].payload["status"], "passed");
    }

    #[test]
    fn audit_events_fill_february() {
        let events = audit_events();
        assert_eq!(events.len(), 2_400);
        assert!(events.iter().all(|event| event.at.starts_with("2026-02")));
        assert!(events.windows(2).all(|pair| pair[0].at <= pair[1].at));
    }
}
