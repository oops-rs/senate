// The conversation page. The app sends it data (rows, theme, labels, and
// Markdown blocks the core rendered) as call arguments, never as script; it
// sends the app a fixed set of actions. Nothing is parsed here: a message's
// Markdown arrives as a list of blocks, then as diffs naming the blocks that
// changed, and each block's HTML (from senate-markdown: a fixed set of tags
// and classes, text escaped) only when the page asks for it, near the
// viewport or in idle time. An update replaces or appends only the blocks
// it names; a growing code block gets only its new lines.
"use strict";
(function () {
  const post = (msg) => window.webkit.messageHandlers.senate.postMessage(msg);
  let labels = { thinking: "Thinking", copy: "Copy", earlier: "Show earlier", working: "working" };
  const rows = new Map();       // row id -> row data
  const open = new Set();       // ids of open tool rows / thinking lines
  const docs = new Map();       // message key -> { key, el, blocks: Map(id -> block) }
  let hasOlder = false, askedOlder = false;

  const el = (tag, cls, text) => {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text !== undefined) e.textContent = text;
    return e;
  };

  // MARK: Markdown blocks

  // A placeholder's height from what the block is, before any rendering:
  // the content face is monospaced, so characters per line are known.
  const LINE = 23, CHARS = 84;
  function estimate(b) {
    const text = (n) => Math.max(1, Math.ceil(n / CHARS)) * LINE;
    switch (b.kind) {
      case "heading": return 40;
      case "code": return b.lines * 20 + 58;
      case "table": return b.lines * 35 + 16;
      case "list": return Math.max(b.lines * (LINE + 4), text(b.len)) + 10;
      case "rule": return 33;
      default: return text(b.len) + 14;
    }
  }

  function placeholder(b) {
    const e = el("div", "blk ph");
    e.dataset.block = b.id;
    e.style.height = estimate(b) + "px";
    return e;
  }

  const BODY = "md";

  // A message's container, from its first update: every block a sized
  // placeholder; those near the viewport ask for their HTML.
  function messageBody(md) {
    const body = el("div", BODY);
    const doc = { key: md.key, el: body, blocks: new Map() };
    body.dataset.key = md.key;
    for (const b of md.blocks) {
      const e = placeholder(b);
      doc.blocks.set(b.id, { el: e, meta: b, state: "placeholder" });
      body.appendChild(e);
      near.observe(e);
    }
    docs.set(md.key, doc);
    return body;
  }

  // A later update: remove, add and mark only the blocks the diff names.
  function applyDiff(doc, md) {
    const t0 = performance.now();
    for (const id of md.removed) {
      const b = doc.blocks.get(id);
      if (b) { near.unobserve(b.el); b.el.remove(); doc.blocks.delete(id); }
    }
    let prev = null;
    for (const meta of md.blocks) {
      let b = doc.blocks.get(meta.id);
      if (!b) {
        b = { el: placeholder(meta), meta, state: "placeholder" };
        doc.blocks.set(meta.id, b);
        near.observe(b.el);
      }
      b.meta = meta;
      const want = prev ? prev.nextSibling : doc.el.firstChild;
      if (want !== b.el) doc.el.insertBefore(b.el, want);
      prev = b.el;
    }
    for (const id of md.replaced) {
      const b = doc.blocks.get(id);
      if (!b) continue;
      if (b.state === "rendered") {
        // Near the viewport: ask for what changed now; else later, whole.
        if (b.visible) queue(doc.key, id, false); else b.state = "stale";
      } else if (b.state === "placeholder") {
        b.el.style.height = estimate(b.meta) + "px";
      }
    }
    debug.update(doc.key, md, performance.now() - t0);
  }

  // Requests to the app, gathered over a turn (not a frame: a web view
  // behind another window gets few frames, and should still update).
  const pending = new Map(); // key -> { full: Set, patch: Set }
  let flushing = false;
  function queue(key, id, full) {
    let p = pending.get(key);
    if (!p) { p = { full: new Set(), patch: new Set() }; pending.set(key, p); }
    if (full) { p.full.add(id); p.patch.delete(id); } else if (!p.full.has(id)) p.patch.add(id);
    if (!flushing) { flushing = true; setTimeout(flush, 0); }
  }
  function flush() {
    flushing = false;
    for (const [key, p] of pending) {
      const doc = docs.get(key);
      if (!doc) continue;
      if (p.full.size) {
        for (const id of p.full) { const b = doc.blocks.get(id); if (b) b.state = "asked"; }
        post({ action: "render", key, ids: Array.from(p.full), full: true });
      }
      if (p.patch.size) post({ action: "render", key, ids: Array.from(p.patch), full: false });
    }
    pending.clear();
  }

  // Blocks near the viewport (a screen and a half either way) render.
  const near = new IntersectionObserver((entries) => {
    for (const e of entries) {
      const body = e.target.parentElement;
      const doc = body && docs.get(body.dataset.key);
      const b = doc?.blocks.get(e.target.dataset.block);
      if (!b || b.el !== e.target) continue;
      b.visible = e.isIntersecting;
      if (e.isIntersecting && (b.state === "placeholder" || b.state === "stale")) queue(doc.key, b.meta.id, true);
    }
  }, { rootMargin: "150% 0px" });

  // The rest, a few at a time when nothing else happens.
  const idle = window.requestIdleCallback || ((f) => setTimeout(f, 250));
  function idleRender() {
    let n = 0;
    outer: for (const doc of docs.values()) {
      if (!doc.el.isConnected) continue;
      for (const [id, b] of doc.blocks) {
        if (b.state !== "placeholder" && b.state !== "stale") continue;
        queue(doc.key, id, true);
        if (++n >= 24) break outer;
      }
    }
    idle(idleRender);
  }
  idle(idleRender);

  // From the app: rendered blocks of one message.
  const tpl = document.createElement("template");
  function blocks(key, r) {
    const doc = docs.get(key);
    if (!doc) return;
    const t0 = performance.now();
    const bottom = atBottom();
    const anchor = bottom ? null : firstVisible();
    let whole = 0, tails = 0, tailLines = 0;
    for (const res of r.blocks) {
      const b = doc.blocks.get(res.id);
      if (!b) continue;
      if (res.html !== undefined) {
        // The renderer's own output: its fixed set of tags, text escaped.
        tpl.innerHTML = res.html;
        const fresh = el("div", "blk");
        fresh.dataset.block = res.id;
        fresh.appendChild(tpl.content);
        decorate(fresh);
        near.unobserve(b.el);
        b.el.replaceWith(fresh);
        b.el = fresh;
        near.observe(fresh);
        b.state = "rendered";
        whole++;
      } else if (res.code && b.state === "rendered") {
        const bodies = b.el.querySelectorAll("pre > code");
        for (const c of res.code) {
          const code = bodies[c.index];
          if (!code) continue;
          const lines = code.querySelectorAll(":scope > .ln");
          for (let i = lines.length - 1; i >= c.from; i--) lines[i].remove();
          tpl.innerHTML = c.lines.join("");
          code.appendChild(tpl.content);
          tails++; tailLines += c.lines.length;
        }
      } else if (b.state === "asked") {
        b.state = "rendered";
      }
    }
    if (bottom) toBottom(); else keep(anchor);
    debug.render(key, r, whole, tails, tailLines, performance.now() - t0);
  }

  // Code panels: a small language label and a copy button over each block.
  function decorate(root) {
    for (const pre of root.querySelectorAll("pre")) {
      const code = pre.querySelector(":scope > code");
      const wrap = el("div", "code");
      const head = el("div", "code-head");
      head.appendChild(el("span", "lang", (code && code.dataset.language) || ""));
      head.appendChild(el("button", "copy", labels.copy));
      pre.replaceWith(wrap);
      wrap.appendChild(head);
      wrap.appendChild(pre);
    }
  }

  // MARK: Rows

  function thinkingBlock(row) {
    return el("div", "thinking", (row.thinking || []).join("\n\n"));
  }

  function thinkingArea(row) {
    const area = el("div", "thinking-area");
    if (row.thinking && row.thinking.length) {
      const isOpen = open.has(row.id);
      const t = el("div", "thinking-toggle", (isOpen ? "▾ " : "▸ ") + labels.thinking);
      t.dataset.toggle = String(row.id);
      t.setAttribute("role", "button"); t.setAttribute("aria-expanded", String(isOpen));
      area.appendChild(t);
      if (isOpen) area.appendChild(thinkingBlock(row));
    }
    return area;
  }

  function render(row) {
    const r = el("div", "row k-" + row.kind);
    r.dataset.id = String(row.id);
    switch (row.kind) {
      case "user": r.appendChild(el("div", "typed", row.text)); break;
      case "assistant": {
        r.appendChild(thinkingArea(row));
        const known = docs.get(row.md.key);
        if (known) { for (const b of known.blocks.values()) near.unobserve(b.el); docs.delete(row.md.key); }
        r.appendChild(messageBody(row.md));
        break;
      }
      case "tool": {
        const c = row.tool, isOpen = open.has(row.id);
        const head = el("div", "tool-head");
        head.dataset.toggle = String(row.id);
        head.setAttribute("role", "button"); head.setAttribute("aria-expanded", String(isOpen));
        head.appendChild(el("span", "kind", { shell: "›", read: "▤", edit: "✎", write: "✎", search: "⌕", fetch: "◎", sub_agent: "◆" }[c.kind] || "•"));
        head.appendChild(el("span", "title", c.title));
        if (c.exit != null && c.exit !== 0) head.appendChild(el("span", "fail", "exit " + c.exit));
        head.appendChild(el("span", c.status === "completed" ? "ok" : c.status === "failed" ? "fail" : "run",
          c.status === "completed" ? "✓" : c.status === "failed" ? "✕" : "…"));
        const more = c.output != null || c.diff != null || (row.thinking && row.thinking.length);
        if (more) head.appendChild(el("span", "run", isOpen ? "▾" : "▸"));
        r.appendChild(head);
        if (isOpen) {
          if (row.thinking && row.thinking.length) r.appendChild(thinkingBlock(row));
          if (c.diff != null) {
            const d = el("div", "output diff");
            for (const line of c.diff.split("\n")) {
              d.appendChild(el("span", line.startsWith("+") ? "add" : line.startsWith("-") ? "del" : null, line + "\n"));
            }
            r.appendChild(d);
          } else if (c.output != null) {
            r.appendChild(el("div", "output", c.output));
          }
        }
        break;
      }
      case "todo": {
        const d = el("div", "todo");
        for (const t of row.todo) {
          d.appendChild(el("div", t.status === "completed" ? "done" : null,
            (t.status === "completed" ? "☑ " : t.status === "in_progress" ? "◐ " : "☐ ") + t.text));
        }
        r.appendChild(d);
        break;
      }
      case "notice": r.appendChild(el("div", "notice", row.text)); break;
      case "senate": {
        const s = row.senate, d = el("div", "senate");
        const head = el("div", "senate-head");
        head.appendChild(el("span", "from", s.from || ""));
        head.appendChild(el("span", "meta", "  " + (s.kindLabel || "") + "  " + (s.committee || "") + (s.thread ? " · #" + s.thread : "")));
        d.appendChild(head);
        d.appendChild(el("div", "typed", s.text));
        if (s.committee && s.thread) { d.dataset.committee = s.committee; d.dataset.thread = String(s.thread); }
        r.appendChild(d);
        break;
      }
    }
    return r;
  }

  const main = () => document.getElementById("rows");
  const scroller = () => document.scrollingElement;
  const measureBottom = () => scroller().scrollTop + window.innerHeight >= scroller().scrollHeight - 24;
  // Whether the reader is at the foot, as of their last scroll (content
  // that grows afterwards does not count as scrolling up).
  let stuck = true;
  const atBottom = () => stuck;
  const toBottom = () => { scroller().scrollTop = scroller().scrollHeight; };

  function firstVisible() {
    for (const r of main().children) {
      if (!r.dataset.id) continue;
      const top = r.getBoundingClientRect().top;
      if (top >= -r.offsetHeight) return { id: r.dataset.id, top };
    }
    return null;
  }

  function keep(anchor) {
    if (!anchor) return;
    const r = main().querySelector('[data-id="' + anchor.id + '"]');
    if (r) scroller().scrollTop += r.getBoundingClientRect().top - anchor.top;
  }

  // A row that changed: an assistant message keeps its body and applies
  // the diff to it; anything else is drawn again.
  function replace(row) {
    rows.set(row.id, row);
    const old = main().querySelector('[data-id="' + row.id + '"]');
    const doc = row.kind === "assistant" ? docs.get(row.md.key) : null;
    if (old && doc && doc.el.parentElement === old) {
      old.querySelector(":scope > .thinking-area").replaceWith(thinkingArea(row));
      applyDiff(doc, row.md);
      return;
    }
    const next = render(row);
    if (old) old.replaceWith(next); else main().appendChild(next);
  }

  function forgetDocs() {
    for (const d of docs.values()) for (const b of d.blocks.values()) near.unobserve(b.el);
    docs.clear();
  }

  // From the app: one operation at a time.
  let applyWorst = 0, applyResetMs = 0, firstMs = 0, arrivedMs = 0, frameAfterMs = 0, readyAt = 0;
  function apply(op) {
    const tApply = performance.now();
    try { applyInner(op); } finally {
      const d = performance.now() - tApply;
      if (op.type === "reset") {
        applyResetMs = d;
        arrivedMs = tApply - readyAt;
        requestAnimationFrame(() => { frameAfterMs = performance.now() - tApply; });
      }
      applyWorst = Math.max(applyWorst, d);
    }
  }
  function applyInner(op) {
    const bottom = atBottom();
    const anchor = bottom ? null : firstVisible();
    switch (op.type) {
      case "reset":
        rows.clear();
        forgetDocs();
        for (const r of Array.from(main().querySelectorAll(".row"))) r.remove();
        for (const row of op.rows) { rows.set(row.id, row); main().appendChild(render(row)); }
        break;
      case "upsert":
        for (const row of op.rows) replace(row);
        break;
      case "prepend": {
        const first = main().querySelector(".row");
        for (const row of op.rows) { rows.set(row.id, row); main().insertBefore(render(row), first); }
        for (const row of op.changed || []) replace(row);
        askedOlder = false;
        break;
      }
    }
    if (op.state) {
      hasOlder = !!op.state.hasOlder;
      if (!op.state.loadingOlder) askedOlder = false;
      document.getElementById("older").hidden = !hasOlder;
      document.getElementById("older").textContent = op.state.loadingOlder ? "…" : labels.earlier;
      const w = document.getElementById("working");
      w.hidden = !op.state.working;
      w.textContent = labels.working;
    }
    if (op.type === "reset" || bottom) { toBottom(); stuck = true; } else keep(anchor);
    if (op.type === "prepend") nearTop();
    // The first screen's blocks, rendered with the reset.
    const tFirst = performance.now();
    for (const f of op.first || []) blocks(f.key, f.r);
    if (op.type === "reset") firstMs = performance.now() - tFirst;
    // At the foot: the last messages render now, without waiting for the
    // observer (it reports on frames, which a hidden view rarely gets).
    if (op.type === "reset" && stuck && !(op.first || []).length) {
      const last = Array.from(docs.values()).slice(-3);
      for (const d of last) for (const [id, b] of d.blocks) if (b.state === "placeholder") queue(d.key, id, true);
    }
  }

  function toggle(id) {
    const anchor = firstVisible();
    if (open.has(id)) open.delete(id); else open.add(id);
    const row = rows.get(id);
    const old = main().querySelector('[data-id="' + id + '"]');
    if (row && old) {
      if (row.kind === "assistant") old.querySelector(":scope > .thinking-area").replaceWith(thinkingArea(row));
      else old.replaceWith(render(row));
    }
    keep(anchor);
    post({ action: "expand", id, open: open.has(id) });
  }

  document.addEventListener("click", (e) => {
    const a = e.target.closest("a[href]");
    if (a) { e.preventDefault(); post({ action: "link", href: a.getAttribute("href") }); return; }
    const copy = e.target.closest("button.copy");
    if (copy) { post({ action: "copy", text: copy.closest(".code").querySelector("pre > code").textContent }); return; }
    const t = e.target.closest("[data-toggle]");
    if (t && !window.getSelection().toString()) { toggle(Number(t.dataset.toggle)); return; }
    const s = e.target.closest(".senate[data-thread]");
    if (s && !window.getSelection().toString()) {
      post({ action: "thread", committee: s.dataset.committee, thread: Number(s.dataset.thread) });
    }
  });

  // Ask for the page before when within a screen and a half of the top.
  const nearTop = () => {
    if (hasOlder && !askedOlder && scroller().scrollTop < window.innerHeight * 1.5) { askedOlder = true; post({ action: "older" }); }
  };
  new ResizeObserver(() => { if (stuck) toBottom(); }).observe(document.getElementById("rows"));
  document.addEventListener("scroll", () => { stuck = measureBottom(); nearTop(); }, { passive: true });

  // MARK: The debug overlay (⌥D): what each update cost.

  const debug = (() => {
    const box = el("div", "debug");
    box.hidden = true;
    const lines = [];
    const totals = { updates: 0, replaced: 0, appended: 0, removed: 0, whole: 0, tails: 0, highlighted: 0 };
    const ms = (x) => (x < 1 ? "<1" : x.toFixed(0)) + " ms";
    const us = (x) => (x / 1000).toFixed(2) + " ms";
    function show() {
      if (box.hidden) return;
      box.textContent = "updates " + totals.updates +
        " · blocks replaced " + totals.replaced + ", appended " + totals.appended + ", removed " + totals.removed +
        " · rendered whole " + totals.whole + ", code tails " + totals.tails + " · lines highlighted " + totals.highlighted +
        " · whole messages re-rendered 0\n\n" + lines.join("\n");
    }
    function add(line) { lines.unshift(line); if (lines.length > 16) lines.pop(); show(); }
    document.addEventListener("keydown", (e) => {
      if (!e.altKey) return;
      if (e.code === "KeyD") { box.hidden = !box.hidden; show(); e.preventDefault(); }
    });
    document.body.appendChild(box);
    const short = (key) => "#" + key.split("/").pop();
    return {
      note(line) { add(line); },
      update(key, md, pageMs) {
        totals.updates++; totals.replaced += md.replaced.length;
        totals.appended += md.appended.length + md.inserted.length; totals.removed += md.removed.length;
        if (!md.replaced.length && !md.appended.length && !md.inserted.length && !md.removed.length) return;
        add("update " + short(key) + ": " + md.blocks.length + " blocks · replaced " + md.replaced.length + ", appended " +
          (md.appended.length + md.inserted.length) + ", removed " + md.removed.length + " · parse " + us(md.micros) +
          " · page " + ms(pageMs));
      },
      render(key, r, whole, tails, tailLines, pageMs) {
        totals.whole += whole; totals.tails += tails; totals.highlighted += r.highlighted;
        if (!whole && !tails) return;
        add("  render " + short(key) + ": " + (whole ? whole + " whole" : "") + (whole && tails ? ", " : "") +
          (tails ? tails + " code tail (" + tailLines + " lines)" : "") + " · highlighted " + r.highlighted +
          " lines · core " + us(r.micros) + " · page " + ms(pageMs));
      },
    };
  })();

  // Frame timing, for the app's measurement: frames, those over 25 ms, the
  // worst, since the last read.
  let frames = 0, slow = 0, worst = 0, last = 0;
  const tick = (t) => {
    if (last) { const d = t - last; frames++; if (d > 25) slow++; if (d > worst) worst = d; }
    last = t;
    requestAnimationFrame(tick);
  };
  requestAnimationFrame(tick);

  window.senate = {
    apply,
    blocks,
    theme(vars) { for (const k in vars) document.documentElement.style.setProperty("--" + k, vars[k]); },
    labels(l) { labels = Object.assign(labels, l); },
    debug(on) { const d = document.querySelector(".debug"); d.hidden = !on; debug.note("debug overlay on (⌥D)"); },
    scrollToRow(id, at) {
      const r = main().querySelector('[data-id="' + id + '"]');
      if (!r) return;
      const box = r.getBoundingClientRect();
      const y = at === "top" ? box.top - 8 : at === "bottom" ? box.bottom - window.innerHeight + 16 :
        at === "middle" ? box.top + box.height / 2 - window.innerHeight / 2 : box.top - window.innerHeight / 2;
      scroller().scrollTop += y;
      stuck = measureBottom();
    },
    openCall(callId) {
      for (const [id, row] of rows) if (row.kind === "tool" && row.tool.callId === callId && !open.has(id)) toggle(id);
    },
    stats() { const s = { frames, slow, worst, applyWorst, applyResetMs, firstMs, arrivedMs, frameAfterMs }; applyWorst = 0; frames = 0; slow = 0; worst = 0; last = 0; return s; },
  };
  readyAt = performance.now();
  post({ action: "ready" });
})();
