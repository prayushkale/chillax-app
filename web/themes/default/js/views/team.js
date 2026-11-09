import { h, fill, keyed, $ } from "../dom.js";
import { store, send, apiJson } from "../store.js";
import { S, statusEl, workingOn, nameOf } from "../util.js";
import { viewShell } from "./common.js";
import { go } from "../nav.js";
import { agentPicker, ensureAgent, agentFields, agentList } from "../agents.js";
import { toast, confirmBox, modal } from "../ui.js";
import { loadCatalog, bindCatalog, openNewRole } from "../review.js";

let els, mode = "list", picker = null, pickerKey = "", roleCount = -1;
try { mode = localStorage.getItem("chillax.team") || "list"; } catch { /* storage blocked */ }

const fire = async (e) => {
  if (await confirmBox(`Fire ${e.name}?`, "Their session ends and their tickets return to the backlog.", "Fire")) send({ type: "let_go", employee: e.id });
};

/** Hire another employee just like this one; the name is picked for them and the hiring defaults stay put. */
const duplicate = (e) => send({
  type: "hire_employee", role: e.role, backend: e.backend, keep_defaults: true,
  ...(e.model ? { model: e.model } : {}), ...(e.effort ? { effort: e.effort } : {}),
});

const agentText = (a) => [a.backend, a.model, a.effort].filter(Boolean).join(" · ");
/** Shown next to the agent while a change waits for the end of the current reply. */
const pendingBadge = (e) => e.pending_agent ? h("span", { class: "pill warn", title: `Switches to ${agentText(e.pending_agent)} when the current reply ends` }, "pending") : null;

/** Pick another agent, model and effort for an existing hire; a busy employee gets it queued until the reply ends. */
async function changeAgent(e) {
  await modal(`Change agent: ${e.name}`, (close) => {
    const p = agentPicker({ backend: e.backend, model: e.model, effort: e.effort });
    const save = async () => {
      const agent = p.value;
      if (agent.backend) { try { await ensureAgent(agent.backend); } catch (err) { toast(err.message); return; } }
      // The server sets model and effort exactly as given (absent = the agent's default).
      if (await send({ type: "set_employee_agent", employee: e.id, when_idle: true, ...agentFields(agent) })) close(true);
    };
    return [h("p", { class: "muted" }, "Their running sessions end and start again on the new agent. If they are replying, the change waits until the reply ends."),
      h("div", { class: "field" }, h("span", {}, "Agent, model and effort"), h("div", { class: "row" }, p.el)),
      h("div", { class: "actions" },
        h("button", { type: "button", class: "btn primary", onclick: save }, "Change agent"),
        h("button", { type: "button", class: "btn", onclick: () => close(null) }, "Cancel"))];
  });
}

/** Change how many agents work at once, up to what this machine fits. */
async function saveLimit(input) {
  try {
    await apiJson("/budget/max-concurrent-agents", { method: "PUT", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ n: Number(input.value) }) });
  } catch (err) { toast(err.message); }
}

function rosterRow(e) {
  const assigned = S().tickets.filter((t) => t.assignee === e.id && !t.dropped).map((t) => t.id);
  return h("tr", {},
    h("td", {}, h("strong", {}, e.name), h("div", { class: "muted" }, e.role_title)),
    h("td", {}, e.backend, " ", pendingBadge(e), e.model || e.effort ? h("div", { class: "muted" }, [e.model, e.effort].filter(Boolean).join(" · ")) : null),
    h("td", {}, statusEl(e.status)),
    h("td", { class: "mono" }, assigned.join(", ") || "—"),
    h("td", { class: "muted" }, workingOn(e)),
    h("td", { class: "act" },
      h("button", { class: "btn small", onclick: () => go("chat", e.id) }, "Chat"), " ",
      h("button", { class: "btn small", onclick: () => go("live", e.id) }, "Live"), " ",
      h("button", { class: "btn small", title: "Run this person on another agent, model or effort", onclick: () => changeAgent(e) }, "Change agent"), " ",
      h("button", { class: "btn small", title: "Hire another employee with the same role and agent", onclick: () => duplicate(e) }, "Duplicate"), " ",
      h("button", { class: "btn small danger", title: "Let go: ends their session; their tickets return to the backlog", onclick: () => fire(e) }, "Fire")));
}

