// A Claude-Code-style transcript: messages, and tool calls as collapsible rows.
import { h, keyed } from "./dom.js";

const KIND_GLYPH = { read: "▤", edit: "✎", delete: "⌫", move: "↦", search: "⌕", execute: "$", think: "…", fetch: "↓", switch_mode: "⇄", other: "⚙" };
const STATUS_TEXT = { completed: "done", failed: "failed", pending: "pending", in_progress: "running" };
const MAX_DIFF_LINES = 120;

function lineDiff(before, after) {
  const a = before ? before.split("\n") : [];
  const b = (after || "").split("\n");
  if (a.at(-1) === "") a.pop();
  if (b.at(-1) === "") b.pop();
  let s = 0;
  while (s < a.length && s < b.length && a[s] === b[s]) s++;
  let ea = a.length, eb = b.length;
  while (ea > s && eb > s && a[ea - 1] === b[eb - 1]) { ea--; eb--; }
  return [...a.slice(s, ea).map((t) => ["-", t]), ...b.slice(s, eb).map((t) => ["+", t])];
}

function contentEl(c) {
  if (c.type === "diff") {
    const lines = lineDiff(c.old, c.new);
    return h("pre", { class: "diff" }, h("span", { class: "path" }, c.path),
      lines.slice(0, MAX_DIFF_LINES).map(([sign, t]) => h("span", { class: sign === "+" ? "add" : "del" }, `${sign} ${t}`)),
      lines.length > MAX_DIFF_LINES ? h("span", { class: "muted" }, `… ${lines.length - MAX_DIFF_LINES} more lines`) : null);
  }
  return h("pre", { class: "out" }, c.type === "terminal" ? c.output : c.text);
}

function toolEl(t, old, openByDefault) {
  const body = (t.content || []).length ? h("div", { class: "body" }, t.content.map(contentEl)) : null;
  const el = h("details", { class: `tool ${t.status}` },
    h("summary", {},
      h("span", { class: "glyph", "aria-hidden": "true" }, KIND_GLYPH[t.tool_kind] || KIND_GLYPH.other),
      h("span", { class: "title" }, t.title),
      t.tool_kind ? h("span", { class: "st" }, t.tool_kind) : null,
      h("span", { class: "st" }, STATUS_TEXT[t.status] || t.status || "")),
    body);
  el.open = old ? old.open : openByDefault && !!body;
  return el;
}

/** Create a transcript container; call `update(items, who, working)` on every state change. */
export function transcriptView() {
  const root = h("div", { class: "transcript", role: "log", "aria-live": "off", tabindex: "0" });
  const list = h("div", { class: "contents" });
  const tail = h("div", { class: "typing" });
  root.append(list, tail);
  let pinned = true;
  root.addEventListener("scroll", () => { pinned = root.scrollHeight - root.scrollTop - root.clientHeight < 48; });
  return {
    el: root,
    update(items, who, workingLabel, emptyText) {
      const lastWith = items.findLastIndex((i) => i.tool?.content?.length);
      const entries = items.map((it, i) => it.tool
        ? { key: `t:${it.tool.tool_call_id}`, it, open: i === lastWith }
        : { key: it.key, it, open: false });
      keyed(list, entries, (e) => e.key, (e, old) => {
        if (e.it.tool) return toolEl(e.it.tool, old, e.open);
        const { speaker, text } = e.it.msg;
        const label = speaker.kind === "agent" ? who : speaker.kind === "teammate" ? speaker.name : speaker.label ?? null;
        const long = text.length > 700;
        const body = long ? h("details", { class: "long" }, h("summary", {}, text.slice(0, 160).replace(/\s+/g, " "), "… ", h("span", { class: "faint" }, `(${text.length} characters, expand)`)), text) : text;
        return h("div", { class: `msg ${speaker.kind}` }, label ? h("span", { class: "who" }, label) : null, body);
      }, (e) => e.it.tool ? JSON.stringify(e.it.tool) : JSON.stringify(e.it.msg));
      if (!items.length && emptyText) { if (!root._empty) { root._empty = h("p", { class: "empty" }, emptyText); root.prepend(root._empty); } root._empty.textContent = emptyText; }
      else if (root._empty) { root._empty.remove(); root._empty = null; }
      tail.textContent = workingLabel || "";
      if (pinned) root.scrollTop = root.scrollHeight;
    },
    reset() { pinned = true; list._keyed = new Map(); list.replaceChildren(); },
  };
}
