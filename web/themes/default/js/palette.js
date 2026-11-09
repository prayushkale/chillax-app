// Command palette (Ctrl/Cmd+K): views, actions, tickets and people in one list.
import { h, fill, $ } from "./dom.js";
import { send } from "./store.js";
import { S } from "./util.js";
import { go, views } from "./nav.js";
import { openTicket } from "./drawer.js";
import { newTicket, newEpic, toggleRequest } from "./views/board.js";

let items = [], sel = 0, opener = null;

function build() {
  const s = S();
  const out = [];
  for (const v of views) out.push({ label: `Go to ${v.title}`, group: "View", run: () => go(v.id) });
  out.push({ label: "New ticket", group: "Action", run: () => { go("board"); newTicket(); } });
  out.push({ label: "New epic", group: "Action", run: () => { go("board"); newEpic(); } });
  out.push({ label: "New request to the project manager", group: "Action", run: () => { go("board"); toggleRequest(true); } });
  out.push({ label: "Refresh agents", group: "Action", when: s.employees.length > 0, run: () => send({ type: "kick" }) });
  out.push({ label: s.dispatch_paused ? "Resume the dev loop" : "Pause the dev loop", group: "Action", run: () => send({ type: s.dispatch_paused ? "resume_dispatch" : "pause_dispatch" }) });
  out.push({ label: "Promote dev to QA", group: "Action", run: () => send({ type: "promote_to_qa" }) });
  for (const e of s.employees) {
    out.push({ label: `Chat with ${e.name}`, group: "Person", run: () => go("chat", e.id) });
    out.push({ label: `Watch ${e.name} live`, group: "Person", run: () => go("live", e.id) });
  }
  for (const t of s.tickets) if (!t.dropped) out.push({ label: `${t.id} ${t.title}`, group: "Ticket", run: () => { go("board"); openTicket(t.id, opener); } });
  return out.filter((i) => i.when !== false);
}

function render() {
  const q = $("#palette-input").value.trim().toLowerCase().split(/\s+/).filter(Boolean);
  items = build().filter((i) => q.every((w) => i.label.toLowerCase().includes(w))).slice(0, 40);
  sel = Math.min(sel, Math.max(0, items.length - 1));
  fill($("#palette-list"), items.length ? items.map((it, i) => h("li", { id: `pi-${i}`, role: "option", "aria-selected": String(i === sel),
      onclick: () => run(i), onmousemove: () => { if (sel !== i) { sel = i; mark(); } } }, it.label, h("span", { class: "grp" }, it.group)))
    : h("li", { class: "faint", role: "option" }, "No matches"));
  $("#palette-input").setAttribute("aria-activedescendant", items.length ? `pi-${sel}` : "");
}
function mark() {
  [...$("#palette-list").children].forEach((li, i) => { li.setAttribute("aria-selected", String(i === sel)); if (i === sel) li.scrollIntoView({ block: "nearest" }); });
  $("#palette-input").setAttribute("aria-activedescendant", `pi-${sel}`);
}
function run(i) {
  const it = items[i];
  if (!it) return;
  closePalette();
  it.run();
}

export function openPalette() {
  const dlg = $("#palette");
  if (dlg.open) return;
  opener = document.activeElement;
  $("#palette-input").value = "";
  sel = 0;
  dlg.showModal();
  render();
  $("#palette-input").focus();
}
export function closePalette() { $("#palette").close(); }

export function bindPalette() {
  const input = $("#palette-input");
  input.addEventListener("input", () => { sel = 0; render(); });
  input.addEventListener("keydown", (e) => {
    if (e.key === "ArrowDown") { e.preventDefault(); sel = Math.min(items.length - 1, sel + 1); mark(); }
    else if (e.key === "ArrowUp") { e.preventDefault(); sel = Math.max(0, sel - 1); mark(); }
    else if (e.key === "Enter") { e.preventDefault(); run(sel); }
  });
  $("#palette").addEventListener("click", (e) => { if (e.target === $("#palette")) closePalette(); });
  $("#palette-open").addEventListener("click", openPalette);
}