function gridCard(e) {
  const live = S().live?.[e.id];
  const recent = (live?.tools || []).slice(-3);
  const idx = S().employees.indexOf(e);
  return h("article", { class: "tcard" },
    h("div", { class: "row" }, h("strong", {}, e.name), h("span", { class: "muted" }, e.role_title), h("span", { class: "push" }, statusEl(e.status))),
    h("div", { class: "muted two-line" }, workingOn(e)),
    h("div", { class: "tools", "aria-label": "Recent tool calls" }, recent.length ? recent.map((t) => h("div", {}, `${t.status === "completed" ? "✓" : t.status === "failed" ? "✗" : "…"} ${t.title}`)) : h("div", {}, "no tool calls yet")),
    h("div", { class: "row" }, h("span", { class: "faint" }, `${e.backend}${e.model ? ` · ${e.model}` : ""}${e.effort ? ` · ${e.effort}` : ""} · ${idx + 1}/${S().employees.length}`), pendingBadge(e), h("span", { class: "push" }),
      h("button", { class: "btn small", onclick: () => go("chat", e.id) }, "Chat"),
      h("button", { class: "btn small", onclick: () => go("live", e.id) }, "Live"),
      h("button", { class: "btn small", onclick: () => changeAgent(e) }, "Change agent"),
      h("button", { class: "btn small", title: "Hire another employee with the same role and agent", onclick: () => duplicate(e) }, "Duplicate")));
}

