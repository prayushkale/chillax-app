import { h, keyed } from "../dom.js";
import { send } from "../store.js";
import { S, nameOf, ticketOf } from "../util.js";
import { viewShell, ticketLink } from "./common.js";
import { go } from "../nav.js";

let els;

function body(item) {
  const s = S();
  switch (item.type) {
    case "permission":
      return [h("span", { class: "kind" }, "Permission"), h("div", {}, h("strong", {}, nameOf(item.employee)), ` wants to: ${item.title}`),
        h("div", { class: "actions" },
          item.choices.map((c) => h("button", { class: `btn small ${c.kind.startsWith("allow") ? "primary" : ""}`, onclick: () => send({ type: "resolve_permission", id: item.id, choice: c.id }) }, c.name)),
          h("button", { class: "btn small", onclick: () => send({ type: "resolve_permission", id: item.id }) }, "Cancel"))];
    case "question": {
      const area = h("textarea", { rows: 3, placeholder: "Your answer…", "aria-label": "Your answer" });
      const answer = () => area.value.trim() && send({ type: "answer_question", id: item.id, answer: area.value });
      area.addEventListener("keydown", (e) => { if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) { e.preventDefault(); answer(); } });
      return [h("span", { class: "kind" }, "Question"), h("div", {}, h("strong", {}, nameOf(item.employee)), ` asks: ${item.question}`), area,
        h("div", { class: "actions" }, h("button", { class: "btn small primary", onclick: answer, title: "Ctrl or Cmd + Enter" }, "Answer"))];
    }
    case "sprint_gate": {
      const sprint = s.sprints.find((x) => x.id === item.sprint);
      const tickets = (sprint?.tickets || []).map(ticketOf).filter((t) => t && !t.dropped);
      const points = tickets.reduce((sum, t) => sum + (t.estimate || 0), 0);
      return [h("span", { class: "kind" }, "Sprint approval"), h("div", {}, h("strong", {}, `${sprint?.id} ${sprint?.name}`), sprint?.goal ? ` — ${sprint.goal}` : ""),
        h("ul", { class: "plain" }, tickets.map((t) => h("li", {}, ticketLink(t), ` ${t.title}`, t.estimate != null ? ` (${t.estimate} pt)` : ""))),
        h("p", { class: "muted" }, `${tickets.length} tickets · ${points} points`),
        h("div", { class: "actions" }, h("button", { class: "btn small primary", onclick: () => send({ type: "approve_sprint", id: item.sprint }) }, "Approve"),
          h("button", { class: "btn small", onclick: () => send({ type: "reject_sprint", id: item.sprint }) }, "Reject"))];
    }
    case "release_gate": {
      const r = s.releases?.find((x) => x.id === item.release);
      return [h("span", { class: "kind" }, "Release approval"),
        r ? [h("div", {}, h("strong", {}, `Release ${r.version}`), ` · ${r.tickets.length} tickets${r.known_issues?.length ? `, ${r.known_issues.length} known issues` : ""}`),
            h("div", { class: "actions" }, h("button", { class: "btn small primary", onclick: () => go("environments") }, "Review notes and approve"))]
          : h("p", { class: "muted" }, `Release ${item.release} is no longer waiting.`)];
    }
    case "env_alert":
      return [h("span", { class: "kind" }, "Environment failed"), h("div", {}, h("strong", {}, `${item.env} ${item.step}`), " failed. Fix the cause, then retry."),
        item.output_tail ? h("pre", { class: "out" }, item.output_tail) : null,
        h("div", { class: "actions" }, h("button", { class: "btn small", onclick: () => send({ type: "retry_env", env: item.env, step: item.step }) }, `Retry ${item.env} ${item.step}`))];
    case "alert":
      return [h("span", { class: "kind" }, "Needs your attention"), h("div", {}, h("strong", {}, item.title)),
        item.detail ? h("p", { class: "desc-text" }, item.detail) : null,
        h("div", { class: "actions" }, h("button", { class: "btn small", onclick: () => send({ type: "dismiss_alert", id: item.id }) }, "Dismiss"))];
    default: // A newer Chillax may add items this theme does not know; show them, never guess an action.
      return [h("span", { class: "kind" }, item.type || "Inbox"), h("div", {}, item.id)];
  }
}

/** What an item's card shows beyond the item itself, so it re-renders when that changes. */
function context(item) {
  const s = S();
  if (item.type === "sprint_gate") { const sp = s.sprints.find((x) => x.id === item.sprint); return [sp, (sp?.tickets || []).map(ticketOf)]; }
  if (item.type === "release_gate") return s.releases?.find((x) => x.id === item.release);
  if (item.type === "permission" || item.type === "question") return nameOf(item.employee);
  return null;
}

export default {
  id: "inbox", title: "Inbox", icon: "inbox",
  mount(root) {
    const { head, body } = viewShell("Inbox");
    els = { list: h("div", { class: "stack" }), empty: h("div", { class: "panel empty-state stack" }, h("strong", {}, "Inbox zero. Chill."), h("p", { class: "muted" }, "Permission requests, questions and approvals land here when an agent needs you.")) };
    body.append(els.list, els.empty);
    root.append(head, body);
  },
  update() {
    const items = S().inbox;
    els.empty.hidden = items.length > 0;
    keyed(els.list, items, (i) => i.id, (i) => h("div", { class: "panel panel-b ibox" }, body(i)), (i) => JSON.stringify([i, context(i)]));
  },
};
