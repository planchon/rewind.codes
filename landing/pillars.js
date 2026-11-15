// rewind.codes: the eight "Why Rewind" cards, each a small animated scene you can play with.
// Every scene is drawn on a canvas and only runs while its card is on screen.
"use strict";

(() => {
  const roots = document.querySelectorAll("[data-scene]");
  if (!roots.length) return;
  const reduceMotion = window.matchMedia("(prefers-reduced-motion: reduce)").matches;

  /* ---------------- drawing helpers ---------------- */

  const INK = "237,231,219";
  const INK2 = "179,172,159";
  const INK3 = "123,117,107";
  const ACC = "52,211,153";
  const RED = "248,113,113";
  const AMBER = "251,191,36";
  const BG = "11,10,9";
  const MONO = '"Geist Mono", ui-monospace, SFMono-Regular, Menlo, monospace';
  const TAU = Math.PI * 2;

  const rgba = (c, a = 1) => `rgba(${c},${Math.max(0, Math.min(1, a)).toFixed(3)})`;
  const clamp = (v, a, b) => (v < a ? a : v > b ? b : v);
  const lerp = (a, b, t) => a + (b - a) * t;
  const sstep = (a, b, x) => {
    const t = clamp((x - a) / (b - a), 0, 1);
    return t * t * (3 - 2 * t);
  };
  const easeOut = (t) => 1 - Math.pow(1 - clamp(t, 0, 1), 3);
  const easeInOut = (t) => {
    t = clamp(t, 0, 1);
    return t < 0.5 ? 4 * t * t * t : 1 - Math.pow(-2 * t + 2, 3) / 2;
  };

  function font(g, size = 10.5, weight = 500) {
    g.font = `${weight} ${size}px ${MONO}`;
  }
  function text(g, s, x, y, color, align = "left", size = 10.5) {
    font(g, size);
    g.fillStyle = color;
    g.textAlign = align;
    g.textBaseline = "middle";
    g.fillText(s, x, y);
  }
  function dot(g, x, y, r, color) {
    if (r <= 0) return;
    g.fillStyle = color;
    g.beginPath();
    g.arc(x, y, r, 0, TAU);
    g.fill();
  }
  function ring(g, x, y, r, color, lw = 1.2) {
    if (r <= 0) return;
    g.strokeStyle = color;
    g.lineWidth = lw;
    g.beginPath();
    g.arc(x, y, r, 0, TAU);
    g.stroke();
  }
  function line(g, x1, y1, x2, y2, color, lw = 1, dash) {
    g.strokeStyle = color;
    g.lineWidth = lw;
    g.setLineDash(dash || []);
    g.beginPath();
    g.moveTo(x1, y1);
    g.lineTo(x2, y2);
    g.stroke();
    g.setLineDash([]);
  }
  function rrect(g, x, y, w, h, r) {
    g.beginPath();
    if (g.roundRect) g.roundRect(x, y, w, h, r);
    else g.rect(x, y, w, h);
  }
  function glow(g, x, y, r, a, c = ACC) {
    if (a <= 0) return;
    const gr = g.createRadialGradient(x, y, 0, x, y, r);
    gr.addColorStop(0, rgba(c, a));
    gr.addColorStop(1, rgba(c, 0));
    g.fillStyle = gr;
    g.fillRect(x - r, y - r, 2 * r, 2 * r);
  }
  function cross(g, x, y, r, color, lw = 1.5) {
    line(g, x - r, y - r, x + r, y + r, color, lw);
    line(g, x - r, y + r, x + r, y - r, color, lw);
  }
  function check(g, x, y, r, color, lw = 1.5) {
    g.strokeStyle = color;
    g.lineWidth = lw;
    g.beginPath();
    g.moveTo(x - r, y);
    g.lineTo(x - r * 0.3, y + r * 0.7);
    g.lineTo(x + r, y - r * 0.7);
    g.stroke();
  }
  function moon(g, x, y, r, color) {
    g.save();
    g.fillStyle = color;
    g.beginPath();
    g.arc(x, y, r, 0, TAU);
    g.fill();
    g.globalCompositeOperation = "destination-out";
    g.beginPath();
    g.arc(x + r * 0.5, y - r * 0.35, r * 0.85, 0, TAU);
    g.fill();
    g.restore();
  }
  // The Rewind mark: two triangles pointing back.
  function mark(g, x, y, s, color) {
    g.fillStyle = color;
    g.beginPath();
    g.moveTo(x + 0.05 * s, y - 0.4 * s);
    g.lineTo(x + 0.05 * s, y + 0.4 * s);
    g.lineTo(x - 0.5 * s, y);
    g.closePath();
    g.moveTo(x + 0.6 * s, y - 0.4 * s);
    g.lineTo(x + 0.6 * s, y + 0.4 * s);
    g.lineTo(x + 0.05 * s, y);
    g.closePath();
    g.fill();
  }
  function tooltip(g, env, s, x, y, color = INK) {
    font(g, 10.5);
    const tw = g.measureText(s).width + 16;
    const bx = clamp(x + 12, 6, env.w - tw - 6);
    const by = clamp(y - 30, 6, env.h - 28);
    rrect(g, bx, by, tw, 22, 6);
    g.fillStyle = rgba(BG, 0.94);
    g.fill();
    g.strokeStyle = rgba(INK, 0.18);
    g.lineWidth = 1;
    g.stroke();
    text(g, s, bx + 8, by + 11, rgba(color));
  }

  /* ---------------- scenes ---------------- */

  const SCENES = {
    // The agent writes a workflow, ships it in one call, and it is live.
    deploy(env) {
      const ORDER = [
        [0, 0],
        [1, 1],
        [2, 0],
        [0, 1],
        [1, 0],
        [2, 1],
      ];
      const BARS = [0.82, 0.56, 0.7, 0.38, 0.64];
      const runs = [];
      let t = 0;
      let auto = 0.3;
      let count = 0;
      const spawn = () => {
        const slot = count++ % ORDER.length;
        for (const r of runs) if (r.slot === slot && r.out == null) r.out = t;
        runs.push({ t0: t, slot, seed: Math.random() * TAU });
      };
      const geo = () => {
        const { w, h } = env;
        const ay = h / 2 - 4;
        const cw = clamp(w * 0.2, 52, 96);
        const cx = 44;
        const ox = cx + cw;
        const zx0 = ox + Math.max(60, w * 0.22);
        const zx1 = w - 22;
        const slots = ORDER.map(([c, r]) => ({ x: lerp(zx0, zx1, (c + 0.5) / 3), y: ay + (r ? 28 : -28) }));
        return { ax: 20, ay, cx, cy: ay - 31, cw, ch: 62, ox, zx0, slots };
      };
      return {
        update(dt) {
          t += dt;
          if (t >= auto) {
            spawn();
            auto = t + 3.4;
          }
          for (let i = runs.length - 1; i >= 0; i--) if (runs[i].out != null && t - runs[i].out > 0.6) runs.splice(i, 1);
        },
        down() {
          spawn();
          auto = t + 5;
        },
        cursor: () => "pointer",
        draw(g) {
          const { w } = env;
          const G = geo();
          ring(g, G.ax, G.ay, 7, rgba(INK2, 0.9));
          dot(g, G.ax, G.ay, 2.5, rgba(INK));
          text(g, "agent", G.ax, G.ay + 19, rgba(INK3), "center");
          line(g, G.ax + 8, G.ay, G.cx, G.ay, rgba(INK, 0.15));

          const cur = runs.length ? runs[runs.length - 1] : null;
          const a = cur ? t - cur.t0 : 99;
          const flash = sstep(1.05, 1.25, a) * (1 - sstep(1.5, 2.3, a));
          rrect(g, G.cx, G.cy, G.cw, G.ch, 6);
          g.fillStyle = rgba(INK, 0.025);
          g.fill();
          g.strokeStyle = flash > 0.01 ? rgba(ACC, 0.25 + 0.6 * flash) : rgba(INK, 0.16);
          g.lineWidth = 1;
          g.stroke();
          const full = G.cw - 18;
          for (let i = 0; i < BARS.length; i++) {
            const grow = a < 1.3 ? easeOut((a - i * 0.2) / 0.28) : 1;
            const alpha = a < 1.25 ? 0.6 : lerp(0.6, 0.18, sstep(1.25, 1.7, a));
            rrect(g, G.cx + 9, G.cy + 10 + i * 9.4, Math.max(0, full * BARS[i] * grow), 3.4, 2);
            g.fillStyle = rgba(INK2, alpha);
            g.fill();
          }
          if (a < 1.1 && Math.sin(t * 18) > -0.3) {
            const i = clamp(Math.floor(a / 0.2), 0, BARS.length - 1);
            const bw = full * BARS[i] * easeOut((a - i * 0.2) / 0.28);
            dot(g, G.cx + 12 + bw, G.cy + 11.7 + i * 9.4, 1.6, rgba(ACC));
          }
          text(g, "workflow", G.cx + G.cw / 2, G.cy + G.ch + 14, rgba(INK3), "center");

          line(g, G.ox + 4, G.ay, G.zx0 - 6, G.ay, rgba(INK, 0.14), 1, [2, 4]);
          text(g, "one call", (G.ox + G.zx0) / 2, G.ay - 11, rgba(INK3), "center");

          let live = 0;
          for (const r of runs) {
            const age = t - r.t0;
            const s = G.slots[r.slot];
            const out = r.out != null ? 1 - clamp((t - r.out) / 0.6, 0, 1) : 1;
            if (age >= 1.25 && age < 1.85) {
              const p = easeInOut((age - 1.25) / 0.6);
              const mx = (G.ox + s.x) / 2;
              const x = (1 - p) * (1 - p) * G.ox + 2 * (1 - p) * p * mx + p * p * s.x;
              const y = (1 - p) * (1 - p) * G.ay + 2 * (1 - p) * p * G.ay + p * p * s.y;
              glow(g, x, y, 13, 0.55);
              dot(g, x, y, 2.6, rgba(ACC));
            }
            if (age >= 1.85) {
              if (r.out == null) live++;
              const b = age - 1.85;
              const k = easeOut(b / 0.45) * out;
              if (b < 0.9) ring(g, s.x, s.y, 8 + b * 26, rgba(ACC, (1 - b / 0.9) * 0.5 * out));
              glow(g, s.x, s.y, 18, 0.16 * k);
              ring(g, s.x, s.y, 8 * k, rgba(ACC, 0.85 * k), 1.3);
              const ang = t * 2.4 + r.seed;
              dot(g, s.x + Math.cos(ang) * 8 * k, s.y + Math.sin(ang) * 8 * k, 1.7, rgba(INK, k));
              dot(g, s.x, s.y, 2.3 * k, rgba(INK, 0.9 * k));
            }
          }
          const liveText = `${live} live`;
          text(g, liveText, w - 14, 16, rgba(live ? INK : INK3), "right");
          dot(g, w - 22 - g.measureText(liveText).width, 16, 2.5, rgba(ACC, live ? 1 : 0.3));
        },
      };
    },

    // One workflow calls many tools, some of them in parallel.
    tools(env) {
      const NAMES = ["github", "slack", "calendar", "ask_user", "llm", "sub-agent"];
      const SCRIPT = [[0], [1], [3], [2, 5, 0], [4]];
      const N = NAMES.length;
      const calls = [];
      let t = 0;
      let step = 0;
      let next = 0.5;
      let hover = -1;
      const call = (i) => calls.push({ i, t0: t });
      const pos = (i) => {
        const { w, h } = env;
        const cx = w / 2;
        const cy = h / 2 - 2;
        const rx = Math.min(w * 0.36, 180);
        const ry = h * 0.32;
        const a = -Math.PI / 2 + (i * TAU) / N;
        let x = cx + Math.cos(a) * rx;
        let y = cy + Math.sin(a) * ry;
        if (env.px != null) {
          x += (env.px - cx) * 0.03;
          y += (env.py - cy) * 0.03;
        }
        return { x, y, a, cx, cy };
      };
      return {
        update(dt) {
          t += dt;
          if (t >= next) {
            for (const i of SCRIPT[step]) call(i);
            step = (step + 1) % SCRIPT.length;
            next = t + (step === 0 ? 1.7 : 1.0);
          }
          for (let k = calls.length - 1; k >= 0; k--) if (t - calls[k].t0 > 1) calls.splice(k, 1);
        },
        move(x, y) {
          let best = -1;
          let bd = 26;
          for (let i = 0; i < N; i++) {
            const p = pos(i);
            const d = Math.hypot(p.x - x, p.y - y);
            if (d < bd) {
              bd = d;
              best = i;
            }
          }
          if (best >= 0 && best !== hover) call(best);
          hover = best;
        },
        leave() {
          hover = -1;
        },
        down() {
          if (hover >= 0) call(hover);
        },
        cursor: () => (hover >= 0 ? "pointer" : ""),
        draw(g) {
          const { cx, cy } = pos(0);
          for (let i = 0; i < N; i++) {
            const p = pos(i);
            line(g, cx, cy, p.x, p.y, rgba(INK, 0.1));
          }
          const active = new Array(N).fill(0);
          for (const c of calls) {
            const a = t - c.t0;
            const p = pos(c.i);
            if (a < 0.45) {
              const q = easeInOut(a / 0.45);
              const x = lerp(cx, p.x, q);
              const y = lerp(cy, p.y, q);
              line(g, cx, cy, x, y, rgba(INK, 0.35));
              dot(g, x, y, 2.2, rgba(INK));
            } else if (a < 0.9) {
              const q = easeInOut((a - 0.45) / 0.45);
              const x = lerp(p.x, cx, q);
              const y = lerp(p.y, cy, q);
              line(g, p.x, p.y, x, y, rgba(ACC, 0.45));
              glow(g, x, y, 10, 0.5);
              dot(g, x, y, 2.4, rgba(ACC));
            }
            active[c.i] = Math.max(active[c.i], 1 - Math.abs(a - 0.45) / 0.45);
          }
          glow(g, cx, cy, 28, calls.length ? 0.2 : 0.1);
          ring(g, cx, cy, 11, rgba(ACC, 0.9), 1.4);
          dot(g, cx, cy, 3, rgba(INK));
          for (let i = 0; i < N; i++) {
            const p = pos(i);
            const k = Math.max(active[i], i === hover ? 0.6 : 0);
            const on = k > 0.1;
            if (on) glow(g, p.x, p.y, 15, 0.35 * k);
            if (NAMES[i] === "ask_user") {
              dot(g, p.x, p.y, 4.6, rgba(BG));
              ring(g, p.x, p.y, 4.6, rgba(on ? ACC : INK2, 0.7 + 0.3 * k), 1.3);
            } else dot(g, p.x, p.y, 4.2, rgba(on ? ACC : INK2, 0.6 + 0.4 * k));
            const above = Math.sin(p.a) < -0.5;
            text(g, NAMES[i], p.x, p.y + (above ? -15 : 16), rgba(on ? INK : INK3), "center");
          }
        },
      };
    },

    // A run sleeps for days, loses its worker, and resumes from its journal.
    durable(env) {
      const STEPS = [
        { s: 0.04, short: "start", long: "start", kind: "call" },
        { s: 0.19, short: "fetch", long: "fetch invoices", kind: "call" },
        { s: 0.33, short: "remind", long: "send reminders", kind: "call" },
        { s: 0.81, short: "reply", long: "client replies", kind: "human" },
        { s: 0.96, short: "done", long: "done", kind: "done" },
      ];
      const SLEEP = [0.37, 0.74];
      const recorded = new Set();
      const flashes = [];
      let t = 0;
      let s = 0;
      let phase = "run";
      let phaseT = 0;
      let worker = 1;
      let crashS = 0;
      let crashes = 0;
      let autoCrashAt = 0.55;
      let crashedThisLoop = false;
      const day = (x) =>
        x < SLEEP[0] ? 0 : x < SLEEP[1] ? (7 * (x - SLEEP[0])) / (SLEEP[1] - SLEEP[0]) : 7 + (2 * (x - SLEEP[1])) / (1 - SLEEP[1]);
      const X = (x) => lerp(30, env.w - 30, x);
      const crash = () => {
        if (phase !== "run" || s < 0.06) return;
        phase = "crashed";
        phaseT = t;
        crashS = s;
        crashes++;
        crashedThisLoop = true;
      };
      const reset = () => {
        s = 0;
        phase = "run";
        phaseT = t;
        worker = 1;
        crashes = 0;
        recorded.clear();
        crashedThisLoop = false;
        autoCrashAt = 0.48 + Math.random() * 0.18;
      };
      return {
        update(dt) {
          t += dt;
          if (phase === "run") {
            const prev = s;
            s = Math.min(1, s + (s > SLEEP[0] && s < SLEEP[1] ? 0.2 : 0.12) * dt);
            STEPS.forEach((st, i) => {
              if (prev < st.s && s >= st.s) {
                recorded.add(i);
                flashes.push({ i, t0: t, replay: false });
              }
            });
            if (!crashedThisLoop && s >= autoCrashAt) crash();
            if (s >= 1) {
              phase = "hold";
              phaseT = t;
            }
          } else if (phase === "crashed") {
            if (t - phaseT > 1.1) {
              phase = "replay";
              phaseT = t;
              worker++;
            }
          } else if (phase === "replay") {
            const p = (t - phaseT) / 0.7;
            STEPS.forEach((st, i) => {
              if (st.s <= crashS * clamp(p, 0, 1) && !flashes.some((f) => f.i === i && f.replay && f.t0 >= phaseT))
                flashes.push({ i, t0: t, replay: true });
            });
            if (p >= 1) {
              phase = "run";
              phaseT = t;
            }
          } else if (phase === "hold" && t - phaseT > 2.4) reset();
          for (let k = flashes.length - 1; k >= 0; k--) if (t - flashes[k].t0 > 1.2) flashes.splice(k, 1);
        },
        action(name) {
          if (name === "crash") crash();
        },
        draw(g) {
          const { w, h } = env;
          const y = h * 0.46;
          const wide = w > 430;
          line(g, X(0), y, X(SLEEP[0]), y, rgba(INK, 0.16));
          line(g, X(SLEEP[0]), y, X(SLEEP[1]), y, rgba(INK, 0.22), 1, [2, 4]);
          line(g, X(SLEEP[1]), y, X(1), y, rgba(INK, 0.16));
          const shown = phase === "replay" || phase === "crashed" ? crashS : s;
          line(g, X(0), y, X(Math.min(shown, SLEEP[0])), y, rgba(INK2, 0.75), 1.5);
          if (shown > SLEEP[0]) line(g, X(SLEEP[0]), y, X(Math.min(shown, SLEEP[1])), y, rgba(INK2, 0.75), 1.5, [2, 4]);
          if (shown > SLEEP[1]) line(g, X(SLEEP[1]), y, X(shown), y, rgba(INK2, 0.75), 1.5);

          const mx = X((SLEEP[0] + SLEEP[1]) / 2);
          moon(g, mx, y - 17, 5, rgba(INK2, 0.85));
          text(g, wide ? "sleeps 7 days" : "7 days", mx, y + 18, rgba(INK3), "center");

          STEPS.forEach((st, i) => {
            const x = X(st.s);
            const rec = recorded.has(i);
            for (const f of flashes) {
              if (f.i !== i) continue;
              const k = 1 - (t - f.t0) / 1.2;
              ring(g, x, y, 6 + (1 - k) * 13, rgba(f.replay ? ACC : INK, k * 0.6));
            }
            if (st.kind === "human") {
              dot(g, x, y, 4.6, rgba(BG));
              ring(g, x, y, 4.6, rgba(ACC, rec ? 1 : 0.4), 1.4);
            } else if (st.kind === "done") {
              if (rec) glow(g, x, y, 15, 0.45);
              dot(g, x, y, 4.6, rgba(rec ? ACC : INK3, rec ? 1 : 0.5));
            } else if (rec) dot(g, x, y, 4.6, rgba(INK2));
            else {
              dot(g, x, y, 4.6, rgba(BG));
              ring(g, x, y, 4.6, rgba(INK3), 1.2);
            }
            const above = i % 2 === 0;
            text(g, wide ? st.long : st.short, x, above ? y - 18 : y + 19, rgba(rec ? INK : INK3), "center");
          });

          if (phase === "run" || phase === "hold") {
            glow(g, X(s), y, 16, 0.45);
            dot(g, X(s), y, 3.2, rgba(INK));
          } else if (phase === "crashed") {
            const k = 1 - clamp((t - phaseT) / 1.1, 0, 1);
            ring(g, X(crashS), y, 6 + (1 - k) * 4, rgba(RED, 0.35 + 0.65 * k), 1.5);
          } else {
            const p = easeInOut((t - phaseT) / 0.7);
            const x = X(crashS * p);
            line(g, X(0), y, x, y, rgba(ACC, 0.9), 1.7);
            ring(g, X(crashS), y, 6, rgba(INK3, 0.8));
            glow(g, x, y, 16, 0.6);
            dot(g, x, y, 3, rgba(ACC));
          }

          const d = day(phase === "run" || phase === "hold" ? s : crashS);
          text(g, `day ${Math.floor(d)}`, 16, 18, rgba(INK), "left", 12);

          const wy = h - 30;
          const bw = 80;
          const shake = phase === "crashed" ? Math.sin(t * 60) * 2 * (1 - clamp((t - phaseT) / 0.4, 0, 1)) : 0;
          rrect(g, 16 + shake, wy - 11, bw, 22, 6);
          g.fillStyle = rgba(phase === "crashed" ? RED : INK, phase === "crashed" ? 0.1 : 0.03);
          g.fill();
          g.strokeStyle = rgba(phase === "crashed" ? RED : INK, phase === "crashed" ? 0.8 : 0.2);
          g.lineWidth = 1;
          g.stroke();
          if (phase === "crashed") cross(g, 28 + shake, wy, 3, rgba(RED));
          else dot(g, 28, wy, 2.6, rgba(ACC));
          text(g, `worker ${worker}`, 38 + shake, wy, rgba(phase === "crashed" ? RED : INK2));

          let status = "";
          let color = INK3;
          if (phase === "crashed") {
            status = "worker lost";
            color = RED;
          } else if (phase === "replay") {
            status = wide ? "new worker, replaying the journal" : "replaying the journal";
            color = ACC;
          } else if (phase === "hold") {
            status = crashes ? (wide ? `done · ${crashes} crash, no step done twice` : "done, nothing done twice") : "done";
            color = ACC;
          } else if (crashes && t - phaseT < 2.5) {
            status = wide ? "resumed where it stopped" : "resumed";
            color = ACC;
          } else if (s > SLEEP[0] && s < SLEEP[1]) status = wide ? "asleep in the database, not in memory" : "asleep in the database";
          text(g, status, 16 + bw + 12, wy, rgba(color));
        },
      };
    },

    // A grid of V8 isolates. A script that reaches for the outside hits its walls.
    isolated(env) {
      const COLS = 4;
      const ROWS = 3;
      const LABELS = ["fs", "net", "env", "process", "Deno"];
      const cells = Array.from({ length: COLS * ROWS }, () => ({
        born: -Math.random() * 3,
        life: 3 + Math.random() * 5,
        phase: Math.random() * TAU,
        hit: -9,
      }));
      const tries = [];
      let t = 0;
      let next = 0.8;
      let hover = -1;
      const rect = (i) => {
        const { w, h } = env;
        const pad = 14;
        const gap = 8;
        const cw = (w - pad * 2 - gap * (COLS - 1)) / COLS;
        const ch = (h - pad * 2 - 18 - gap * (ROWS - 1)) / ROWS;
        const c = i % COLS;
        const r = Math.floor(i / COLS);
        return { x: pad + c * (cw + gap), y: pad + r * (ch + gap), w: cw, h: ch };
      };
      const attempt = (c) =>
        tries.push({ c, t0: t, ang: Math.random() * TAU, label: LABELS[Math.floor(Math.random() * LABELS.length)], hit: false });
      return {
        update(dt) {
          t += dt;
          if (t >= next) {
            attempt(Math.floor(Math.random() * cells.length));
            next = t + 1.3 + Math.random();
          }
          for (const c of cells) {
            if (t - c.born > c.life) {
              c.born = t + Math.random() * 0.8;
              c.life = 3 + Math.random() * 5;
            }
          }
          for (let k = tries.length - 1; k >= 0; k--) {
            const tr = tries[k];
            if (!tr.hit && t - tr.t0 >= 0.35) {
              tr.hit = true;
              cells[tr.c].hit = t;
            }
            if (t - tr.t0 > 1.7) tries.splice(k, 1);
          }
        },
        move(x, y) {
          hover = -1;
          for (let i = 0; i < cells.length; i++) {
            const r = rect(i);
            if (x >= r.x && x <= r.x + r.w && y >= r.y && y <= r.y + r.h) hover = i;
          }
        },
        leave() {
          hover = -1;
        },
        down() {
          if (hover >= 0) attempt(hover);
        },
        cursor: () => (hover >= 0 ? "pointer" : ""),
        draw(g) {
          cells.forEach((c, i) => {
            const r = rect(i);
            const age = t - c.born;
            const alive = age >= 0 ? sstep(0, 0.4, age) * (1 - sstep(c.life - 0.4, c.life, age)) : 0;
            const hit = Math.max(0, 1 - (t - c.hit) / 0.7);
            rrect(g, r.x, r.y, r.w, r.h, 7);
            g.fillStyle = rgba(hit > 0 ? RED : INK, hit > 0 ? 0.06 * hit : 0.02 + 0.03 * alive);
            g.fill();
            g.strokeStyle = hit > 0 ? rgba(RED, 0.3 + 0.6 * hit) : i === hover ? rgba(ACC, 0.85) : rgba(INK, 0.14);
            g.lineWidth = hit > 0 || i === hover ? 1.4 : 1;
            g.stroke();
            const cx = r.x + r.w / 2;
            const cy = r.y + r.h / 2;
            if (alive > 0) {
              glow(g, cx, cy, 13, 0.22 * alive * (0.6 + 0.4 * Math.sin(t * 3 + c.phase)));
              dot(g, cx, cy, 2.6 * alive, rgba(ACC, alive));
              const a = t * 2 + c.phase;
              dot(g, cx + Math.cos(a) * 7, cy + Math.sin(a) * 7, 1.1, rgba(INK, 0.6 * alive));
            }
          });
          for (const k of tries) {
            const r = rect(k.c);
            const cx = r.x + r.w / 2;
            const cy = r.y + r.h / 2;
            const a = t - k.t0;
            const dx = Math.cos(k.ang);
            const dy = Math.sin(k.ang);
            const dist = Math.min((r.w / 2 - 2) / Math.max(Math.abs(dx), 1e-6), (r.h / 2 - 2) / Math.max(Math.abs(dy), 1e-6));
            const hx = cx + dx * dist;
            const hy = cy + dy * dist;
            if (a < 0.35) {
              const q = a / 0.35;
              const tail = Math.max(0, q - 0.45);
              line(g, cx + dx * dist * tail, cy + dy * dist * tail, cx + dx * dist * q, cy + dy * dist * q, rgba(RED, 0.5));
              dot(g, cx + dx * dist * q, cy + dy * dist * q, 1.8, rgba(RED));
            } else {
              const q = clamp((a - 0.35) / 0.5, 0, 1);
              for (let j = 0; j < 5; j++) {
                const sa = k.ang + Math.PI + (j - 2) * 0.5;
                const L = 3 + 8 * q;
                line(g, hx, hy, hx + Math.cos(sa) * L, hy + Math.sin(sa) * L, rgba(RED, (1 - q) * 0.85));
              }
              const k2 = 1 - clamp((a - 0.7) / 1, 0, 1);
              text(g, `no ${k.label}`, clamp(cx, 30, env.w - 30), cy + 13, rgba(RED, k2), "center", 10);
            }
          }
          if (hover >= 0) {
            const r = rect(hover);
            tooltip(g, env, "one V8 isolate, one run", r.x + r.w / 2, r.y);
          }
        },
      };
    },

    // Run 1 hits a bug. The agent fixes it, rewinds, tests, and run 2 reuses the steps already done.
    retest(env) {
      const TOTAL = 9.6;
      const F = [0.04, 0.27, 0.5, 0.73, 0.96];
      const FORK = 0.385;
      let T = 0;
      let dragging = false;
      let lastT = 0;
      let backTill = -1;
      let hoverTrack = false;
      const X = (f) => lerp(62, env.w - 18, f);
      return {
        update(dt) {
          if (!dragging) T = (T + dt) % TOTAL;
        },
        down(x) {
          dragging = true;
          lastT = T;
          this.move(x);
        },
        move(x) {
          hoverTrack = true;
          if (!dragging) return;
          const nt = clamp(x / env.w, 0, 0.999) * TOTAL;
          if (nt < lastT - 0.01) backTill = performance.now() + 250;
          lastT = T = nt;
        },
        up() {
          dragging = false;
        },
        leave() {
          hoverTrack = false;
        },
        cursor: () => (dragging ? "grabbing" : "ew-resize"),
        draw(g) {
          const { w, h } = env;
          const yA = h * 0.33;
          const yB = h * 0.66;
          const wide = w > 430;

          text(g, "run 1", 16, yA, rgba(INK2));
          line(g, X(0), yA, X(1), yA, rgba(INK, 0.14));
          const run2 = T >= 4.8;
          text(g, "run 2", 16, yB, rgba(run2 ? INK2 : INK3, run2 ? 1 : 0.5));
          line(g, X(0), yB, X(1), yB, rgba(INK, run2 ? 0.14 : 0.06), 1, [2, 4]);

          // Run 1: plays to step 3, where the bug is.
          const p1 = clamp(T / 2.4, 0, 1);
          const pos1 = lerp(F[0], F[2], p1);
          line(g, X(F[0]), yA, X(pos1), yA, rgba(INK2, 0.75), 1.5);
          F.forEach((f, i) => {
            const x = X(f);
            if (i < 2 && pos1 >= f) dot(g, x, yA, 4.4, rgba(INK2));
            else if (i === 2 && T >= 2.4) {
              dot(g, x, yA, 6, rgba(BG));
              ring(g, x, yA, 6, rgba(RED, 0.9), 1.4);
              cross(g, x, yA, 2.6, rgba(RED), 1.4);
            } else {
              dot(g, x, yA, 4.4, rgba(BG));
              ring(g, x, yA, 4.4, rgba(INK3, 0.8), 1.1);
            }
          });
          if (T < 2.4) {
            glow(g, X(pos1), yA, 15, 0.45);
            dot(g, X(pos1), yA, 3, rgba(INK));
          }
          if (T >= 2.4) text(g, "bug", X(F[2]), yA + 17, rgba(RED), "center");

          // The agent fixes the code.
          const fix = sstep(2.6, 2.9, T) * (1 - sstep(4.6, 4.9, T));
          if (fix > 0) {
            const s = "agent fixes the code";
            font(g, 10.5);
            const tw = g.measureText(s).width + 16;
            const bx = clamp(X(F[2]) - tw / 2, 8, w - tw - 8);
            rrect(g, bx, yA - 33, tw, 20, 10);
            g.fillStyle = rgba(ACC, 0.12 * fix);
            g.fill();
            g.strokeStyle = rgba(ACC, 0.7 * fix);
            g.stroke();
            text(g, s, bx + 8, yA - 23, rgba(ACC, fix));
          }

          // Rewind: back to the start, then a test in virtual time.
          if (T >= 3.2 && T < 4.0) {
            const q = easeInOut((T - 3.2) / 0.8);
            const x = X(lerp(F[2], F[0], q));
            line(g, x, yA, X(F[2]), yA, rgba(ACC, 0.5 * (1 - q)), 2);
            glow(g, x, yA, 16, 0.6);
            dot(g, x, yA, 3, rgba(ACC));
            mark(g, x - 2, yA - 16, 11, rgba(ACC));
          }
          if (T >= 4.0 && T < 4.8) {
            const q = (T - 4.0) / 0.5;
            const x = X(lerp(F[0], F[4], clamp(q, 0, 1)));
            line(g, X(F[0]), yA, x, yA, rgba(ACC, 0.8 * (1 - sstep(0.5, 0.8, (T - 4) / 0.8))), 1.5);
            if (q < 1) {
              glow(g, x, yA, 14, 0.6);
              dot(g, x, yA, 2.6, rgba(ACC));
            }
            F.forEach((f) => {
              if (X(f) <= x) check(g, X(f), yA - 15, 3.4, rgba(ACC, 0.9));
            });
          }

          // Run 2 forks after step 2: steps 1 and 2 are reused, not run again.
          if (run2) {
            const k = sstep(4.8, 5.2, T);
            g.strokeStyle = rgba(ACC, 0.6 * k);
            g.lineWidth = 1.3;
            g.beginPath();
            g.moveTo(X(FORK), yA);
            g.bezierCurveTo(X(FORK), lerp(yA, yB, 0.6), X(FORK), lerp(yA, yB, 0.4), X(FORK), yB);
            g.stroke();
            for (let i = 0; i < 2; i++) {
              g.setLineDash([2, 2]);
              ring(g, X(F[i]), yB, 4.4, rgba(INK2, 0.7 * k), 1.1);
              g.setLineDash([]);
            }
            text(g, "reused", (X(F[0]) + X(F[1])) / 2, yB + 17, rgba(INK3, k), "center");
            const p2 = clamp((T - 5.2) / 2.4, 0, 1);
            const pos2 = lerp(FORK, F[4], p2);
            if (T >= 5.2) line(g, X(FORK), yB, X(pos2), yB, rgba(ACC, 0.8), 1.5);
            for (let i = 2; i < 5; i++) {
              const x = X(F[i]);
              if (T >= 5.2 && pos2 >= F[i]) {
                if (i === 4) glow(g, x, yB, 15, 0.45);
                dot(g, x, yB, 4.4, rgba(ACC));
              } else {
                dot(g, x, yB, 4.4, rgba(BG));
                ring(g, x, yB, 4.4, rgba(INK3, 0.8 * k), 1.1);
              }
            }
            if (T >= 5.2 && p2 < 1) {
              glow(g, X(pos2), yB, 15, 0.5);
              dot(g, X(pos2), yB, 3, rgba(INK));
            }
          }

          // What is happening, top right.
          let status = "run 1 is running";
          let color = INK3;
          if (T >= 2.4) [status, color] = ["run 1 failed at step 3", RED];
          if (T >= 2.6) [status, color] = ["the agent fixes the code", ACC];
          if (T >= 3.2) [status, color] = ["rewind", ACC];
          if (T >= 4.0) [status, color] = [wide ? "test: 10 days replayed in milliseconds" : "test, in virtual time", ACC];
          if (T >= 4.8) [status, color] = [wide ? "run 2 reuses steps 1 and 2" : "run 2 reuses steps 1-2", INK2];
          if (T >= 7.6) [status, color] = [wide ? "passed, no call made twice" : "passed", ACC];
          text(g, status, w - 16, 18, rgba(color), "right");

          // The tape: drag anywhere to move through the story.
          const ty = h - 16;
          line(g, 16, ty, w - 16, ty, rgba(INK, 0.1), 2);
          const kx = lerp(16, w - 16, T / TOTAL);
          line(g, 16, ty, kx, ty, rgba(INK2, 0.5), 2);
          dot(g, kx, ty, hoverTrack || dragging ? 5 : 4, rgba(INK));
          if (performance.now() < backTill) mark(g, kx - 2, ty - 13, 10, rgba(ACC));
        },
      };
    },

    // The agent loop of any harness, with Rewind plugged into its tools.
    harness(env) {
      const FMTS = ["Anthropic", "OpenAI"];
      let t = 0;
      let theta = Math.PI;
      let detour = -1;
      let laps = 0;
      let fmt = 0;
      let pinned = -99;
      let plug = 0;
      let hoverChip = -1;
      let hoverRw = false;
      const chips = [];
      const geo = () => {
        const { w, h } = env;
        const cx = w * 0.34;
        const cy = h * 0.58;
        const r = Math.min(h * 0.26, w * 0.19);
        const tx = cx + r;
        return { cx, cy, r, mx: cx - r, tx, rx: tx + (w - 18 - tx) * 0.58 };
      };
      return {
        update(dt) {
          t += dt;
          plug = Math.min(1, plug + dt * 1.6);
          if (detour >= 0) {
            if (t - detour > 0.95) detour = -1;
          } else {
            const prev = theta;
            theta += dt * 2.2;
            if (prev < TAU && theta >= TAU) {
              theta -= TAU;
              if (plug >= 1) detour = t;
              laps++;
              if (t - pinned > 6 && laps % 2 === 0) fmt = (fmt + 1) % FMTS.length;
            }
          }
        },
        move(x, y) {
          hoverChip = chips.findIndex((c) => x >= c.x && x <= c.x + c.w && y >= c.y && y <= c.y + c.h);
          const G = geo();
          hoverRw = Math.hypot(x - G.rx, y - G.cy) < 18;
        },
        leave() {
          hoverChip = -1;
          hoverRw = false;
        },
        down() {
          if (hoverChip >= 0) {
            fmt = hoverChip;
            pinned = t;
          } else if (hoverRw) plug = 0;
        },
        cursor: () => (hoverChip >= 0 || hoverRw ? "pointer" : ""),
        draw(g) {
          const G = geo();
          ring(g, G.cx, G.cy, G.r, rgba(INK, 0.16));
          // direction marks on the loop
          for (const a of [-Math.PI / 2, Math.PI / 2]) {
            const x = G.cx + Math.cos(a) * G.r;
            const y = G.cy + Math.sin(a) * G.r;
            const d = a < 0 ? 1 : -1;
            line(g, x - 3 * d, y - 3, x + 1 * d, y, rgba(INK, 0.3));
            line(g, x - 3 * d, y + 3, x + 1 * d, y, rgba(INK, 0.3));
          }
          text(g, "your harness", G.cx, G.cy, rgba(INK3), "center");
          dot(g, G.mx, G.cy, 4.6, rgba(INK2));
          text(g, "model", G.mx, G.cy + 17, rgba(INK2), "center");
          dot(g, G.tx, G.cy, 4.6, rgba(BG));
          ring(g, G.tx, G.cy, 4.6, rgba(INK2), 1.3);
          text(g, "tools", G.tx, G.cy + 17, rgba(INK2), "center");

          // The plug between the tools and Rewind.
          const p = easeOut(plug);
          const mid = (G.tx + G.rx) / 2;
          const gap = (1 - p) * 16;
          line(g, G.tx + 6, G.cy, mid - 3 - gap / 2, G.cy, rgba(INK, 0.35), 1.3);
          line(g, mid + 3 + gap / 2, G.cy, G.rx - 11, G.cy, rgba(INK, 0.35), 1.3);
          rrect(g, mid - 3 - gap / 2 - 4, G.cy - 5, 4, 10, 1.5);
          g.fillStyle = rgba(INK2, 0.9);
          g.fill();
          rrect(g, mid + 3 + gap / 2, G.cy - 6, 5, 12, 2);
          g.strokeStyle = rgba(INK2, 0.9);
          g.lineWidth = 1.2;
          g.stroke();
          if (plug > 0.85 && plug < 1) glow(g, mid, G.cy, 16, (1 - plug) * 5);

          let rk = 0;
          if (detour >= 0) {
            const a = (t - detour) / 0.95;
            const out = a < 0.5;
            const q = easeInOut(out ? a / 0.5 : (a - 0.5) / 0.5);
            const x = out ? lerp(G.tx, G.rx, q) : lerp(G.rx, G.tx, q);
            glow(g, x, G.cy, 12, 0.6);
            dot(g, x, G.cy, 2.6, rgba(ACC));
            text(g, out ? "run_script" : "result", mid, G.cy - 15, rgba(ACC, 0.9), "center");
            rk = 1 - Math.abs(a - 0.5) * 2;
          } else {
            const x = G.cx + Math.cos(theta) * G.r;
            const y = G.cy + Math.sin(theta) * G.r;
            glow(g, x, y, 10, 0.3, INK);
            dot(g, x, y, 2.4, rgba(INK));
          }
          glow(g, G.rx, G.cy, 26, 0.15 + 0.35 * rk);
          dot(g, G.rx, G.cy, 11, rgba(BG));
          ring(g, G.rx, G.cy, 11, rgba(ACC, 0.6 + 0.4 * rk), 1.4);
          mark(g, G.rx - 2, G.cy, 9, rgba(ACC));
          text(g, "rewind", G.rx, G.cy + 22, rgba(ACC), "center");
          if (hoverRw) tooltip(g, env, "run · test · retry · signal", G.rx - 60, G.cy + 6);

          // Tool formats: the same tools, whatever the model API.
          chips.length = 0;
          let x = 14;
          FMTS.forEach((f, i) => {
            font(g, 10.5);
            const tw = g.measureText(f).width + 18;
            const on = i === fmt;
            rrect(g, x, 8, tw, 20, 10);
            g.fillStyle = rgba(ACC, on ? 0.14 : 0);
            g.fill();
            g.strokeStyle = on ? rgba(ACC, 0.8) : rgba(INK, i === hoverChip ? 0.4 : 0.16);
            g.lineWidth = 1;
            g.stroke();
            text(g, f, x + 9, 18, rgba(on ? INK : INK3));
            chips.push({ x, y: 8, w: tw, h: 20 });
            x += tw + 6;
          });
        },
      };
    },

    // A trace built live from the journal; every span knows its line.
    observable(env) {
      const SPANS = [
        { label: "run", a: 0, b: 1, meta: "12 days · completed" },
        { label: "fetch issues", a: 0.02, b: 0.09, meta: "line 7 · 182 ms" },
        { label: "post to Slack", a: 0.1, b: 0.15, meta: "line 10 · 96 ms" },
        { label: "wait for approval", a: 0.16, b: 0.46, wait: true, meta: "line 12 · answered after 2 days" },
        { label: "close 12 issues", a: 0.47, b: 0.6, par: true, meta: "line 15 · 12 calls in parallel" },
        { label: "sleep 10 days", a: 0.61, b: 0.92, wait: true, meta: "line 17 · durable timer" },
        { label: "list reopened", a: 0.93, b: 0.985, meta: "line 18 · 140 ms" },
      ];
      const CYCLE = 7.5;
      let t = 0;
      let hover = -1;
      const geo = () => {
        const { w, h } = env;
        const lw = clamp(w * 0.3, 104, 156);
        const top = 34;
        return { lw, top, rowH: (h - top - 10) / SPANS.length, x0: lw + 6, x1: w - 16 };
      };
      return {
        update(dt) {
          t += dt;
        },
        move(x, y) {
          const G = geo();
          const i = Math.floor((y - G.top) / G.rowH);
          hover = i >= 0 && i < SPANS.length ? i : -1;
        },
        leave() {
          hover = -1;
        },
        cursor: () => (hover >= 0 ? "help" : ""),
        draw(g) {
          const { w, h } = env;
          const G = geo();
          const c = (t % CYCLE) / 5.2;
          const now = clamp(c, 0, 1);
          const fade = 1 - sstep(1.3, 1.42, c);
          text(g, "→ OpenTelemetry", w - 16, 17, rgba(ACC, 0.9), "right");
          for (let k = 0; k <= 4; k++) {
            const x = lerp(G.x0, G.x1, k / 4);
            line(g, x, G.top - 4, x, h - 8, rgba(INK, 0.05));
          }
          SPANS.forEach((s, i) => {
            const y = G.top + i * G.rowH + G.rowH / 2;
            const hv = i === hover;
            if (hv) {
              g.fillStyle = rgba(INK, 0.05);
              g.fillRect(8, y - G.rowH / 2, w - 16, G.rowH);
            }
            font(g, 10.5);
            let label = s.label;
            const room = G.lw - (i ? 26 : 16) - 6;
            while (label.length > 3 && g.measureText(label).width > room) label = label.slice(0, -1);
            if (label !== s.label) label = label.slice(0, -1) + "…";
            text(g, label, i ? 26 : 16, y, rgba(hv ? INK : i ? INK2 : INK));
            if (now < s.a) return;
            const end = Math.min(now, s.b);
            const xa = lerp(G.x0, G.x1, s.a);
            const xb = Math.max(xa + 2, lerp(G.x0, G.x1, end));
            const bh = Math.min(9, G.rowH * 0.45);
            g.globalAlpha = fade;
            if (s.wait) {
              rrect(g, xa, y - bh / 2, xb - xa, bh, 2);
              g.setLineDash([3, 3]);
              g.strokeStyle = rgba(INK2, hv ? 0.9 : 0.55);
              g.lineWidth = 1;
              g.stroke();
              g.setLineDash([]);
            } else if (s.par) {
              for (let j = 0; j < 4; j++) {
                const off = (j - 1.5) * 2.4;
                const xs = xa + j * 3;
                if (xb > xs) {
                  rrect(g, xs, y + off - 1, xb - xs, 2, 1);
                  g.fillStyle = rgba(ACC, hv ? 1 : 0.75);
                  g.fill();
                }
              }
            } else {
              rrect(g, xa, y - bh / 2, xb - xa, bh, 2);
              g.fillStyle = i === 0 ? rgba(INK2, hv ? 0.6 : 0.35) : rgba(ACC, hv ? 1 : 0.75);
              g.fill();
            }
            if (now < s.b && i > 0) glow(g, xb, y, 9, 0.5);
            g.globalAlpha = 1;
          });
          if (c < 1) {
            const nx = lerp(G.x0, G.x1, now);
            line(g, nx, G.top - 6, nx, h - 8, rgba(ACC, 0.35));
          }
          if (hover >= 0 && env.px != null) tooltip(g, env, SPANS[hover].meta, env.px, env.py);
        },
      };
    },

    // One namespace per end user. Load comes and goes; stateless workers follow it.
    scale(env) {
      const CELL = 13;
      const GAP = 5;
      const ROWS = 6;
      const cells = [];
      let cols = 0;
      let t = 0;
      let hover = -1;
      let workers = 2;
      let alert = { i: -1, t0: -99 };
      let nextAlert = 4;
      const build = () => {
        cols = Math.max(8, Math.floor((env.w - 28 + GAP) / (CELL + GAP)));
        const n = cols * ROWS;
        while (cells.length < n) cells.push({ load: 0, runs: 0, id: 1000 + Math.floor(Math.random() * 9000) });
        cells.length = n;
      };
      const origin = () => ({ x: (env.w - (cols * (CELL + GAP) - GAP)) / 2, y: 32 });
      const center = (i) => {
        const o = origin();
        return { x: o.x + (i % cols) * (CELL + GAP) + CELL / 2, y: o.y + Math.floor(i / cols) * (CELL + GAP) + CELL / 2 };
      };
      return {
        resize: build,
        update(dt) {
          t += dt;
          if (!cols) build();
          let active = 0;
          for (let i = 0; i < cells.length; i++) {
            const c = cells[i];
            let rate = 0.05;
            if (env.px != null) {
              const p = center(i);
              rate += 2.6 * Math.max(0, 1 - Math.hypot(p.x - env.px, p.y - env.py) / 80);
            }
            if (Math.random() < rate * dt) c.runs = Math.min(3, c.runs + 1);
            if (c.runs > 0 && Math.random() < 0.3 * dt) c.runs--;
            c.load = lerp(c.load, c.runs / 3, Math.min(1, dt * 6));
            active += c.runs;
          }
          workers = lerp(workers, clamp(Math.ceil(active / 4), 2, 40), Math.min(1, dt * 1.6));
          if (t > nextAlert) {
            alert = { i: Math.floor(Math.random() * cells.length), t0: t };
            cells[alert.i].runs = 3;
            nextAlert = t + 7 + Math.random() * 4;
          }
        },
        move(x, y) {
          const o = origin();
          const c = Math.floor((x - o.x) / (CELL + GAP));
          const r = Math.floor((y - o.y) / (CELL + GAP));
          hover = c >= 0 && r >= 0 && c < cols && r < ROWS ? r * cols + c : -1;
        },
        leave() {
          hover = -1;
        },
        draw(g) {
          if (!cols) build();
          const { w } = env;
          const o = origin();
          const alerting = t - alert.t0 < 2.6;
          cells.forEach((c, i) => {
            const x = o.x + (i % cols) * (CELL + GAP);
            const y = o.y + Math.floor(i / cols) * (CELL + GAP);
            const isAlert = alerting && i === alert.i;
            const dim = hover >= 0 && hover !== i ? 0.4 : 1;
            rrect(g, x, y, CELL, CELL, 3);
            g.fillStyle = isAlert ? rgba(AMBER, 0.8) : rgba(ACC, (0.05 + 0.75 * c.load) * dim);
            g.fill();
            if (c.load < 0.08 && !isAlert) {
              g.strokeStyle = rgba(INK, 0.1 * dim);
              g.lineWidth = 1;
              g.stroke();
            }
            if (i === hover) {
              rrect(g, x - 3, y - 3, CELL + 6, CELL + 6, 5);
              g.strokeStyle = rgba(INK, 0.95);
              g.lineWidth = 1.3;
              g.stroke();
            }
          });
          text(g, `${cells.length} end users, one namespace each`, 16, 17, rgba(INK3));
          const wy = o.y + ROWS * (CELL + GAP) + 16;
          text(g, "workers", 16, wy, rgba(INK3));
          const n = Math.ceil(workers);
          for (let k = 0; k < n; k++) {
            const bx = 76 + k * 13;
            if (bx > w - 96) break;
            const a = clamp(workers - k, 0, 1);
            rrect(g, bx, wy - 6, 9, 12, 2);
            g.strokeStyle = rgba(INK2, 0.75 * a);
            g.lineWidth = 1;
            g.stroke();
            dot(g, bx + 4.5, wy, 1.5, rgba(ACC, a));
          }
          text(g, `${Math.round(workers)} running`, w - 16, wy, rgba(INK2), "right");
          if (hover >= 0) {
            const c = cells[hover];
            const p = center(hover);
            tooltip(g, env, `user_${c.id} · ${c.runs} run${c.runs === 1 ? "" : "s"} · sees only its own`, p.x, p.y);
          } else if (alerting) {
            const p = center(alert.i);
            tooltip(g, env, `user_${cells[alert.i].id} · near its quota, you get an alert`, p.x, p.y, AMBER);
          }
        },
      };
    },
  };

  /* ---------------- mounting and the shared loop ---------------- */

  const items = new Map();
  let raf = 0;
  let last = 0;

  function render(it) {
    const { g, env, scene } = it;
    g.clearRect(0, 0, env.w, env.h);
    scene.draw(g);
  }

  function tick(now) {
    const dt = last ? Math.min(0.05, (now - last) / 1000) : 1 / 60;
    last = now;
    let any = false;
    for (const it of items.values()) {
      if (!it.visible || it.w === 0) continue;
      if (reduceMotion && now > it.awakeUntil) continue;
      it.scene.update(dt);
      render(it);
      any = true;
    }
    if (any) raf = requestAnimationFrame(tick);
    else {
      raf = 0;
      last = 0;
    }
  }

  const kick = () => {
    if (!raf) raf = requestAnimationFrame(tick);
  };

  // With reduced motion, a scene stays still and only plays while someone interacts with it.
  const wake = (it) => {
    it.awakeUntil = performance.now() + 4000;
    kick();
  };

  const io = new IntersectionObserver(
    (entries) => {
      for (const e of entries) {
        const it = items.get(e.target);
        if (it) it.visible = e.isIntersecting;
      }
      kick();
    },
    { rootMargin: "60px" },
  );

  function mount(root) {
    const make = SCENES[root.dataset.scene];
    if (!make) return;
    const canvas = document.createElement("canvas");
    canvas.setAttribute("aria-hidden", "true");
    root.prepend(canvas);
    const g = canvas.getContext("2d");
    const env = { w: 0, h: 0, px: null, py: null };
    const scene = make(env);
    const it = { root, canvas, g, env, scene, visible: false, awakeUntil: reduceMotion ? 0 : Infinity, w: 0 };
    items.set(root, it);

    let primed = false;
    new ResizeObserver(() => {
      const r = canvas.getBoundingClientRect();
      const dpr = Math.min(window.devicePixelRatio || 1, 2);
      env.w = it.w = r.width;
      env.h = r.height;
      canvas.width = Math.round(r.width * dpr);
      canvas.height = Math.round(r.height * dpr);
      g.setTransform(dpr, 0, 0, dpr, 0, 0);
      if (scene.resize) scene.resize();
      if (reduceMotion && !primed) {
        primed = true;
        for (let i = 0; i < 90; i++) scene.update(0.05);
      }
      render(it);
    }).observe(canvas);

    const at = (e) => {
      const r = canvas.getBoundingClientRect();
      return [e.clientX - r.left, e.clientY - r.top];
    };
    canvas.addEventListener("pointermove", (e) => {
      [env.px, env.py] = at(e);
      if (scene.move) scene.move(env.px, env.py);
      canvas.style.cursor = scene.cursor ? scene.cursor() : "";
      if (reduceMotion) wake(it);
    });
    canvas.addEventListener("pointerleave", () => {
      env.px = env.py = null;
      if (scene.leave) scene.leave();
    });
    canvas.addEventListener("pointerdown", (e) => {
      const [x, y] = at(e);
      env.px = x;
      env.py = y;
      if (scene.up) canvas.setPointerCapture(e.pointerId);
      if (scene.down) scene.down(x, y);
      canvas.style.cursor = scene.cursor ? scene.cursor() : "";
      wake(it);
    });
    const up = () => {
      if (scene.up) scene.up();
      canvas.style.cursor = scene.cursor ? scene.cursor() : "";
    };
    canvas.addEventListener("pointerup", up);
    canvas.addEventListener("pointercancel", up);
    for (const b of root.querySelectorAll("[data-action]")) {
      b.addEventListener("click", () => {
        if (scene.action) scene.action(b.dataset.action);
        wake(it);
      });
    }
    io.observe(root);
  }

  const start = () => roots.forEach(mount);
  // Canvas text needs Geist Mono loaded, or the first frames use a fallback.
  if (document.fonts && document.fonts.ready) document.fonts.ready.then(start);
  else start();
})();
