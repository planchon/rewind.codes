// rewind.codes: the hero drawing, reveal on scroll, the waitlist form, and the copy button.
"use strict";

(() => {
  const reduceMotion = window.matchMedia("(prefers-reduced-motion: reduce)").matches;

  /* ---------------- hero: a human hand and an agent's hand ---------------- */

  const data = window.REWIND_HERO;
  const canvas = document.querySelector("[data-hero]");
  if (data && canvas && canvas.getContext) hero(data, canvas);

  function hero(data, canvas) {
    const ctx = canvas.getContext("2d");
    const SPARK = { x: data.spark[0], y: data.spark[1] };
    const IVORY = "242,234,220";
    const STEEL = "214,226,221";
    const EMERALD = "52,211,153";

    const smooth = (e0, e1, x) => {
      const t = Math.min(1, Math.max(0, (x - e0) / (e1 - e0)));
      return t * t * (3 - 2 * t);
    };
    const fadeHuman = (x) => 1 - smooth(1380, 1555, x);
    const fadeAgent = (x) => smooth(150, 420, x);

    // Decode. Coordinates come in half pixels of the 1640x380 frame, tones in 0..15.
    const human = [];
    for (let i = 0; i < data.H.length; i += 3) {
      const x = data.H[i] / 2;
      human.push({ x, y: data.H[i + 1] / 2, t: data.H[i + 2] / 15, f: fadeHuman(x) });
    }
    const nodes = [];
    for (let i = 0; i < data.N.length; i += 5) {
      const x = data.N[i] / 2;
      nodes.push({
        x,
        y: data.N[i + 1] / 2,
        t: data.N[i + 2] / 15,
        kind: data.N[i + 3],
        d: data.N[i + 4],
        f: fadeAgent(x),
      });
    }
    const edges = [];
    for (let i = 0; i < data.E.length; i += 2) {
      const a = nodes[data.E[i]];
      const b = nodes[data.E[i + 1]];
      edges.push({ a, b, d: (a.d + b.d) / 2 });
    }
    const maxDist = nodes.reduce((m, n) => Math.max(m, n.d), 0);
    // Sorted by distance from the fingertip, so a pulse only touches a slice.
    const nodesByD = nodes.slice().sort((p, q) => p.d - q.d);
    const edgesByD = edges.slice().sort((p, q) => p.d - q.d);
    const lowerBound = (arr, v) => {
      let lo = 0;
      let hi = arr.length;
      while (lo < hi) {
        const mid = (lo + hi) >> 1;
        if (arr[mid].d < v) lo = mid + 1;
        else hi = mid;
      }
      return lo;
    };

    const humanLayer = document.createElement("canvas");
    const agentLayer = document.createElement("canvas");
    const scratch = document.createElement("canvas");
    let view = null;

    function layout() {
      const rect = canvas.getBoundingClientRect();
      const w = rect.width;
      const h = rect.height;
      const dpr = Math.min(window.devicePixelRatio || 1, 2);
      // Leave room for the headline under the drawing.
      const copy = canvas.parentElement.querySelector(".hero-copy");
      const top = Math.max(56, h * 0.07);
      const bottom = copy ? Math.max(top + 160, copy.offsetTop - 12) : h * 0.62;
      // On narrow screens, crop to the hands rather than shrink the whole fresco.
      const visible = Math.min(1300, Math.max(430, w < 1000 ? w * 0.72 : w));
      const s = Math.min(w / visible, (bottom - top) / 330);
      const artTop = 25;
      const artBottom = 345;
      const free = bottom - top - (artBottom - artTop) * s;
      const sy = top + free * 0.5 + (SPARK.y - artTop) * s;
      view = { w, h, dpr, s, ox: w / 2 - SPARK.x * s, oy: sy - SPARK.y * s, sx: w / 2, sy };
      for (const c of [canvas, humanLayer, agentLayer, scratch]) {
        c.width = Math.round(w * dpr);
        c.height = Math.round(h * dpr);
      }
      paintLayers();
    }

    function paintLayers() {
      const { s, ox, oy, dpr } = view;
      // Human: a stipple, denser where the fresco is lit.
      let g = humanLayer.getContext("2d");
      g.setTransform(dpr, 0, 0, dpr, 0, 0);
      g.clearRect(0, 0, view.w, view.h);
      for (const p of human) {
        const r = Math.max(0.42, (0.36 + 0.6 * p.t) * s);
        g.fillStyle = `rgba(${IVORY},${(p.f * (0.5 + 0.5 * p.t)).toFixed(3)})`;
        g.beginPath();
        g.arc(ox + p.x * s, oy + p.y * s, r, 0, Math.PI * 2);
        g.fill();
      }
      // Agent: a mesh of nodes, the same arm drawn as a graph.
      g = agentLayer.getContext("2d");
      g.setTransform(dpr, 0, 0, dpr, 0, 0);
      g.clearRect(0, 0, view.w, view.h);
      g.lineWidth = Math.max(0.5, 0.42 * s);
      for (const e of edges) {
        const t = (e.a.t + e.b.t) / 2;
        const f = Math.min(e.a.f, e.b.f);
        g.strokeStyle = `rgba(${STEEL},${((0.12 + 0.42 * t) * f).toFixed(3)})`;
        g.beginPath();
        g.moveTo(ox + e.a.x * s, oy + e.a.y * s);
        g.lineTo(ox + e.b.x * s, oy + e.b.y * s);
        g.stroke();
      }
      for (const n of nodes) {
        const r = Math.max(0.6, (0.5 + 0.85 * n.t) * s * (n.kind === 0 ? 0.85 : 1));
        g.fillStyle = `rgba(${STEEL},${(n.f * (0.45 + 0.55 * n.t)).toFixed(3)})`;
        g.beginPath();
        g.arc(ox + n.x * s, oy + n.y * s, r, 0, Math.PI * 2);
        g.fill();
      }
    }

    // Draws a layer revealed up to an edge, with a soft border: the arms reach in from the sides.
    function drawRevealed(layer, edgeX, soft, fromLeft) {
      if (fromLeft ? edgeX - soft > view.w : edgeX + soft < 0) {
        ctx.drawImage(layer, 0, 0, view.w, view.h);
        return;
      }
      const g = scratch.getContext("2d");
      g.setTransform(1, 0, 0, 1, 0, 0);
      g.globalCompositeOperation = "source-over";
      g.clearRect(0, 0, scratch.width, scratch.height);
      g.drawImage(layer, 0, 0);
      g.globalCompositeOperation = "destination-in";
      g.setTransform(view.dpr, 0, 0, view.dpr, 0, 0);
      const grad = g.createLinearGradient(edgeX - soft, 0, edgeX + soft, 0);
      grad.addColorStop(0, fromLeft ? "rgba(0,0,0,1)" : "rgba(0,0,0,0)");
      grad.addColorStop(1, fromLeft ? "rgba(0,0,0,0)" : "rgba(0,0,0,1)");
      g.fillStyle = grad;
      g.fillRect(0, 0, view.w, view.h);
      g.globalCompositeOperation = "source-over";
      ctx.drawImage(scratch, 0, 0, view.w, view.h);
    }

    function drawSpark(intensity, now) {
      if (intensity <= 0) return;
      const { sx, sy, s } = view;
      const breath = reduceMotion ? 1 : 0.85 + 0.15 * Math.sin(now / 700);
      const k = intensity * breath;
      const R = Math.max(26, 46 * s);
      const glow = ctx.createRadialGradient(sx, sy, 0, sx, sy, R);
      glow.addColorStop(0, `rgba(${EMERALD},${0.55 * k})`);
      glow.addColorStop(0.25, `rgba(${EMERALD},${0.18 * k})`);
      glow.addColorStop(1, `rgba(${EMERALD},0)`);
      ctx.fillStyle = glow;
      ctx.fillRect(sx - R, sy - R, 2 * R, 2 * R);
      ctx.fillStyle = `rgba(225,255,240,${0.95 * intensity})`;
      ctx.beginPath();
      ctx.arc(sx, sy, Math.max(1.4, 2.1 * s), 0, Math.PI * 2);
      ctx.fill();
    }

    // A signal leaves the human's touch and runs through the agent's graph.
    function drawPulse(radius, strength) {
      const { s, ox, oy } = view;
      const band = 70;
      ctx.lineWidth = Math.max(0.7, 0.6 * s);
      const e0 = lowerBound(edgesByD, radius - band);
      const e1 = lowerBound(edgesByD, radius + band);
      for (let i = e0; i < e1; i++) {
        const e = edgesByD[i];
        const k = (1 - Math.abs(e.d - radius) / band) * strength * Math.min(e.a.f, e.b.f);
        if (k <= 0) continue;
        ctx.strokeStyle = `rgba(${EMERALD},${(0.75 * k).toFixed(3)})`;
        ctx.beginPath();
        ctx.moveTo(ox + e.a.x * s, oy + e.a.y * s);
        ctx.lineTo(ox + e.b.x * s, oy + e.b.y * s);
        ctx.stroke();
      }
      const n0 = lowerBound(nodesByD, radius - band);
      const n1 = lowerBound(nodesByD, radius + band);
      for (let i = n0; i < n1; i++) {
        const n = nodesByD[i];
        const k = (1 - Math.abs(n.d - radius) / band) * strength * n.f;
        if (k <= 0) continue;
        ctx.fillStyle = `rgba(${EMERALD},${k.toFixed(3)})`;
        ctx.beginPath();
        ctx.arc(ox + n.x * s, oy + n.y * s, Math.max(1, (1 + 0.8 * n.t) * s), 0, Math.PI * 2);
        ctx.fill();
      }
    }

    const INTRO = 2200;
    const PERIOD = 4200;
    const TRAVEL = 2600;
    let start = 0;
    let raf = 0;
    let visible = true;

    function frame(now) {
      if (!start) start = now;
      const t = now - start;
      const { w, h, s, ox } = view;
      ctx.setTransform(view.dpr, 0, 0, view.dpr, 0, 0);
      ctx.clearRect(0, 0, w, h);

      const p = reduceMotion ? 1 : Math.min(1, t / INTRO);
      const ease = 1 - Math.pow(1 - p, 3);
      // The agent's arm reaches in from the left, the human's from the right.
      const soft = 160 * s;
      const agentEdge = ox + (150 + (905 - 150 + 60) * ease) * s;
      const humanEdge = ox + (1560 - (1560 - 920 + 60) * ease) * s;
      drawRevealed(agentLayer, p >= 1 ? Infinity : agentEdge, soft, true);
      drawRevealed(humanLayer, p >= 1 ? -Infinity : humanEdge, soft, false);

      const sparkOn = smooth(0.82, 1, p);
      let flash = 0;
      if (p >= 1 && !reduceMotion) {
        const since = (t - INTRO) % PERIOD;
        const radius = (since / TRAVEL) * (maxDist + 140);
        const strength = 1 - smooth(0.7, 1, since / TRAVEL);
        if (since < TRAVEL) drawPulse(radius, strength);
        flash = Math.max(0, 1 - since / 500);
      }
      drawSpark(sparkOn * (1 + 0.6 * flash), now);

      if (!reduceMotion && visible) raf = requestAnimationFrame(frame);
    }

    function restart() {
      cancelAnimationFrame(raf);
      raf = requestAnimationFrame(frame);
    }

    layout();
    restart();

    let resizeTimer = 0;
    new ResizeObserver(() => {
      clearTimeout(resizeTimer);
      resizeTimer = setTimeout(() => {
        layout();
        restart();
      }, 60);
    }).observe(canvas);

    // Only animate while the hero is on screen.
    new IntersectionObserver(([entry]) => {
      visible = entry.isIntersecting;
      if (visible) restart();
      else cancelAnimationFrame(raf);
    }).observe(canvas);
  }

  /* ---------------- reveal on scroll ---------------- */

  const reveals = document.querySelectorAll("[data-reveal]");
  if ("IntersectionObserver" in window && !reduceMotion) {
    // Siblings revealed together come in one after the other.
    for (const el of reveals) {
      const group = Array.from(el.parentElement.children).filter((c) => c.hasAttribute("data-reveal"));
      const i = group.indexOf(el);
      if (el.matches(".pillar, .shift-grid article")) {
        el.style.setProperty("--delay", `${(i % 4) * 0.08}s`);
      }
    }
    const io = new IntersectionObserver(
      (entries) => {
        for (const e of entries) {
          if (e.isIntersecting) {
            e.target.classList.add("is-in");
            io.unobserve(e.target);
          }
        }
      },
      { rootMargin: "0px 0px -8% 0px", threshold: 0.12 },
    );
    reveals.forEach((el) => io.observe(el));
  } else {
    reveals.forEach((el) => el.classList.add("is-in"));
  }

  /* ---------------- join the waitlist ---------------- */

  const join = document.querySelector("[data-waitlist]");
  if (join) waitlist(join);

  // The email goes to POST /auth/waitlist on this origin, which vercel.json rewrites to the API:
  // 202 once it is on the list, whether it already has an account or not.
  function waitlist(root) {
    const form = root.querySelector("form");
    const input = form.querySelector("input");
    const submit = form.querySelector("button[type=submit]");
    const error = form.querySelector(".join-error");
    const done = root.querySelector(".join-done");
    const label = submit.textContent;

    const fail = (message, detail) => {
      error.textContent = message;
      if (detail) {
        const small = document.createElement("small");
        small.textContent = detail;
        error.append(small);
      }
      error.hidden = false;
    };

    form.addEventListener("submit", async (event) => {
      event.preventDefault();
      const email = input.value.trim();
      if (!email || submit.disabled) return;
      error.hidden = true;
      submit.disabled = true;
      submit.textContent = "Joining…";
      try {
        const response = await fetch("/auth/waitlist", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ email }),
        });
        if (response.ok) {
          done.querySelector("[data-waitlist-email]").textContent = email;
          form.hidden = true;
          done.hidden = false;
          done.focus();
          return;
        }
        // A problem+json: the API's 400 says what it refused (the identity provider's own
        // message, for an address it does not take).
        const problem = await response.json().catch(() => null);
        if (response.status === 400) {
          fail("This email was not accepted. Check it and try again.", problem?.detail);
        } else {
          fail("The waitlist cannot be reached right now. Try again in a moment.");
        }
      } catch {
        fail("The waitlist cannot be reached. Check your connection and try again.");
      } finally {
        submit.disabled = false;
        submit.textContent = label;
      }
    });

    input.addEventListener("input", () => (error.hidden = true));

    done.querySelector(".join-again").addEventListener("click", () => {
      done.hidden = true;
      form.hidden = false;
      input.focus();
      input.select();
    });

    // The hero's button scrolls here; with a mouse, the field takes the focus on the way. After
    // the click's navigation to the fragment, which would take it back.
    if (window.matchMedia("(pointer: fine)").matches) {
      for (const link of document.querySelectorAll('a[href="#waitlist"]')) {
        link.addEventListener("click", () => {
          setTimeout(() => !form.hidden && input.focus({ preventScroll: true }));
        });
      }
    }
  }

  /* ---------------- copy the install command ---------------- */

  const copy = document.querySelector("[data-copy]");
  const text = document.querySelector("[data-copy-text]");
  if (copy && text && navigator.clipboard) {
    copy.addEventListener("click", async () => {
      try {
        await navigator.clipboard.writeText(text.textContent.trim());
        copy.textContent = "Copied";
      } catch {
        copy.textContent = "Press ⌘C";
      }
      setTimeout(() => (copy.textContent = "Copy"), 1600);
    });
  }
})();