export default {
  id: "team", title: "Team", icon: "team",
  mount(root) {
    const toggle = h("div", { class: "seg", role: "group", "aria-label": "Layout" },
      ["list", "grid"].map((m) => h("button", { type: "button", "data-mode": m, "aria-pressed": String(m === mode), onclick: () => {
        mode = m; try { localStorage.setItem("chillax.team", m); } catch { /* ignore */ }
        this.update();
      } }, m === "list" ? "List" : "Grid")));
    const { head, body } = viewShell("Team", toggle);
    const NEW_ROLE = "__new__";
    const roleSel = h("select", { required: true, "aria-label": "Role", onchange: () => {
      // "＋ New role based on…" opens the copy dialog and leaves the pick on a real role.
      if (roleSel.value === NEW_ROLE) { roleSel.value = roleSel.dataset.last || ""; openNewRole(); return; }
      roleSel.dataset.last = roleSel.value;
      this.syncRole();
    } });
    const nameIn = h("input", { placeholder: "auto", "aria-label": "Name (optional)" });
    const limitIn = h("input", { type: "number", min: "1", step: "1", "aria-label": "Maximum agents at once", style: "width:5rem" });
    const limitBtn = h("button", { type: "submit", class: "btn small primary" }, "Set");
    els = {
      toggle, roleSel, nameIn, limitIn, limitBtn,
      limitText: h("span", { class: "muted", role: "status" }),
      limitBar: h("form", { class: "row pad", onsubmit: (ev) => { ev.preventDefault(); saveLimit(limitIn); } },
        h("strong", {}, "Agents"), null),
      roleDesc: h("p", { class: "muted" }),
      agent: h("div", { class: "row" }),
      table: h("tbody"),
      gridWrap: h("div", { class: "tgrid" }),
      empty: h("p", { class: "empty pad" }, "No employees yet. Pick a role and hire your first one."),
      listWrap: h("section", { class: "panel" }, h("table", { class: "tbl" }, h("thead", {}, h("tr", {}, ["Name", "Agent", "Status", "Tickets", "Working on", ""].map((x) => h("th", {}, x)))), null)),
      catalog: h("section", { class: "panel" },
        h("h3", {}, "Find a persona", h("button", { id: "catalog-refresh", class: "btn small", type: "button" }, "Refresh catalog")),
        h("div", { class: "panel-b" }, h("input", { id: "persona-search", type: "search", placeholder: "Search: backend, security, game…", autocomplete: "off", "aria-label": "Search personas", class: "w-full" }),
          h("p", { id: "catalog-status", class: "muted mt-6" })),
        h("ul", { id: "personas", class: "personas" })),
    };
    els.limitBar.append(els.limitText, h("span", { class: "push" }), h("label", { class: "muted" }, "Allow ", els.limitIn), els.limitBtn);
    els.listWrap.querySelector("table").append(els.table);
    const form = h("form", { class: "panel", onsubmit: async (e) => {
      e.preventDefault();
      const agent = picker?.value || {};
      if (agent.backend) { try { await ensureAgent(agent.backend); } catch (err) { toast(err.message); return; } }
      if (await send({ type: "hire_employee", role: roleSel.value, ...(nameIn.value.trim() ? { name: nameIn.value.trim() } : {}), ...agentFields(agent) })) nameIn.value = "";
    } }, h("h3", {}, "Hire"),
      h("div", { class: "form" }, h("label", { class: "field" }, h("span", {}, "Role"), roleSel), h("label", { class: "field" }, h("span", {}, "Name"), nameIn),
        h("div", { class: "field" }, h("span", {}, "Agent, model and effort"), els.agent), els.roleDesc,
        h("div", { class: "actions" }, h("button", { type: "submit", class: "btn primary" }, "Hire"))));
    body.append(h("div", { class: "teamlayout" },
      h("div", { class: "col-stack min-0" }, els.limitBar, els.listWrap, els.gridWrap, els.empty),
      h("div", { class: "col-stack" }, form, els.catalog)));
    root.append(head, body);
    bindCatalog();
    loadCatalog();
  },
  syncRole() {
    const r = store.roles.find((x) => x.id === els.roleSel.value);
    els.roleDesc.textContent = r ? r.description : store.roles.length ? "" : "No roles available, so nobody can be hired yet.";
  },
  update() {
    const s = S();
    els.toggle.querySelectorAll("button").forEach((b) => b.setAttribute("aria-pressed", String(b.dataset.mode === mode)));
    if (roleCount !== store.roles.length) {
      roleCount = store.roles.length;
      const prev = els.roleSel.value;
      fill(els.roleSel, [h("option", { value: "__new__" }, "＋ New role based on…"), ...store.roles.map((r) => h("option", { value: r.id }, r.title))]);
      els.roleSel.value = prev && prev !== "__new__" ? prev : store.roles[0]?.id || "";
      els.roleSel.dataset.last = els.roleSel.value;
      this.syncRole();
    }
    const key = JSON.stringify(s.hiring || {}) + agentList.length;
    if (key !== pickerKey) { pickerKey = key; picker = agentPicker(s.hiring || {}); fill(els.agent, picker.el); }
    const lim = s.agent_limit;
    els.limitBar.hidden = !lim;
    if (lim) {
      const running = s.employees.filter((e) => e.status === "working").length;
      els.limitText.textContent = `${running} working of ${lim.limit} allowed (this machine fits ${lim.hardware_max})`;
      els.limitIn.max = String(lim.hardware_max);
      if (document.activeElement !== els.limitIn) els.limitIn.value = String(lim.limit);
    }
    els.empty.hidden = s.employees.length > 0;
    els.listWrap.hidden = mode !== "list" || !s.employees.length;
    els.gridWrap.hidden = mode !== "grid" || !s.employees.length;
    const sigOf = (e) => JSON.stringify([e, workingOn(e), S().tickets.filter((t) => t.assignee === e.id).map((t) => t.id), S().live?.[e.id]?.tools?.slice(-3), S().employees.length]);
    if (mode === "list") keyed(els.table, s.employees, (e) => e.id, rosterRow, sigOf);
    else keyed(els.gridWrap, s.employees, (e) => e.id, gridCard, sigOf);
  },
};
