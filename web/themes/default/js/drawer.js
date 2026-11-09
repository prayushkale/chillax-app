// The ticket side drawer: one place to read and act on a ticket, from anywhere.
import { h, fill, keyed, $ } from "./dom.js";
import { send } from "./store.js";
import { S, nameOf, ticketOf, droppable, employee, statusEl, relTime } from "./util.js";
import { askText } from "./ui.js";
import { commentBox, attachmentLink } from "./attach.js";

let cur = null; // { id, opener, dyn, comment, form, editing }

export const isOpen = () => !!cur;
export const currentId = () => cur?.id || null;

export function openTicket(id, opener) {
  if (cur && cur.id === id) { $("#drawer-body").scrollTop = 0; return; }
  const drawer = $("#drawer");
  const dyn = h("div", { class: "drawer-dyn" });
  const form = editForm();
  cur = { id, opener: opener || document.activeElement, dyn, comment: commentBox(id), form, editing: false };
  fill($("#drawer-body"), dyn, form.el, h("div", { class: "dsec" }, h("h4", {}, "Add a comment"), cur.comment));
  drawer.hidden = false;
  updateDrawer();
  document.dispatchEvent(new CustomEvent("drawer-changed"));
  $("#drawer-close").focus();
}

export function closeDrawer() {
  if (!cur) return;
  const opener = cur.opener;
  cur = null;
  $("#drawer").hidden = true;
  if (opener && document.contains(opener)) opener.focus();
  document.dispatchEvent(new CustomEvent("drawer-changed"));
}

const sec = (key, data, build) => ({ key, sig: JSON.stringify(data), build });
const link = (id) => h("a", { href: "#", onclick: (e) => { e.preventDefault(); openTicket(id); } }, id);

function editForm() {
  const title = h("input", { "aria-label": "Title" });
  const desc = h("textarea", { rows: 5, "aria-label": "Description" });
  const ac = h("textarea", { rows: 4, "aria-label": "Acceptance criteria, one per line" });
  const est = h("input", { type: "number", min: 0, "aria-label": "Estimate", class: "w-90" });
  const epic = h("select", { "aria-label": "Epic" });
  const el = h("form", { class: "dsec", hidden: true, onsubmit: async (e) => {
    e.preventDefault();
    const t = ticketOf(cur.id);
    const body = { type: "update_ticket", id: t.id };
    if (title.value.trim() && title.value.trim() !== t.title) body.title = title.value.trim();
    if (desc.value !== t.description) body.description = desc.value;
    const lines = ac.value.split("\n").map((l) => l.trim()).filter(Boolean);
    if (JSON.stringify(lines) !== JSON.stringify(t.acceptance_criteria)) body.acceptance_criteria = lines;
    if (est.value !== "" && Number(est.value) !== t.estimate) body.estimate = Number(est.value);
    if (epic.value && epic.value !== (t.epic || "")) body.epic = epic.value;
    if (Object.keys(body).length === 2) { cur.editing = false; el.hidden = true; updateDrawer(); return; }
    if (await send(body)) { cur.editing = false; el.hidden = true; updateDrawer(); }
  } },
    h("h4", {}, "Edit ticket"),
    h("div", { class: "form flush" },
      h("label", { class: "field" }, h("span", {}, "Title"), title),
      h("label", { class: "field" }, h("span", {}, "Description"), desc),
      h("label", { class: "field" }, h("span", {}, "Acceptance criteria (one per line)"), ac),
      h("div", { class: "row" },
        h("label", { class: "field" }, h("span", {}, "Estimate (points)"), est),
        h("label", { class: "field" }, h("span", {}, "Epic"), epic)),
      h("div", { class: "actions" },
        h("button", { type: "submit", class: "btn primary small" }, "Save"),
        h("button", { type: "button", class: "btn small", onclick: () => { cur.editing = false; el.hidden = true; updateDrawer(); } }, "Cancel"))));
  return {
    el,
    open(t) {
      title.value = t.title; desc.value = t.description || ""; ac.value = (t.acceptance_criteria || []).join("\n");
      est.value = t.estimate ?? "";
      fill(epic, h("option", { value: "" }, t.epic ? "Keep current" : "No epic"), S().epics.map((e) => h("option", { value: e.id, selected: false }, `${e.id} ${e.title}`)));
      el.hidden = false; title.focus();
    },
  };
}

