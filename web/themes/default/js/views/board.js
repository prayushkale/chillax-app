import { h, fill, keyed } from "../dom.js";
import { send, store } from "../store.js";
import { S, nameOf, ticketOf, sprintInFlight, pastSprints, boardTickets } from "../util.js";
import { viewShell, assigneeChip, startNewSprint } from "./common.js";
import { openTicket, currentId } from "../drawer.js";
import { modal, askText, confirmBox, toast } from "../ui.js";

let els, filter = "", reqOpen = false;
// "active", "backlog" or a past sprint id. Past sprints and the backlog are read-only.
let view = "active";
try { view = localStorage.getItem("chillax.boardView") || "active"; } catch { /* storage may be blocked */ }

export async function newTicket(prefill = {}) {
  const s = S();
  const r = await modal("New ticket", (close) => {
    const kind = h("select", { "aria-label": "Type" }, ["story", "bug", "task"].map((k) => h("option", { value: k }, k)));
    const title = h("input", { "aria-label": "Title", required: true, placeholder: "What should be built?" });
    const desc = h("textarea", { rows: 4, "aria-label": "Description" });
    const ac = h("textarea", { rows: 3, "aria-label": "Acceptance criteria, one per line", placeholder: "One criterion per line" });
    const est = h("input", { type: "number", min: 0, "aria-label": "Estimate", class: "w-90" });
    const epic = h("select", { "aria-label": "Epic" }, h("option", { value: "" }, "No epic"), s.epics.map((e) => h("option", { value: e.id }, `${e.id} ${e.title}`)));
    const ok = h("button", { type: "submit", class: "btn primary", disabled: true }, "Create ticket");
    title.addEventListener("input", () => { ok.disabled = !title.value.trim(); });
    ok.addEventListener("click", () => {
      if (!title.value.trim()) return;
      close({ type: "create_ticket", kind: kind.value, title: title.value.trim(), description: desc.value,
        acceptance_criteria: ac.value.split("\n").map((l) => l.trim()).filter(Boolean),
        ...(est.value !== "" ? { estimate: Number(est.value) } : {}), ...(epic.value ? { epic: epic.value } : {}) });
    });
    return [h("label", { class: "field" }, h("span", {}, "Title"), title),
      h("div", { class: "row" }, h("label", { class: "field" }, h("span", {}, "Type"), kind), h("label", { class: "field" }, h("span", {}, "Estimate"), est), h("label", { class: "field" }, h("span", {}, "Epic"), epic)),
      h("label", { class: "field" }, h("span", {}, "Description"), desc), h("label", { class: "field" }, h("span", {}, "Acceptance criteria"), ac),
      h("div", { class: "actions" }, ok, h("button", { type: "button", class: "btn", onclick: () => close(null) }, "Cancel"))];
  });
  if (r) {
    const res = await send(r);
    const created = res?.events?.find((e) => e.type === "ticket_created");
    if (created) openTicket(created.ticket.id);
  }
}

export async function newEpic() {
  const title = await askText({ title: "New epic", label: "Epic title", confirm: "Create epic", required: true });
  if (title) send({ type: "create_epic", title, description: "" });
}

export function toggleRequest(open) {
  reqOpen = open ?? !reqOpen;
  if (els) { els.req.hidden = !reqOpen; if (reqOpen) els.reqText.focus(); }
}

/** Dev-loop facts about a ticket, for its card. */
function badges(t) {
  const s = S();
  const checks = t.checks || [];
  const out = [];
  if (t.urgent) out.push(["! urgent", "bad"]);
  const blocked = (t.blocked_by || []).some((id) => { const b = ticketOf(id); return b && b.state !== s.workflow_states.at(-1) && !(b.checks || []).includes("merged_to_dev"); });
  if (blocked) out.push(["blocked", "warn"]);
  if (t.review_requested) out.push(["testing", "warn"]);
  if (checks.includes("tests_pass")) out.push(["tests ✓", "ok"]);
  if (t.reviewer && t.state === "in_review") out.push([`review: ${nameOf(t.reviewer)}`, ""]);
  if (checks.includes("review_approved")) out.push(["approved ✓", "ok"]);
  if (checks.includes("merged_to_dev")) out.push(["merged ✓", "ok"]);
  if (t.needs_rebase) out.push(["behind dev", "warn"]);
  if (t.conflict) out.push([`conflict ${Math.min(t.conflict.attempts, 2)}/2`, "warn"]);
  if (checks.includes("qa_passed")) out.push(["QA ✓", "ok"]);
  if (checks.includes("qa_failed")) out.push(["QA ✗", "bad"]);
  if (t.found_in) out.push([`bug of ${t.found_in}`, ""]);
  if (t.pr_url) out.push([`PR #${t.pr_number}`, ""]);
  return out;
}

