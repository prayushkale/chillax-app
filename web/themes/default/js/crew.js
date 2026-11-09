// The crew strip under the brand: one chip per hire (ids and names only, never ticket
// titles). Mirrors core::notify::hire_summaries.
import { h, fill, $ } from "./dom.js";
import { store, subscribe } from "./store.js";
import { S } from "./util.js";

const MAX = 8;
const RANK = { blocked: 0, working: 1, failed: 2 };
// Hire state -> the existing .status dot class.
const DOT = { working: "working", blocked: "waiting_on_stakeholder", failed: "failed", idle: "idle", offline: "offline", stopped: "offline" };

/** Precedence: stopped, failed, waiting on you (status or an inbox item), working, offline, idle. */
export function hireState(e) {
  if (e.stopped) return "stopped";
  if (e.status === "failed") return "failed";
  if (e.status === "waiting_on_stakeholder" || S().inbox.some((i) => i.employee === e.id && (i.type === "permission" || i.type === "question"))) return "blocked";
  if (e.turn_open || e.reply_pending || e.status === "working" || e.status === "starting") return "working";
  return e.status === "offline" ? "offline" : "idle";
}

/** First assigned ticket that is neither finished nor dropped. */
function ticketId(e) {
  const last = S().workflow_states[S().workflow_states.length - 1];
  return S().tickets.find((t) => t.assignee === e.id && !t.dropped && t.state !== last)?.id;
}

export function crew() {
  return S().employees
    .map((e) => ({ id: e.id, name: e.name, state: hireState(e), ticket: ticketId(e) }))
    .sort((a, b) => (RANK[a.state] ?? 3) - (RANK[b.state] ?? 3));
}

let last = "";
function render() {
  const el = $("#crew");
  if (!el || !store.state) return;
  const hires = crew();
  const sig = JSON.stringify(hires);
  if (sig === last) return;
  last = sig;
  el.hidden = hires.length === 0;
  const chips = hires.slice(0, MAX).map((c) => {
    const what = c.ticket ? `${c.state} ${c.ticket}` : c.state;
    return h("div", { class: "crew-chip", title: `${c.name}: ${what}` },
      h("span", { class: `status ${DOT[c.state]}`, "aria-hidden": "true" }),
      h("span", { class: "crew-name" }, c.name),
      h("span", { class: "crew-what" }, what));
  });
  if (hires.length > MAX) chips.push(h("div", { class: "crew-more", title: `${hires.length - MAX} more` }, `+${hires.length - MAX}`));
  fill(el, ...chips);
}

subscribe((kind) => { if (kind === "state") render(); });
