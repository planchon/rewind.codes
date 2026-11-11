//! The ops: the only way the script's glue talks to Rust.
//!
//! Ops are synchronous: they record a command, assign its `seq` and return it. Nothing is
//! asynchronous from V8's point of view; the glue turns a `seq` into a promise that a later
//! delivery settles.

use std::time::Duration;

use chacha20::ChaCha20;
use chacha20::cipher::{KeyIvInit, StreamCipher};
use deno_core::{OpState, extension, op2};
use deno_error::JsErrorBox;
use ptc_core::{
    Annotation, Callsite, Command, LogLevel, Payload, RunId, ScriptError, Seed, Seq, Timestamp,
    Toolset, parse_duration,
};
use ptc_engine::ports::Emitted;
use serde_json::Value;
use sha2::{Digest as _, Sha256};

/// The state of one script instance, kept in deno_core's `OpState`.
pub(crate) struct ExecState {
    /// The run's logical time: `recorded_at` of the last delivered trigger.
    pub(crate) now: Timestamp,
    started_at: Timestamp,
    run_id: RunId,
    toolset: Toolset,
    next_seq: Option<Seq>,
    emitted: Vec<Emitted>,
    /// State of `Math.random` (sfc32), derived from the seed.
    math_random_state: [u32; 4],
    /// The keystream behind `crypto` (ChaCha20, RFC 8439), keyed by the seed.
    crypto: ChaCha20,
}

impl ExecState {
    pub(crate) fn new(run_id: RunId, seed: &Seed, toolset: Toolset, started_at: Timestamp) -> Self {
        // Each generator gets its own key, derived from the seed: what `Math.random` reveals
        // tells nothing about `crypto`.
        let math = derive_key(b"ai-ptc/epoch-1/math-random", seed);
        let math_random_state = std::array::from_fn(|i| {
            u32::from_le_bytes([
                math[4 * i],
                math[4 * i + 1],
                math[4 * i + 2],
                math[4 * i + 3],
            ])
        });
        let key = derive_key(b"ai-ptc/epoch-1/crypto", seed);
        Self {
            now: started_at,
            started_at,
            run_id,
            toolset,
            next_seq: Some(Seq::FIRST),
            emitted: Vec::new(),
            math_random_state,
            crypto: ChaCha20::new(&key.into(), &[0u8; 12].into()),
        }
    }

    pub(crate) fn take_emitted(&mut self) -> Vec<Emitted> {
        std::mem::take(&mut self.emitted)
    }

    fn take_seq(&mut self) -> Result<Seq, JsErrorBox> {
        let seq = self
            .next_seq
            .ok_or_else(|| JsErrorBox::range_error("the script emitted too many commands"))?;
        self.next_seq = seq.next();
        Ok(seq)
    }

    fn command(&mut self, command: Command) {
        self.emitted.push(Emitted::Command(command));
    }

    fn annotation(&mut self, annotation: Annotation) {
        self.emitted.push(Emitted::Annotation(annotation));
    }
}

fn derive_key(domain: &[u8], seed: &Seed) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(seed.as_bytes());
    hasher.finalize().into()
}

/// A position from the glue: 0 when the command was not issued from the script's source.
fn callsite(line: u32, column: u32) -> Option<Callsite> {
    (line > 0).then_some(Callsite { line, column })
}

fn parse_json(text: &str, what: &str) -> Result<Value, JsErrorBox> {
    serde_json::from_str(text)
        .map_err(|err| JsErrorBox::type_error(format!("{what} is not valid JSON: {err}")))
}

/// A number of milliseconds (written by the glue as a string), or `"1h30m"`-style text.
fn parse_script_duration(text: &str) -> Result<Duration, JsErrorBox> {
    if let Ok(ms) = text.parse::<f64>() {
        if ms.is_finite() && ms >= 0.0 && ms < u64::MAX as f64 {
            return Ok(Duration::from_millis(ms.round() as u64));
        }
        return Err(JsErrorBox::type_error(format!("invalid duration: {text}")));
    }
    parse_duration(text)
        .map_err(|err| JsErrorBox::type_error(format!("invalid duration {text:?}: {err}")))
}

#[op2(fast)]
fn op_ptc_now(state: &mut OpState) -> f64 {
    state.borrow::<ExecState>().now.as_millis() as f64
}

#[op2(fast)]
fn op_ptc_started_at(state: &mut OpState) -> f64 {
    state.borrow::<ExecState>().started_at.as_millis() as f64
}

#[op2]
#[string]
fn op_ptc_run_id(state: &mut OpState) -> String {
    state.borrow::<ExecState>().run_id.to_string()
}

/// The four 32-bit words of the sfc32 state behind `Math.random`.
#[op2]
#[string]
fn op_ptc_math_random_state(state: &mut OpState) -> String {
    let words = state.borrow::<ExecState>().math_random_state;
    words.map(|word| word.to_string()).join(",")
}

/// Fills `bytes` from the run's ChaCha20 keystream.
#[op2(fast)]
fn op_ptc_random_fill(state: &mut OpState, #[buffer] bytes: &mut [u8]) {
    bytes.fill(0);
    state
        .borrow_mut::<ExecState>()
        .crypto
        .apply_keystream(bytes);
}

