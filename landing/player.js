// The hero's player: the journal of one run of triage.ts, entry by entry, each linked to the line
// that caused it. "Rewind" goes back to the start and replays from the journal: the same entries,
// read instead of executed.

const ENTRIES = [
  { kind: "start", event: "run_started", detail: '{ repo: "acme/api" } · epoch 1', line: 6, at: "Nov 3, 09:12", status: "active" },
  { kind: "call", event: "schedule_tool_call #1", tool: "github.list_issues", line: 7 },
  { kind: "call", event: "tool_call_completed #1", detail: "→ 214 issues", line: 7, at: "Nov 3, 09:12" },
  { kind: "call", event: "schedule_tool_call #2", tool: "slack.post_message", line: 10 },
  { kind: "timer", event: "start_timer #3", detail: "3d, the approval's timeout", line: 12 },
  { kind: "signal", event: "signal_awaited", detail: '"approval"', line: 12, status: "waiting", label: "waiting for approval" },
  { kind: "call", event: "tool_call_completed #2", detail: "→ ok", line: 10, at: "Nov 3, 09:12" },
  { kind: "signal", event: "signal_received", detail: '"approval" { close: true }', line: 12, at: "Nov 3, 11:47", gap: "2 h 35 min later: a human approved", status: "active" },
  { kind: "timer", event: "cancel_timer #3", line: 12 },
  { kind: "call", event: "schedule_tool_call #4 … #40", tool: "github.close_issue × 37", line: 15 },
  { kind: "call", event: "tool_call_completed #4 … #40", detail: "→ 37 closed", line: 15, at: "Nov 3, 11:48" },
  { kind: "timer", event: "start_timer #41", detail: "10d", line: 17, status: "sleeping", label: "sleeping 10 days" },
  { kind: "timer", event: "timer_fired #41", line: 17, at: "Nov 13, 11:48", gap: "10 days later: the run slept in the database", status: "active" },
  { kind: "call", event: "schedule_tool_call #42", tool: "github.list_issues", line: 18 },
  { kind: "call", event: "tool_call_completed #42", detail: "→ 2 issues", line: 18, at: "Nov 13, 11:48" },
  { kind: "end", event: "complete_run", detail: "{ closed: 37, reopened: 2 }", line: 19, status: "completed" },
];

const STEP_MS = 1100;
const REPLAY_MS = 170;

function setup(player) {
  const journal = player.querySelector("[data-journal]");
  const code = player.querySelector("[data-code]");
  const scrub = player.querySelector("[data-scrub]");
  const status = player.querySelector("[data-status]");
  const clock = player.querySelector("[data-clock]");
  const note = player.querySelector("[data-note]");
  const rewind = player.querySelector("[data-rewind]");
  const reduced = window.matchMedia("(prefers-reduced-motion: reduce)").matches;

  scrub.max = String(ENTRIES.length - 1);
  let position = -1;
  let timer = null;
  let replaying = false;

  function item(entry, index) {
    const li = document.createElement("li");
    li.dataset.kind = entry.kind;
    const idx = document.createElement("span");
    idx.className = "idx";
    idx.textContent = String(index);
    const event = document.createElement("span");
    event.className = "event";
    const name = document.createElement("b");
    name.textContent = entry.event;
    event.append(name);
    const detail = entry.tool ?? entry.detail;
    if (detail) {
      const small = document.createElement("span");
      small.className = "detail";
      small.textContent = detail;
      event.append(small);
    }
    const where = document.createElement("span");
    where.className = "where";
    where.textContent = `L${entry.line}`;
    li.append(idx, event, where);
    if (entry.gap) {
      const gap = document.createElement("span");
      gap.className = "gap";
      gap.textContent = entry.gap;
      li.prepend(gap);
    }
    return li;
  }

  function state(upTo) {
    let current = { status: "pending", label: "pending", at: "Nov 3, 09:12" };
    for (const entry of ENTRIES.slice(0, upTo + 1)) {
      if (entry.status) {
        current = { ...current, status: entry.status, label: entry.label ?? entry.status };
      }
      if (entry.at) current = { ...current, at: entry.at };
    }
    return current;
  }

  function show(to, fromJournal) {
    journal.replaceChildren(
      ...ENTRIES.slice(0, to + 1).map((entry, index) => {
        const li = item(entry, index);
        if (fromJournal && entry.kind !== "start") li.classList.add("replayed");
        if (index === to) li.classList.add("current");
        return li;
      }),
    );
    for (const line of code.querySelectorAll("[data-line]")) {
      line.classList.toggle("on", to >= 0 && Number(line.dataset.line) === ENTRIES[to].line);
    }
    const now = state(to);
    status.textContent = now.label;
    status.dataset.state = now.status;
    clock.textContent = now.at;
    scrub.value = String(Math.max(to, 0));
    journal.scrollTop = journal.scrollHeight;
    position = to;
  }

  function stop() {
    if (timer) clearTimeout(timer);
    timer = null;
  }

  function play(fromJournal) {
    stop();
    replaying = fromJournal;
    note.textContent = fromJournal
      ? "Replay: results come from the journal. No tool call is sent again."
      : "Live: every effect is recorded once.";
    const tick = () => {
      if (position >= ENTRIES.length - 1) {
        note.textContent = replaying
          ? "Replayed 16 entries: same output, zero tool calls."
          : "Completed. Rewind to replay it from the journal.";
        timer = null;
        return;
      }
      show(position + 1, replaying);
      timer = setTimeout(tick, replaying ? REPLAY_MS : STEP_MS);
    };
    tick();
  }

  rewind.addEventListener("click", () => {
    stop();
    show(-1, true);
    play(true);
  });

  scrub.addEventListener("input", () => {
    stop();
    show(Number(scrub.value), replaying);
    note.textContent = "Any point of a run can be inspected from its journal.";
  });

  if (reduced) {
    show(ENTRIES.length - 1, false);
    note.textContent = "Rewind to replay it from the journal.";
    return;
  }

  // Start when the player comes into view.
  const observer = new IntersectionObserver(
    (seen) => {
      if (seen.some((entry) => entry.isIntersecting) && position < 0 && !timer) {
        play(false);
        observer.disconnect();
      }
    },
    { threshold: 0.35 },
  );
  observer.observe(player);
}

for (const player of document.querySelectorAll(".player")) setup(player);
