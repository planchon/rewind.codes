//! The eval suite of Rewind (`archi/05-testing-and-observability.md`, "Evals").
//!
//! Models solve realistic tasks end to end: they get a system prompt with the toolset's
//! declarations in a prompt rendering, and the agent tools of the SDK (`run_script`,
//! `check_script`, `get_run`…); their scripts run on the real engine and V8 sandbox, in
//! virtual time, against deterministic fake MCP servers called through the real MCP client.
//! Each task's checker looks at the world at the end. Reports give, per model and rendering,
//! the first-try check pass rate, task success, iterations and tokens.
//!
//! - [`fake`]: the fake MCP servers and their fixtures.
//! - [`world`]: the engine wired in process, and the outside world in virtual time.
//! - [`agent`]: the agent tools, as the SDK handles them, rendering the same text.
//! - [`model`]: Claude (Anthropic API), an OpenAI model (Responses API), a scripted model.
//! - [`tasks`]: the tasks; [`task`]: what a task and its outcome are.
//! - [`episode`]: the loop generate → check → run → fix; [`report`]: the numbers.
//!
//! Real models run on demand only (`cargo run -p evals -- run`): they cost tokens. The tests
//! drive the whole harness with the scripted model.

#![forbid(unsafe_code)]

pub mod agent;
pub mod clock;
pub mod episode;
pub mod fake;
pub mod model;
pub mod prompt;
pub mod report;
pub mod task;
pub mod tasks;
pub mod view;
pub mod world;
