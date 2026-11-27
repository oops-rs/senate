// The conversation page's one script. The app injects it into a content
// world of its own ("senate"): the page's world runs no script, and only
// this world can reach the app's message handler. The app sends data (rows,
// theme, labels, rendered Markdown blocks) as call arguments, never as
// script; this sends the app a fixed set of actions.
//
// Nothing is parsed here. A message's Markdown arrives as a list of blocks,
// then as diffs naming the blocks that changed, and each block's HTML (from
// senate-markdown: a fixed set of tags and classes, text escaped) only when
// this asks for it, near the viewport or in idle time. An update replaces
// or appends only the blocks it names; a growing code block gets only its
// new lines; a message's container is never written whole.
"use strict";
(function () {
  const post = (msg) => window.webkit.messageHandlers.senate.postMessage(msg);
  let labels = { thinking: "Thinking", copy: "Copy", earlier: "Show earlier", working: "working", commands: "%lld commands" };
  // A row's key: its id at the top level; "m" and its id inside a group of
  // commands (whose own id is its first command's).
  const key = (row) => row.key || String(row.id);
  const rows = new Map();       // row id -> row data
  const open = new Set();       // ids of open tool rows / thinking lines
  const docs = new Map();       // message key -> { key, el, blocks: Map(id -> block) }
  let hasOlder = false, askedOlder = false;

  // The one HTML sink takes the renderer's output through one named
  // policy, which the page's Content-Security-Policy requires where
  // WebKit has Trusted Types.
  const policy = (window.trustedTypes && window.trustedTypes.createPolicy)
    ? window.trustedTypes.createPolicy("senate", { createHTML: (s) => s }) : null;
  const trusted = (s) => (policy ? policy.createHTML(s) : s);

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

  // A message's container, from its first update: every block a sized
  // placeholder; those near the viewport ask for their HTML.
  function messageBody(md) {
    const body = el("div", "md");
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
  let flushing = false, outstanding = 0;
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
        outstanding++;
        post({ action: "render", key, ids: Array.from(p.full), full: true });
      }
      if (p.patch.size) { outstanding++; post({ action: "render", key, ids: Array.from(p.patch), full: false }); }
    }
    pending.clear();
  }

  // Blocks near the viewport (a screen and a half either way) render.
  const near = new IntersectionObserver((entries) => {
    for (const e of entries) {
      const body = e.target.parentElement;
      const doc = body && docs.get(body.dataset.key);
      const b = doc && doc.blocks.get(e.target.dataset.block);
      if (!b || b.el !== e.target) continue;
      b.visible = e.isIntersecting;
      if (e.isIntersecting && (b.state === "placeholder" || b.state === "stale")) queue(doc.key, b.meta.id, true);
    }
  }, { rootMargin: "150% 0px" });

  const unrendered = function* () {
    for (const doc of docs.values()) {
      if (!doc.el.isConnected) continue;
      for (const [id, b] of doc.blocks) if (b.state === "placeholder" || b.state === "stale") yield [doc, id];
    }
  };

  // The rest, a few at a time when nothing else happens.
  const idle = window.requestIdleCallback || ((f) => setTimeout(f, 250));
  function idleRender() {
    let n = 0;
    for (const [doc, id] of unrendered()) { queue(doc.key, id, true); if (++n >= 24) break; }
    idle(idleRender);
  }
  idle(idleRender);

  // Everything read so far rendered (find, select all): ask for every
  // block left, and wait for the answers (a few seconds at most).
  async function renderAll() {
    for (const [doc, id] of unrendered()) queue(doc.key, id, true);
    const until = performance.now() + 5000;
    while (performance.now() < until) {
      await new Promise((r) => setTimeout(r, 20));
      if (!flushing && outstanding === 0 && unrendered().next().done) return;
    }
  }

  // From the app: rendered blocks of one message.
  const tpl = document.createElement("template");
  function blocks(key, r) {
    outstanding = Math.max(0, outstanding - 1);
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
        tpl.innerHTML = trusted(res.html);
        const fresh = el("div", "blk");
        fresh.dataset.block = res.id;
        fresh.appendChild(tpl.content);
        decorate(fresh);
        near.unobserve(b.el);
        if (anchor && anchor.el === b.el) anchor.el = fresh;
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
          tpl.innerHTML = trusted(c.lines.join(""));
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
      const isOpen = open.has(key(row));
      const t = el("div", "thinking-toggle", (isOpen ? "▾ " : "▸ ") + labels.thinking);
      t.dataset.toggle = key(row);
      t.setAttribute("role", "button"); t.setAttribute("aria-expanded", String(isOpen));
      area.appendChild(t);
      if (isOpen) area.appendChild(thinkingBlock(row));
    }
    return area;
  }

  // A row's Markdown body: a fresh container (its blocks to come).
  function body(md) {
    const known = docs.get(md.key);
    if (known) { for (const b of known.blocks.values()) near.unobserve(b.el); docs.delete(md.key); }
    return messageBody(md);
  }

  // The MCP mark, drawn as the runtime marks are: a small monochrome
  // vector in the text's colour (no bitmap, no brand colour).
  function mcpMark() {
    const ns = "http://www.w3.org/2000/svg";
    const svg = document.createElementNS(ns, "svg");
    svg.setAttribute("viewBox", "0 0 16 16");
    svg.setAttribute("width", "12"); svg.setAttribute("height", "12");
    svg.setAttribute("aria-hidden", "true");
    svg.classList.add("mcp-mark");
    for (const d of ["M2.5 7.5 L7.5 2.5 a2.1 2.1 0 0 1 3 3 L5.5 10.5",
                     "M6 5 L9 2 a2.1 2.1 0 0 1 3 3 L9 8 a2.1 2.1 0 0 1 3 3 L8.5 14.5",
                     "M7.5 6.5 L5.5 8.5 a2.1 2.1 0 0 0 3 3 L10.5 9.5"]) {
      const p = document.createElementNS(ns, "path");
      p.setAttribute("d", d);
      svg.appendChild(p);
    }
    return svg;
  }

  // A tool's glyph: the MCP mark for an MCP tool, else its kind's.
  function toolGlyph(c) {
    const k = el("span", "kind");
    if (c.mcp) k.appendChild(mcpMark());
    else k.textContent = { shell: "›", read: "▤", edit: "✎", write: "✎", search: "⌕", fetch: "◎", sub_agent: "◆" }[c.kind] || "•";
    return k;
  }

  // A tool's line: an MCP tool as server.tool, then what it was given;
  // its raw name in the tooltip.
  function toolTitle(c) {
    const t = el("span", "title");
    if (c.mcp) {
      t.appendChild(el("span", "mcp-name", c.mcp));
      if (c.title && c.title !== c.mcp && c.title !== c.name) t.appendChild(el("span", "mcp-args", " " + c.title));
      t.title = c.name;
    } else t.textContent = c.title;
    return t;
  }

  function groupHead(row) {
    const k = key(row), isOpen = open.has(k), ms = row.members;
    const done = (m) => m.tool.status === "completed" || m.tool.status === "failed";
    const running = ms.find((m) => !done(m));
    const failed = ms.filter((m) => m.tool.status === "failed" || (m.tool.exit != null && m.tool.exit !== 0)).length;
    const shown = running || ms[ms.length - 1];
    const head = el("div", "tool-head group-head");
    head.dataset.toggle = k;
    head.setAttribute("role", "button"); head.setAttribute("aria-expanded", String(isOpen));
    // A run of one MCP server's calls carries its mark.
    const allMcp = ms.every((m) => m.tool.mcp);
    head.appendChild(allMcp ? toolGlyph(shown.tool) : el("span", "kind", "≡"));
    head.appendChild(el("span", "count", labels.commands.replace("%lld", String(ms.length))));
    head.appendChild(toolTitle(shown.tool));
    if (failed) head.appendChild(el("span", "fail", "✕ " + failed));
    else head.appendChild(el("span", running ? "run" : "ok", running ? "…" : "✓"));
    head.appendChild(el("span", "run", isOpen ? "▾" : "▸"));
    return head;
  }

  function render(row) {
    const r = el("div", "row k-" + row.kind);
    r.dataset.id = key(row);
    switch (row.kind) {
      case "user": r.appendChild(el("div", "typed", row.text)); break;
      case "assistant":
        r.appendChild(thinkingArea(row));
        r.appendChild(body(row.md));
        break;
      case "event": {
        // A thread's event: others' under who said it, yours in a bubble.
        if (row.mine) r.classList.add("mine");
        const head = el("div", "event-head");
        if (!row.mine) head.appendChild(el("span", "by", row.by));
        head.appendChild(el("span", "meta", row.kindLabel + " · " + row.when));
        // Words the Speaker's office carried: how they came, under who.
        if (row.via) head.appendChild(el("div", "via", row.via));
        const content = el("div", "event-body");
        content.appendChild(body(row.md));
        if (row.mine) { r.appendChild(content); r.appendChild(head); } else { r.appendChild(head); r.appendChild(content); }
        break;
      }
      case "status": {
        // A task status: who, what it is doing now, when; its checklist
        // and links under it, when it gives them.
        const s = row.status;
        const line = el("div", "status-line");
        line.appendChild(el("span", "by", row.by));
        line.appendChild(el("span", "what", s.line));
        line.appendChild(el("span", "meta", row.when));
        r.classList.add("st-" + s.state);
        r.appendChild(line);
        // Why, in full (an escalation's reason): Markdown, as a message.
        if (row.md) {
          const why = el("div", "status-reason");
          why.appendChild(body(row.md));
          r.appendChild(why);
        }
        if (s.steps.length) {
          const list = el("ul", "status-steps");
          for (const st of s.steps) {
            const li = el("li", st.done ? "done" : "");
            li.appendChild(el("span", "tick", st.done ? "✓" : "○"));
            li.appendChild(el("span", "", st.label));
            list.appendChild(li);
          }
          r.appendChild(list);
        }
        if (s.links.length) {
          const links = el("div", "status-links");
          for (const l of s.links) {
            const a = el("a", "", l.label);
            a.setAttribute("href", l.url);
            links.appendChild(a);
          }
          r.appendChild(links);
        }
        break;
      }
      case "group": {
        // Commands back to back: a count and the running (else the last)
        // one; open, each as its own row. The members are always here,
        // hidden while closed, so find and select-all reach them.
        r.appendChild(groupHead(row));
        const box = el("div", "group-members");
        box.hidden = !open.has(key(row));
        if (row.thinking && row.thinking.length) box.appendChild(thinkingBlock(row));
        for (const m of row.members) {
          m.key = "m" + m.id;
          rows.set(m.key, m);
          box.appendChild(render(m));
        }
        r.appendChild(box);
        break;
      }
      case "tool": {
        const c = row.tool, isOpen = open.has(key(row));
        const head = el("div", "tool-head");
        head.dataset.toggle = key(row);
        head.setAttribute("role", "button"); head.setAttribute("aria-expanded", String(isOpen));
        head.appendChild(toolGlyph(c));
        head.appendChild(toolTitle(c));
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
        // Sent from here, not in the session's conversation yet.
        if (row.pending) { d.classList.add("pending"); head.appendChild(el("span", "pending-mark", "  " + row.pending)); }
        d.appendChild(head);
        const content = el("div", "senate-body");
        content.appendChild(body(row.md));
        d.appendChild(content);
        // Images it sent along: small pictures (served by the app).
        if (s.images && s.images.length) {
          const strip = el("div", "senate-images");
          for (const src of s.images) {
            if (!/^senate-app:\/\/thumb\/[0-9a-f-]+$/.test(src)) continue;
            const img = el("img");
            img.src = src; img.alt = ""; img.decoding = "async";
            strip.appendChild(img);
          }
          d.appendChild(strip);
        }
        if (s.committee && s.thread) {
          d.dataset.committee = s.committee; d.dataset.thread = String(s.thread);
          d.setAttribute("role", "link");
        }
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

  // What the reader is looking at: the block (or row) at the top of the
  // viewport, held where it is while what is above it changes height.
  function firstVisible() {
    const box = main().getBoundingClientRect();
    const x = box.left + Math.min(48, box.width / 2);
    for (let y = 2; y < Math.min(window.innerHeight, 240); y += 16) {
      const hit = document.elementFromPoint(x, y);
      const a = hit && (hit.closest(".blk") || hit.closest(".row"));
      if (a && main().contains(a)) return { el: a, top: a.getBoundingClientRect().top };
    }
    for (const r of main().children) {
      if (!r.dataset.id) continue;
      const top = r.getBoundingClientRect().top;
      if (top >= -r.offsetHeight) return { el: r, top };
    }
    return null;
  }

  function keep(anchor) {
    if (!anchor || !anchor.el.isConnected) return;
    scroller().scrollTop += anchor.el.getBoundingClientRect().top - anchor.top;
  }

  // A row that changed: a Markdown message keeps its body and applies the
  // diff to it; anything else is drawn again.
  function replace(row) {
    rows.set(key(row), row);
    const old = main().querySelector('[data-id="' + key(row) + '"]');
    if (old && row.kind === "group" && old.classList.contains("k-group")) {
      // A run that grew or changed: its head, and only the members that did.
      old.querySelector(":scope > .tool-head").replaceWith(groupHead(row));
      const box = old.querySelector(":scope > .group-members");
      const th = box.querySelector(":scope > .thinking");
      if (row.thinking && row.thinking.length) {
        if (th) th.textContent = row.thinking.join("\n\n"); else box.prepend(thinkingBlock(row));
      } else if (th) th.remove();
      for (const m of row.members) {
        m.key = "m" + m.id;
        const before = rows.get(m.key);
        rows.set(m.key, m);
        const e = box.querySelector(':scope > [data-id="' + m.key + '"]');
        if (!e) box.appendChild(render(m));
        else if (JSON.stringify(before) !== JSON.stringify(m)) e.replaceWith(render(m));
      }
      return;
    }
    const doc = row.md ? docs.get(row.md.key) : null;
    if (old && doc && old.contains(doc.el)) {
      if (row.kind === "assistant") old.querySelector(":scope > .thinking-area").replaceWith(thinkingArea(row));
      if (row.kind === "event") old.querySelector(".event-head .meta").textContent = row.kindLabel + " · " + row.when;
      applyDiff(doc, row.md);
      return;
    }
    const next = render(row);
    if (old) old.replaceWith(next); else main().appendChild(next);
  }

  function forgetDocs() {
    for (const d of docs.values()) for (const b of d.blocks.values()) near.unobserve(b.el);
    docs.clear();
    pending.clear();
    outstanding = 0;
  }

  // From the app: one operation at a time.
  function apply(op) {
    const bottom = atBottom();
    let anchor = bottom ? null : firstVisible();
    switch (op.type) {
      case "reset":
        rows.clear();
        open.clear();
        forgetDocs();
        clearFind();
        for (const r of Array.from(main().querySelectorAll(".row"))) r.remove();
        for (const row of op.rows) { rows.set(key(row), row); main().appendChild(render(row)); }
        break;
      case "upsert":
        for (const row of op.rows) replace(row);
        break;
      case "prepend": {
        // Rows that joined one of the older rows: gone. If the reader was
        // on one of them, they are held on the row after instead.
        const gone = (op.remove || []).map((k) => main().querySelector(':scope > [data-id="' + k + '"]')).filter(Boolean);
        if (anchor && gone.some((g) => g.contains(anchor.el))) {
          let next = gone[gone.length - 1].nextElementSibling;
          while (next && (!next.dataset.id || gone.includes(next))) next = next.nextElementSibling;
          anchor = next ? { el: next, top: next.getBoundingClientRect().top } : null;
        }
        for (const g of gone) { rows.delete(g.dataset.id); g.remove(); }
        const first = main().querySelector(".row");
        for (const row of op.rows) { rows.set(key(row), row); main().insertBefore(render(row), first); }
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
    // The first screen's blocks, rendered with the reset.
    for (const f of op.first || []) { outstanding++; blocks(f.key, f.r); }
    if (op.type === "reset" || bottom) { toBottom(); stuck = true; } else keep(anchor);
    if (op.type === "prepend") nearTop();
  }

  function toggle(k) {
    const anchor = firstVisible();
    if (open.has(k)) open.delete(k); else open.add(k);
    const row = rows.get(k);
    const old = main().querySelector('[data-id="' + k + '"]');
    if (row && old) {
      if (row.kind === "group") {
        old.querySelector(":scope > .tool-head").replaceWith(groupHead(row));
        old.querySelector(":scope > .group-members").hidden = !open.has(k);
      } else if (row.kind === "assistant") {
        old.querySelector(":scope > .thinking-area").replaceWith(thinkingArea(row));
      } else {
        old.replaceWith(render(row));
      }
      // A command opened from outside (a link, a search) inside a closed group.
      const box = main().querySelector('[data-id="' + k + '"]')?.closest(".group-members");
      if (box && box.hidden && open.has(k)) toggle(box.parentElement.dataset.id);
    }
    keep(anchor);
    post({ action: "expand", id: Number(String(k).replace(/^m/, "")), open: open.has(k) });
  }

  document.addEventListener("click", (e) => {
    const a = e.target.closest("a[href]");
    if (a) { e.preventDefault(); post({ action: "link", href: a.getAttribute("href") }); return; }
    const copy = e.target.closest("button.copy");
    if (copy) { post({ action: "copy", text: copy.closest(".code").querySelector("pre > code").textContent }); return; }
    const t = e.target.closest("[data-toggle]");
    if (t && !window.getSelection().toString()) { toggle(t.dataset.toggle); return; }
    const s = e.target.closest(".senate[data-thread]");
    if (s && !window.getSelection().toString()) {
      post({ action: "thread", committee: s.dataset.committee, thread: Number(s.dataset.thread) });
    }
  });
  // Nothing is dragged out of the page but text.
  document.addEventListener("dragstart", (e) => { if (e.target.closest && e.target.closest("a, img")) e.preventDefault(); });

  // Ask for the page before when within a screen and a half of the top.
  const nearTop = () => {
    if (hasOlder && !askedOlder && scroller().scrollTop < window.innerHeight * 1.5) { askedOlder = true; post({ action: "older" }); }
  };
  new ResizeObserver(() => { if (stuck) toBottom(); }).observe(document.getElementById("rows"));
  document.addEventListener("scroll", () => { stuck = measureBottom(); nearTop(); }, { passive: true });

  // MARK: Copy: plain text only, as it reads

  // What is not text: left out of a copy.
  const CHROME = ".code-head, .thinking-toggle, .tool-head .kind, .tool-head .ok, .tool-head .fail, .tool-head .run, #older, #working, .debug, .ph";
  const BLOCK = new Set(["P", "H1", "H2", "H3", "H4", "H5", "H6", "BLOCKQUOTE", "PRE", "TABLE", "UL", "OL", "HR", "DIV"]);

  // The text of a fragment: paragraphs apart, list items with their
  // markers (and task boxes as [x] / [ ]), nested lists indented, code as
  // written, a table as tab-separated rows.
  function plain(node) {
    const out = [];
    const walk = (n, indent) => {
      if (n.nodeType === Node.TEXT_NODE) {
        const inPre = n.parentElement && n.parentElement.closest("pre");
        out.push(inPre ? n.data : n.data.replace(/\s+/g, " "));
        return;
      }
      if (n.nodeType !== Node.ELEMENT_NODE && n.nodeType !== Node.DOCUMENT_FRAGMENT_NODE) return;
      const tag = n.tagName || "";
      if (tag === "BR") { out.push("\n" + indent); return; }
      if (tag === "HR") { out.push("\n\n---\n\n"); return; }
      if (tag === "TABLE") {
        out.push("\n\n");
        for (const tr of n.querySelectorAll("tr")) {
          out.push(Array.from(tr.children).map((c) => plain(c).trim()).join("\t") + "\n");
        }
        out.push("\n");
        return;
      }
      if (tag === "UL" || tag === "OL") {
        let i = Number(n.getAttribute("start") || 1);
        if (!indent) out.push("\n");
        for (const li of n.children) {
          if (li.tagName !== "LI") continue;
          let marker = tag === "OL" ? (i++) + ". " : "- ";
          const box = li.querySelector(":scope > .task-marker");
          if (box) marker = "- " + (box.dataset.state === "done" ? "[x] " : "[ ] ");
          out.push("\n" + indent + marker);
          let lead = true;
          for (const c of li.childNodes) {
            if (c === box) continue;
            // The item's text starts right after its marker.
            if (lead && c.nodeType === Node.TEXT_NODE) { out.push(c.data.replace(/^\s+/, "").replace(/\s+/g, " ")); lead = false; continue; }
            lead = false;
            if (c.nodeType === Node.ELEMENT_NODE && (c.tagName === "UL" || c.tagName === "OL")) walk(c, indent + "  ");
            else if (c.nodeType === Node.ELEMENT_NODE && c.tagName === "P") { for (const g of c.childNodes) walk(g, indent); }
            else walk(c, indent);
          }
        }
        if (!indent) out.push("\n");
        return;
      }
      const block = BLOCK.has(tag);
      if (block) out.push("\n\n");
      for (const c of n.childNodes) walk(c, indent);
      if (block) out.push("\n\n");
    };
    walk(node, "");
    return out.join("")
      .replace(/[ \t]+\n/g, "\n")
      .replace(/\n[ ]+(?=\n)/g, "\n")
      .replace(/\n{3,}/g, "\n\n")
      .trim();
  }

  // The text of some copied nodes, chrome left out.
  function copied(frag) {
    for (const c of frag.querySelectorAll(CHROME)) c.remove();
    return plain(frag);
  }

  document.addEventListener("copy", (e) => {
    const sel = window.getSelection();
    if (!sel || sel.isCollapsed) return;
    const frag = document.createDocumentFragment();
    for (let i = 0; i < sel.rangeCount; i++) frag.appendChild(sel.getRangeAt(i).cloneContents());
    e.clipboardData.setData("text/plain", copied(frag));
    e.preventDefault();
  });

  async function selectAll() {
    await renderAll();
    const range = document.createRange();
    range.selectNodeContents(main());
    const sel = window.getSelection();
    sel.removeAllRanges();
    sel.addRange(range);
  }

  // MARK: Find, over everything read (rendered first)

  let found = [], foundAt = -1, foundQuery = null;
  const highlights = window.CSS && CSS.highlights;

  function clearFind() {
    found = []; foundAt = -1; foundQuery = null;
    if (highlights) { highlights.delete("find"); highlights.delete("find-now"); }
  }

  function search(q) {
    const needle = q.toLocaleLowerCase();
    const ranges = [];
    const walker = document.createTreeWalker(main(), NodeFilter.SHOW_TEXT, {
      acceptNode: (n) => (n.parentElement && n.parentElement.closest(CHROME)) ? NodeFilter.FILTER_REJECT : NodeFilter.FILTER_ACCEPT,
    });
    for (let n = walker.nextNode(); n; n = walker.nextNode()) {
      const hay = n.data.toLocaleLowerCase();
      for (let i = hay.indexOf(needle); i >= 0; i = hay.indexOf(needle, i + needle.length)) {
        const r = document.createRange();
        r.setStart(n, i); r.setEnd(n, i + needle.length);
        ranges.push(r);
      }
    }
    return ranges;
  }

  function paint() {
    if (!highlights) return;
    highlights.set("find", new Highlight(...found));
    if (foundAt >= 0) {
      highlights.set("find-now", new Highlight(found[foundAt]));
      const box = found[foundAt].getBoundingClientRect();
      if (box.top < 60 || box.bottom > window.innerHeight - 60) {
        scroller().scrollTop += box.top - window.innerHeight / 3;
        stuck = measureBottom();
      }
    } else highlights.delete("find-now");
  }

  async function find(q, forward, again) {
    if (q === null) { clearFind(); return; }
    if (q !== foundQuery || !again) {
      await renderAll();
      found = search(q);
      foundQuery = q;
      // From what is on screen, going down.
      foundAt = found.findIndex((r) => r.getBoundingClientRect().bottom >= 0);
      if (foundAt < 0 && found.length) foundAt = 0;
    } else if (found.length) {
      foundAt = (foundAt + (forward ? 1 : -1) + found.length) % found.length;
    }
    if (foundAt >= 0) {
      const box = found[foundAt].startContainer.parentElement?.closest(".group-members");
      if (box && box.hidden) toggle(box.parentElement.dataset.id);
    }
    paint();
    post({ action: "found", count: found.length, index: found.length ? foundAt + 1 : 0 });
  }

  // A search's match: the row it is in, its blocks rendered (a tool call
  // opened), scrolled to the first match in that row, highlighted.
  async function reveal(id, q) {
    const pause = (ms) => new Promise((r) => setTimeout(r, ms));
    // A command in a group is its member ("m" + id); the group's own row
    // shares its first command's id, so the member is looked for first.
    const rowEl = () => main().querySelector('[data-id="m' + id + '"]') || main().querySelector('[data-id="' + id + '"]');
    for (let i = 0; i < 100 && !rowEl(); i++) await pause(20);
    let r = rowEl();
    if (!r) return;
    const group = r.closest(".group-members");
    if (group && group.hidden) { toggle(group.parentElement.dataset.id); r = rowEl(); }
    if (r.classList.contains("k-tool") && !open.has(r.dataset.id)) { toggle(r.dataset.id); r = rowEl(); }
    for (const d of docs.values()) {
      if (!r.contains(d.el)) continue;
      for (const [bid, b] of d.blocks) if (b.state === "placeholder" || b.state === "stale") queue(d.key, bid, true);
    }
    for (let i = 0; i < 100 && (flushing || outstanding > 0); i++) await pause(20);
    r = rowEl();
    found = q ? search(q) : [];
    foundQuery = q;
    foundAt = found.findIndex((range) => r.contains(range.startContainer));
    if (foundAt >= 0) {
      paint();
      const box = found[foundAt].getBoundingClientRect();
      scroller().scrollTop += box.top - window.innerHeight / 3;
    } else {
      scroller().scrollTop += r.getBoundingClientRect().top - window.innerHeight / 3;
    }
    stuck = measureBottom();
  }

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
      box.textContent = "updates " + totals.updates + " · blocks replaced " + totals.replaced + ", appended " + totals.appended +
        ", removed " + totals.removed + " · rendered whole " + totals.whole + ", code tails " + totals.tails +
        " · lines highlighted " + totals.highlighted + " · whole messages re-rendered 0\n\n" + lines.join("\n");
    }
    function add(line) { lines.unshift(line); if (lines.length > 16) lines.pop(); show(); }
    document.addEventListener("keydown", (e) => {
      if (e.altKey && e.code === "KeyD") { box.hidden = !box.hidden; show(); e.preventDefault(); }
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
    find,
    reveal,
    selectAll,
    renderAll,
    plainText: () => { const f = document.createDocumentFragment(); f.appendChild(main().cloneNode(true)); return copied(f); },
    theme(vars) { for (const k in vars) document.documentElement.style.setProperty("--" + k, vars[k]); },
    labels(l) { labels = Object.assign(labels, l); },
    debug(on) { document.querySelector(".debug").hidden = !on; debug.note("debug overlay on (⌥D)"); },
    scrollToRow(id, at) {
      let r = main().querySelector('[data-id="' + id + '"]') || main().querySelector('[data-id="m' + id + '"]');
      const group = r && r.closest(".group-members");
      if (group && group.hidden) { toggle(group.parentElement.dataset.id); r = main().querySelector('[data-id="m' + id + '"]'); }
      if (!r) return;
      const box = r.getBoundingClientRect();
      const y = at === "top" ? box.top - 8 : at === "bottom" ? box.bottom - window.innerHeight + 16 :
        at === "middle" ? box.top + box.height / 2 - window.innerHeight / 2 : box.top - window.innerHeight / 2;
      scroller().scrollTop += y;
      stuck = measureBottom();
    },
    // Where the reader is: the row at the top of the window and how far
    // down it sits, or the foot.
    // What was open (commands, groups, thinking) comes back with it.
    anchor() {
      const opened = Array.from(open);
      if (stuck) return { bottom: true, open: opened };
      const a = firstVisible();
      let r = a && a.el.closest(".row");
      while (r && r.parentElement !== main()) r = r.parentElement.closest(".row");
      if (!r) return { bottom: true, open: opened };
      return { id: r.dataset.id, top: r.getBoundingClientRect().top, open: opened };
    },
    // Back where the reader was; false while that row is not on the page.
    restore(a) {
      if (!a.bottom && !main().querySelector(':scope > [data-id="' + a.id + '"]')) return false;
      for (const k of a.open || []) if (!open.has(k) && rows.has(k)) toggle(k);
      if (a.bottom) { toBottom(); stuck = true; return true; }
      const r = main().querySelector(':scope > [data-id="' + a.id + '"]');
      scroller().scrollTop += r.getBoundingClientRect().top - a.top;
      stuck = measureBottom();
      return true;
    },
    scrollToTop() { scroller().scrollTop = 0; stuck = measureBottom(); nearTop(); },
    // Every group of commands and every command open.
    expandAll() {
      for (const [k, row] of Array.from(rows)) {
        if ((row.kind === "group" || row.kind === "tool") && !open.has(k)) toggle(k);
      }
    },
    openCall(callId) {
      for (const [k, row] of rows) if (row.kind === "tool" && row.tool.callId === callId && !open.has(k)) toggle(k);
    },
    // A frame with rows in it, and (when it has Markdown) some rendered.
    shown: () => new Promise((r) => requestAnimationFrame(() => r(!!document.querySelector(".row") &&
      (!document.querySelector(".blk") || !!document.querySelector(".blk:not(.ph)"))))),
    stats() { const s = { frames, slow, worst }; frames = 0; slow = 0; worst = 0; last = 0; return s; },
  };
  post({ action: "ready" });
})();