function card(t) {
  const el = h("div", { class: "card", tabindex: 0, role: "button", "data-id": t.id, "aria-label": `${t.id} ${t.title}`,
    onclick: () => openTicket(t.id, el), onkeydown: (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); openTicket(t.id, el); } } },
    h("div", { class: "card-top" }, h("span", { class: "id" }, t.id), h("span", { class: "push" }), assigneeChip(t.assignee)),
    h("div", { class: "ttl" }, t.title),
    h("div", { class: "meta" }, h("span", { class: `pill ${t.kind}` }, t.kind), t.estimate != null ? h("span", { class: "pill" }, `${t.estimate} pt`) : null,
      t.epic ? h("span", { class: "pill" }, t.epic) : null,
      badges(t).map(([b, tone]) => h("span", { class: `pill ${tone}` }, b))));
  return el;
}

function requestRow(r) {
  const s = S();
  const planner = s.employees.some((e) => (store.roles.find((x) => x.id === e.role)?.owns_states || []).includes(s.workflow_states[0]));
  const note = r.status === "abandoned" ? "the project manager never answered; see the Inbox"
    : r.status === "submitted" && !planner ? "hire a project manager and it will be picked up"
    : r.status === "submitted" ? "queued: waiting for the project manager"
    : r.status === "briefed" ? "the project manager is on it" : r.summary || "";
  const filed = r.tickets.map((id) => `${id}${ticketOf(id)?.assignee ? ` → ${nameOf(ticketOf(id).assignee)}` : ""}`);
  return h("div", { class: "item flush-x" },
    h("span", { class: "mono faint" }, r.id),
    h("div", { class: "grow" }, h("div", { class: "t" }, r.text.split("\n")[0]), h("div", { class: "muted" }, [note, filed.length ? `Filed: ${filed.join(", ")}` : ""].filter(Boolean).join(" · "))),
    r.start_now ? h("span", { class: "pill" }, "start now") : null,
    r.from ? h("span", { class: "pill" }, `${r.kind || "suggestion"} from ${nameOf(r.from)}`) : null,
    h("span", { class: `pill ${r.status}` }, r.status === "submitted" ? "queued" : r.status));
}