#[op2(fast)]
fn op_ptc_call_tool(
    state: &mut OpState,
    #[string] tool: &str,
    #[string] args: &str,
    line: u32,
    column: u32,
) -> Result<u32, JsErrorBox> {
    let state = state.borrow_mut::<ExecState>();
    if !state.toolset.contains(tool) {
        return Err(JsErrorBox::type_error(format!(
            "{tool} is not a tool of this run's toolset"
        )));
    }
    let args = parse_json(args, "the arguments")?;
    let seq = state.take_seq()?;
    state.command(Command::ScheduleToolCall {
        seq,
        tool: tool.to_owned(),
        args: Payload::Value(args),
        callsite: callsite(line, column),
    });
    Ok(seq.0)
}

#[op2(fast)]
fn op_ptc_start_timer(
    state: &mut OpState,
    #[string] duration: &str,
    line: u32,
    column: u32,
) -> Result<u32, JsErrorBox> {
    let duration = parse_script_duration(duration)?;
    let state = state.borrow_mut::<ExecState>();
    let seq = state.take_seq()?;
    state.command(Command::StartTimer {
        seq,
        duration_ms: u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
        fire_at: state.now.saturating_add(duration),
        callsite: callsite(line, column),
    });
    Ok(seq.0)
}

#[op2(fast)]
fn op_ptc_cancel_timer(state: &mut OpState, seq: u32) {
    state
        .borrow_mut::<ExecState>()
        .command(Command::CancelTimer { seq: Seq(seq) });
}

#[op2(fast)]
fn op_ptc_log(
    state: &mut OpState,
    #[string] level: &str,
    #[string] message: String,
    line: u32,
    column: u32,
) {
    let level = match level {
        "debug" => LogLevel::Debug,
        "warn" => LogLevel::Warn,
        "error" => LogLevel::Error,
        _ => LogLevel::Info,
    };
    state.borrow_mut::<ExecState>().annotation(Annotation::Log {
        level,
        message,
        callsite: callsite(line, column),
    });
}

/// `timeout` is the `seq` of the timeout's timer, 0 for none.
#[op2(fast)]
fn op_ptc_signal_awaited(
    state: &mut OpState,
    #[string] name: String,
    timeout: u32,
    line: u32,
    column: u32,
) {
    state
        .borrow_mut::<ExecState>()
        .annotation(Annotation::SignalAwaited {
            name,
            timeout: (timeout > 0).then_some(Seq(timeout)),
            callsite: callsite(line, column),
        });
}

#[op2(fast)]
fn op_ptc_complete(state: &mut OpState, #[string] output: &str) -> Result<(), JsErrorBox> {
    let output = parse_json(output, "the output of main")?;
    state
        .borrow_mut::<ExecState>()
        .command(Command::CompleteRun {
            output: Payload::Value(output),
        });
    Ok(())
}

#[op2(fast)]
fn op_ptc_fail(
    state: &mut OpState,
    #[string] name: String,
    #[string] message: String,
    #[string] stack: String,
) {
    state.borrow_mut::<ExecState>().command(Command::FailRun {
        error: ScriptError {
            name,
            message,
            stack: (!stack.is_empty()).then_some(stack),
        },
    });
}

extension!(
    ptc_sandbox,
    ops = [
        op_ptc_now,
        op_ptc_started_at,
        op_ptc_run_id,
        op_ptc_math_random_state,
        op_ptc_random_fill,
        op_ptc_call_tool,
        op_ptc_start_timer,
        op_ptc_cancel_timer,
        op_ptc_log,
        op_ptc_signal_awaited,
        op_ptc_complete,
        op_ptc_fail,
    ],
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generators_are_keyed_by_the_seed_and_independent() {
        let toolset = Toolset::default();
        let run = RunId::from_u128(1);
        let a = ExecState::new(
            run,
            &Seed::from_bytes([1; 32]),
            toolset.clone(),
            Timestamp(0),
        );
        let b = ExecState::new(
            run,
            &Seed::from_bytes([1; 32]),
            toolset.clone(),
            Timestamp(0),
        );
        let c = ExecState::new(run, &Seed::from_bytes([2; 32]), toolset, Timestamp(0));
        assert_eq!(a.math_random_state, b.math_random_state);
        assert_ne!(a.math_random_state, c.math_random_state);

        let keystream = |mut state: ExecState| {
            let mut bytes = [0u8; 16];
            state.crypto.apply_keystream(&mut bytes);
            bytes
        };
        assert_eq!(keystream(a), keystream(b));
    }

    #[test]
    fn script_durations_are_milliseconds_or_text() {
        assert_eq!(
            parse_script_duration("1500").unwrap(),
            Duration::from_millis(1500)
        );
        assert_eq!(parse_script_duration("0.4").unwrap(), Duration::ZERO);
        assert_eq!(
            parse_script_duration("1h").unwrap(),
            Duration::from_secs(3600)
        );
        assert!(parse_script_duration("-1").is_err());
        assert!(parse_script_duration("NaN").is_err());
        assert!(parse_script_duration("soon").is_err());
    }
}
