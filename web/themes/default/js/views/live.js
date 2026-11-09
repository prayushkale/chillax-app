import { h, fill, keyed } from "../dom.js";
import { S, employee, nameOf, statusEl, workingOn, currentTicket, transcript, ticketOf } from "../util.js";
import { viewShell } from "./common.js";
import { transcriptView } from "../transcript.js";
import { openTicket } from "../drawer.js";
import { go } from "../nav.js";

let els, who = null;

export default {
  id: "live", title: "Live", icon: "live",
  mount(root) {
    const { head, body } = viewShell("Live");
    body.className = "twocol";
    body.classList.add("flush", "clip");
    els = {
      people: h("div", { class: "people", role: "list", "aria-label": "Employees" }),
      none: h("p", { class: "empty pad" }, "Hire someone first."),
      head: h("div", { class: "live-head" }),
      tr: transcriptView(),
      plan: h("aside", { class: "plan", "aria-label": "Plan" }),
    };
    body.append(h("div", { class: "col-fill" }, els.people, els.none),
      h("div", { class: "pane" }, els.head, h("div", { class: "livebody" }, els.tr.el, els.plan)));
    root.append(head, body);
  },
  open(id) { this.select(id); },
  select(id) { if (id && id !== who) { who = id; els.tr.reset(); } },
  update() {
    const s = S();
    if (!who || !employee(who)) { who = null; this.select((s.employees.find((e) => currentTicket(e.id)) || s.employees[0])?.id); }
    els.none.hidden = s.employees.length > 0;
    keyed(els.people, s.employees, (e) => e.id, (e) => h("button", { class: "person", type: "button", role: "listitem", "aria-current": String(e.id === who),
        onclick: () => { this.select(e.id); this.update(); } }, h("strong", {}, e.name), h("small", {}, workingOn(e))),
      (e) => JSON.stringify([e, e.id === who, workingOn(e)]));
    const e = employee(who);
    if (!e) { fill(els.head, h("p", { class: "empty" }, "Nobody to watch yet.")); fill(els.plan); els.tr.update([], "", "", ""); return; }
    const t = currentTicket(e.id);
    const live = s.live?.[e.id] || { plan: [], tools: [] };
    const idx = s.employees.indexOf(e) + 1;
    const stateIdx = t ? s.workflow_states.indexOf(t.state) : -1;
    fill(els.head,
      h("div", { class: "row" }, h("strong", { class: "h-title" }, e.name), h("span", { class: "muted" }, e.role_title),
        h("span", { class: "faint" }, `agent ${idx} of ${s.employees.length} · ${e.backend}${e.model ? ` · ${e.model}` : ""}${e.effort ? ` · ${e.effort}` : ""}`), statusEl(e.status),
        h("button", { class: "btn small push", onclick: () => go("chat", e.id) }, "Chat")),
      t ? [
        h("div", { class: "row" }, h("a", { href: "#", onclick: (ev) => { ev.preventDefault(); openTicket(t.id, ev.currentTarget); } }, t.id), h("span", {}, t.title),
          t.branch ? h("span", { class: "pill mono" }, t.branch) : null,
          t.pr_url && /^https?:\/\//.test(t.pr_url) ? h("a", { href: t.pr_url, target: "_blank", rel: "noopener noreferrer" }, `PR #${t.pr_number}`) : null,
          (t.checks || []).includes("tests_pass") ? h("span", { class: "pill ok" }, "tests passed") : null,
          t.reviewer ? h("span", { class: "pill" }, `reviewer ${nameOf(t.reviewer)}`) : null),
        h("div", { class: "flow", "aria-label": "Workflow" }, s.workflow_states.map((k, i) => h("span", { class: i === stateIdx ? "cur" : i < stateIdx ? "past" : "" }, k.replace(/_/g, " ")))),
      ] : h("p", { class: "muted" }, "Not on a ticket."));
    fill(els.plan, h("h4", {}, "Plan"), live.plan.length
      ? h("ul", {}, live.plan.map((p) => h("li", { class: p.status }, p.content))) : h("p", { class: "faint" }, "No plan reported yet."));
    els.tr.update(transcript(e.id), e.name, ["working", "starting"].includes(e.status) ? `${e.name} is working…` : "", e.last_progress && !["working", "starting"].includes(e.status) ? "Context cleared — idle" : "No activity yet. Message them from Chat or let the dispatcher assign work.");
  },
};