export default {
  id: "board", title: "Board", icon: "board",
  mount(root) {
    const search = h("input", { type: "search", placeholder: "Filter tickets", "aria-label": "Filter tickets", class: "w-180", oninput: (e) => { filter = e.target.value.toLowerCase(); this.update(); } });
    const reqBtn = h("button", { class: "btn", type: "button", onclick: () => toggleRequest() }, "New request");
    const picker = h("select", { "aria-label": "Sprint", class: "w-180", onchange: (e) => {
      view = e.target.value;
      try { localStorage.setItem("chillax.boardView", view); } catch { /* ignore */ }
      this.update();
    } });
    const { head, body } = viewShell("Board", picker, search, reqBtn,
      h("button", { class: "btn", type: "button", onclick: newEpic }, "New epic"),
      h("button", { class: "btn primary", type: "button", onclick: () => newTicket() }, "New ticket"));
    body.className = "vbody flush board-body";
    const reqText = h("textarea", { rows: 2, class: "grow", placeholder: "Not sure who should do it? Describe the feature, bug or idea. The project manager files it and assigns the right person.", "aria-label": "Request" });
    const now = h("input", { type: "checkbox", id: "request-now" });
    const post = async () => {
      const text = reqText.value.trim();
      if (!text) return;
      if (await send({ type: "submit_request", text, start_now: now.checked })) { reqText.value = ""; toast("Request sent. The project manager will file it and assign the work."); }
    };
    reqText.addEventListener("keydown", (e) => { if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); post(); } });
    els = {
      reqText, search, picker,
      strip: h("div", {}),
      req: h("div", { class: "reqbox", hidden: true },
        h("div", { class: "row top" }, reqText, h("button", { type: "button", class: "btn primary", onclick: post }, "Send request")),
        h("label", { class: "row muted mt-6" }, now, "Start now: the tickets join the active sprint (or open one) without waiting for approval"),
        h("div", { class: "list mt-8", id: "request-list" })),
      empty: h("div", { class: "pad empty", hidden: true }, "The board is empty. Create a ticket, send a request, or ask your Project Manager to plan a sprint in Chat."),
      board: h("div", { class: "board" }),
    };
    els.requestList = els.req.querySelector("#request-list");
    body.append(els.strip, els.req, els.empty, els.board);
    root.append(head, body);
  },
  update() {
    const s = S();
    const past = pastSprints();
    if (view !== "active" && view !== "backlog" && !past.some((p) => p.id === view)) view = "active";
    const inFlight = sprintInFlight();
    // The strip describes the sprint being viewed: the one in flight, or a past one.
    const sprint = view === "active" ? inFlight : view === "backlog" ? null : past.find((p) => p.id === view);
    const readOnly = view !== "active";
    const planning = s.planning;
    const tickets = boardTickets(view);
    const done = tickets.filter((t) => t.state === s.workflow_states.at(-1)).length;
    const opts = JSON.stringify([past.map((p) => [p.id, p.name, p.status]), view]);
    if (els.picker._sig !== opts) {
      els.picker._sig = opts;
      fill(els.picker, [h("option", { value: "active" }, "Active sprint"),
        ...past.slice().reverse().map((p) => h("option", { value: p.id }, `${p.id} ${p.name} (${p.status})`)),
        h("option", { value: "backlog" }, "Backlog")]);
      els.picker.value = view;
    }
    const sig = JSON.stringify([view, sprint, done, tickets.length, inFlight?.id, planning]);
    if (els.strip._sig !== sig) {
      els.strip._sig = sig;
      const bits = [];
      if (view === "backlog") bits.push(h("div", { class: "strip" }, h("strong", {}, "Backlog"),
        h("span", { class: "muted grow" }, `${tickets.length} unsprinted ticket${tickets.length === 1 ? "" : "s"} (read-only)`)));
      if (sprint) bits.push(h("div", { class: "strip" },
        h("strong", {}, `${sprint.id} ${sprint.name}`), h("span", { class: `pill ${sprint.status}` }, sprint.status),
        h("span", { class: "muted grow" }, (sprint.goal || "no goal") + (readOnly ? " (read-only)" : "")),
        h("span", { class: "muted" }, `${done}/${tickets.length} done`), h("div", { class: "w-120" }, h("progress", { max: tickets.length || 1, value: done })),
        !readOnly && sprint.status === "proposed" ? [h("button", { class: "btn small primary", onclick: () => send({ type: "approve_sprint", id: sprint.id }) }, "Approve sprint"),
          h("button", { class: "btn small", onclick: () => send({ type: "reject_sprint", id: sprint.id }) }, "Reject")] : null,
        !readOnly && sprint.status === "active" ? h("button", { class: "btn small danger", title: "Unfinished tickets leave the sprint and can be planned again", onclick: async () => {
          if (await confirmBox(`Close sprint ${sprint.id}?`, "Unfinished tickets leave it and can be planned again.", "Close sprint")) send({ type: "close_sprint", id: sprint.id });
        } }, "Close sprint") : null));
      if (view === "active" && !inFlight) {
        const backlog = boardTickets("backlog").length;
        bits.push(h("div", { class: "strip" },
          h("span", { class: "muted grow" }, (planning && !planning.briefed ? "Waiting for the project manager to take the planning request." : planning ? "The project manager is planning the next sprint." : "No sprint is planned.")
            + (past.length ? " Past sprints and the backlog are in the picker." : "") + (backlog ? ` ${backlog} in Backlog.` : "")),
          !planning || planning.briefed ? h("button", { class: "btn small primary", onclick: startNewSprint }, "Start a new sprint") : null));
      }
      fill(els.strip, bits);
    }
    const reqs = (s.requests || []).slice().reverse().slice(0, 6);
    keyed(els.requestList, reqs, (r) => r.id, requestRow, (r) => JSON.stringify([r, s.employees.length]));
    const live = tickets;
    els.empty.hidden = live.length > 0 || !!sprint;
    els.empty.textContent = view === "backlog" ? "The backlog is empty. Every ticket belongs to a sprint."
      : view === "active" ? "The board is empty. Create a ticket, send a request, or ask your Project Manager to plan a sprint in Chat."
      : "No tickets in this sprint.";
    const match = (t) => !filter || `${t.id} ${t.title} ${t.assignee ? nameOf(t.assignee) : ""} ${t.epic || ""}`.toLowerCase().includes(filter);
    const sel = currentId();
    // Columns are built once per workflow; only their cards are keyed.
    const wf = JSON.stringify(s.workflow_states);
    if (els.board._wf !== wf) {
      els.board._wf = wf;
      els.board._cols = new Map();
      fill(els.board, s.workflow_states.map((k, i, all) => {
        const count = h("span", { class: "col-n" }); const bodyEl = h("div", { class: "col-b" }); const none = h("p", { class: "empty-col" }, "Nothing here");
        els.board._cols.set(k, { count, bodyEl, none });
        return h("section", { class: "col", "aria-label": k.replace(/_/g, " ") }, h("div", { class: "col-h" }, h("span", { class: `col-dot ${i === 0 ? "first" : i === all.length - 1 ? "last" : "mid"}`, "aria-hidden": "true" }), h("span", { class: "col-name" }, k.replace(/_/g, " ")), count), bodyEl, none);
      }));
    }
    for (const k of s.workflow_states) {
      const c = els.board._cols.get(k);
      const list = live.filter((t) => t.state === k && match(t));
      c.count.textContent = list.length;
      c.none.hidden = list.length > 0;
      keyed(c.bodyEl, list, (t) => t.id, card, (t) => JSON.stringify([t, badges(t), t.id === sel, S().employees.map((e) => e.name)]));
      c.bodyEl.querySelectorAll(".card").forEach((el) => el.classList.toggle("selected", el.dataset.id === sel));
    }
  },
};