export function updateDrawer() {
  if (!cur) return;
  const t = ticketOf(cur.id);
  if (!t) { fill($("#drawer-title"), `${cur.id} no longer exists`); fill(cur.dyn); return; }
  const s = S();
  const epic = s.epics.find((e) => e.id === t.epic);
  fill($("#drawer-title"), t.urgent ? h("span", { class: "urgent-mark", title: "Urgent: taken up first" }, "! ") : null, h("span", { class: "mono faint" }, t.id), " ", t.title);

  const failures = (s.failures || []).filter((f) => f.ticket === t.id);
  const bugs = s.tickets.filter((b) => b.found_in === t.id);
  const stakeholderMoves = (s.workflow_transitions || []).filter((x) => x.from === t.state && !x.chillax_owned);
  const canAssign = !t.dropped && t.state !== s.workflow_states.at(-1);
  const people = s.employees.map((e) => [e.id, e.name, e.role_title]);

  const sections = [
    sec("facts", [t.kind, t.state, t.estimate, epic, t.assignee, t.sprint, t.branch, t.reviewer, [...(t.checks || [])], t.needs_rebase, t.dropped, t.external, t.pr_url, t.pr_number, t.found_in, people], () =>
      h("div", { class: "dsec" },
        h("dl", { class: "facts" },
          h("dt", {}, "Type"), h("dd", {}, h("span", { class: `pill ${t.kind}` }, t.kind), t.dropped ? h("span", { class: "pill bad" }, "dropped") : null, t.urgent ? h("span", { class: "pill bad" }, "! urgent") : null),
          h("dt", {}, "State"), h("dd", {}, t.state.replace(/_/g, " ")),
          t.estimate != null ? [h("dt", {}, "Estimate"), h("dd", {}, `${t.estimate} pt`)] : null,
          epic ? [h("dt", {}, "Epic"), h("dd", {}, `${epic.id} ${epic.title}`)] : null,
          t.sprint ? [h("dt", {}, "Sprint"), h("dd", {}, t.sprint)] : null,
          t.assignee ? [h("dt", {}, "Assignee"), h("dd", {}, nameOf(t.assignee))] : null,
          t.reviewer ? [h("dt", {}, "Reviewer"), h("dd", {}, nameOf(t.reviewer))] : null,
          t.branch ? [h("dt", {}, "Branch"), h("dd", {}, h("span", { class: "mono" }, t.branch))] : null,
          t.pr_url ? [h("dt", {}, "Pull request"), h("dd", {}, /^https?:\/\//.test(t.pr_url) ? h("a", { href: t.pr_url, target: "_blank", rel: "noopener noreferrer" }, `#${t.pr_number}`) : `#${t.pr_number}`)] : null,
          t.external ? [h("dt", {}, "Tracker"), h("dd", {}, t.external.key || t.external.id)] : null,
          (t.checks || []).length ? [h("dt", {}, "Checks"), h("dd", {}, [...t.checks].map((c) => h("span", { class: `pill ${c.endsWith("failed") ? "bad" : "ok"}` }, c.replace(/_/g, " "))))] : null,
          t.needs_rebase ? [h("dt", {}, "Branch"), h("dd", {}, h("span", { class: "pill warn" }, "behind dev"))] : null,
          t.found_in ? [h("dt", {}, "Bug of"), h("dd", {}, link(t.found_in))] : null,
          t.dropped ? [h("dt", {}, "Dropped"), h("dd", {}, t.dropped)] : null))),

    sec("actions", [t.id, t.state, t.assignee, t.dropped, t.urgent, canAssign, droppable(t), cur.editing, stakeholderMoves, people], () => {
      if (t.dropped) return null;
      return h("div", { class: "dsec" }, h("h4", {}, "Actions"),
        h("div", { class: "actions" },
          h("button", { class: "btn small", type: "button", onclick: () => { cur.editing = true; cur.form.open(t); updateDrawer(); }, disabled: cur.editing, title: cur.editing ? "Already editing" : "" }, "Edit"),
          canAssign ? h("select", { "aria-label": "Assign to", onchange: (e) => send({ type: "assign_ticket", id: t.id, assignee: e.target.value || undefined }) },
            h("option", { value: "" }, "Unassigned"),
            people.map(([id, name, role]) => h("option", { value: id, selected: id === t.assignee }, `${name} · ${role}`))) : null,
          canAssign ? h("button", { class: "btn small", type: "button", title: t.urgent ? "Back to normal order" : "Take this up before other work (sprint approval and blockers still apply)", onclick: () => send({ type: "prioritize_ticket", id: t.id, urgent: !t.urgent }) }, t.urgent ? "Clear urgent" : "Mark urgent") : null,
          stakeholderMoves.map((m) => h("button", { class: "btn small", type: "button", title: m.requires?.length ? `Needs: ${m.requires.join(", ")}` : "", onclick: () => send({ type: "transition_ticket", id: t.id, to: m.to }) }, `Move to ${m.to.replace(/_/g, " ")}`)),
          droppable(t) ? h("button", { class: "btn small danger", type: "button", title: "Won't do: leaves the board and its sprint, is unassigned and its agents stop", onclick: async () => {
            const reason = await askText({ title: `Drop ${t.id}`, label: "Why won't this be done?", confirm: "Drop ticket", required: true, danger: true, multiline: true });
            if (reason) send({ type: "drop_ticket", id: t.id, reason });
          } }, "Drop") : h("span", { class: "faint", title: "Code is in dev or the ticket is done; file a bug instead" }, "Can't be dropped")));
    }),

    t.description ? sec("desc", t.description, () => h("div", { class: "dsec" }, h("h4", {}, "Description"), h("p", { class: "desc-text" }, t.description))) : null,
    t.acceptance_criteria.length ? sec("ac", t.acceptance_criteria, () => h("div", { class: "dsec" }, h("h4", {}, "Acceptance criteria"), h("ul", { class: "plain" }, t.acceptance_criteria.map((c) => h("li", {}, "☐ ", c))))) : null,

    t.dropped ? null : sec("blockers", [t.blocked_by, s.tickets.map((x) => [x.id, x.state])], () => {
      const input = h("input", { placeholder: "T-4", "aria-label": "Add a blocker by id", class: "w-90" });
      const set = (ids) => send({ type: "set_blockers", id: t.id, blocked_by: ids });
      const add = () => { const id = input.value.trim(); if (id && !(t.blocked_by || []).includes(id)) set([...(t.blocked_by || []), id]); input.value = ""; };
      input.addEventListener("keydown", (e) => { if (e.key === "Enter") { e.preventDefault(); add(); } });
      return h("div", { class: "dsec" }, h("h4", {}, "Blocked by"),
        h("div", { class: "chips" }, (t.blocked_by || []).length ? t.blocked_by.map((id) => h("span", { class: "chip" }, link(id), ` ${ticketOf(id)?.state?.replace(/_/g, " ") || "unknown"}`,
          h("button", { type: "button", "aria-label": `Remove blocker ${id}`, title: "Remove", onclick: () => set(t.blocked_by.filter((x) => x !== id)) }, "×"))) : h("span", { class: "faint" }, "Nothing blocks this ticket.")),
        h("div", { class: "row mt-6" }, input, h("button", { class: "btn small", type: "button", onclick: add }, "Add")));
    }),

    failures.length || bugs.length ? sec("links", [failures, bugs.map((b) => [b.id, b.state])], () => h("div", { class: "dsec" }, h("h4", {}, "Failures and bugs"),
      failures.map((f) => h("p", {}, h("span", { class: `pill ${f.status === "filed" ? "warn" : f.status === "resolved" ? "ok" : "bad"}` }, f.status), " ",
        `${f.class.replace(/_/g, " ")} on ${f.subject}`, f.occurrences > 1 ? ` · ${f.occurrences}×` : "", f.bug ? [" · bug ", link(f.bug)] : "",
        f.output ? h("pre", { class: "out" }, f.output) : null)),
      bugs.map((b) => h("p", {}, "Bug ", link(b.id), ` ${b.title} `, h("span", { class: "pill" }, b.state.replace(/_/g, " ")))))) : null,

    sec("comments", t.comments, () => h("div", { class: "dsec" }, h("h4", {}, `Comments (${t.comments.length})`),
      t.comments.length ? t.comments.map((c) => h("div", { class: "comment" }, h("span", { class: "by" }, c.author), c.body,
        c.attachments?.length ? h("div", { class: "thumbs" }, c.attachments.map(attachmentLink)) : null)) : h("p", { class: "faint" }, "No comments yet."))),
  ].filter(Boolean);

  keyed(cur.dyn, sections, (x) => x.key, (x) => x.build() || h("div", { hidden: true }), (x) => x.sig);
}
