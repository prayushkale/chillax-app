import { h, fill } from "../dom.js";
import { store, send } from "../store.js";
import { S } from "../util.js";
import { viewShell } from "./common.js";
import { agentPicker, ensureAgent, agentList } from "../agents.js";
import { toast } from "../ui.js";
import { go } from "../nav.js";

let els, picker = null, key = "";
export default {
  id: "settings", title: "Settings", icon: "settings",
  mount(root) {
    const { head, body } = viewShell("Settings");
    els = { none: h("section", { class: "panel empty-state", hidden: true }, h("strong", {}, "Nobody is hired yet"), h("p", { class: "muted" }, "Hiring defaults apply to your first hire. Start with a Project Manager."), h("button", { class: "btn primary", type: "button", onclick: () => go("team") }, "Open Team")), company: h("dl", { class: "kv" }), conn: h("dl", { class: "kv" }), agent: h("div", { class: "row" }), now: h("p", { class: "muted" }) };
    const form = h("form", { class: "form", onsubmit: async (e) => {
      e.preventDefault();
      const { backend, model, effort } = picker.value;
      if (!backend) { toast("Pick an agent"); return; }
      try { await ensureAgent(backend); } catch (err) { toast(err.message); return; }
      if (await send({ type: "set_hiring_defaults", backend, ...(model ? { model } : {}), ...(effort ? { effort } : {}) })) toast("Saved");
    } }, els.agent, h("div", { class: "actions" }, h("button", { class: "btn primary", type: "submit" }, "Save")), els.now);
    body.append(els.none, h("div", { class: "grid2" },
      h("section", { class: "panel" }, h("h3", {}, "Company"), h("div", { class: "panel-b" }, els.company)),
      h("section", { class: "panel" }, h("h3", {}, "Connection"), h("div", { class: "panel-b" }, els.conn)),
      h("section", { class: "panel" }, h("h3", {}, "Hiring defaults"), h("p", { class: "muted lead" }, "The coding agent, model and effort new hires and persona revisions use. Picking another agent, model or effort while hiring also makes it the new default."), form),
      h("section", { class: "panel" }, h("h3", {}, "Keyboard"), h("div", { class: "panel-b" }, h("dl", { class: "kv" },
        [["Ctrl/Cmd K", "Command palette"], ["g then a letter", "Go to a view: d c b l t i e v u s"], ["n", "New ticket"], ["Esc", "Close drawer or dialog"]]
          .flatMap(([k, v]) => [h("dt", {}, h("kbd", {}, k)), h("dd", {}, v)])))),
      h("section", { class: "panel" }, h("h3", {}, "This browser"), h("div", { class: "panel-b" }, h("p", { class: "muted" }, "Chillax remembers the view you were on and the Team layout."),
        h("button", { class: "btn", type: "button", onclick: () => { try { localStorage.removeItem("chillax.view"); localStorage.removeItem("chillax.team"); } catch { /* ignore */ } toast("Forgotten."); } }, "Forget remembered view")))));
    root.append(head, body);
  },
  update() {
    const s = S();
    els.none.hidden = s.employees.length > 0;
    fill(els.company, h("dt", {}, "Name"), h("dd", {}, s.company || "Chillax"), h("dt", {}, "Employees"), h("dd", {}, s.employees.length), h("dt", {}, "Tickets"), h("dd", {}, s.tickets.length));
    fill(els.conn, h("dt", {}, "Server"), h("dd", {}, location.host), h("dt", {}, "Version"), h("dd", {}, store.health?.version || "unknown"), h("dt", {}, "API"), h("dd", {}, store.health?.api || "unknown"));
    const k = JSON.stringify(s.hiring || {}) + agentList.length;
    if (k !== key) { key = k; picker = agentPicker(s.hiring || {}); fill(els.agent, picker.el); }
    els.now.textContent = `Now: ${s.hiring?.backend || "each role's own agent"} · ${s.hiring?.model || "the agent's default model"}${s.hiring?.effort ? ` · ${s.hiring.effort} effort` : ""}`;
  },
};
