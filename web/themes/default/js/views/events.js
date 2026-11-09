import { h, keyed } from "../dom.js";
import { store } from "../store.js";
import { summary } from "../util.js";
import { viewShell } from "./common.js";

let els, filter = "", showNoisy = false;
export default {
  id: "events", title: "Events", icon: "events",
  mount(root) {
    const search = h("input", { type: "search", placeholder: "Filter by type or text", "aria-label": "Filter events", class: "w-240", oninput: (e) => { filter = e.target.value.toLowerCase(); this.update(); } });
    const noisy = h("label", { class: "row muted" }, h("input", { type: "checkbox", onchange: (e) => { showNoisy = e.target.checked; this.update(); } }), "Show streaming chunks");
    const { head, body } = viewShell("Events", noisy, search);
    body.className = "vbody flush";
    els = { list: h("ol", { class: "log" }), empty: h("p", { class: "empty pad" }, "No events yet. Hire someone or send a message and they will show up here.") };
    body.append(els.list, els.empty);
    root.append(head, body);
  },
  update() {
    const noisy = ["agent_message_chunk", "agent_tool_call_content", "agent_plan_updated"];
    const items = store.events.filter((e) => (showNoisy || !noisy.includes(e.type)) && (!filter || `${e.type} ${summary(e)}`.toLowerCase().includes(filter))).slice(-300).reverse();
    els.empty.hidden = items.length > 0;
    els.empty.textContent = store.events.length ? "No events match." : "No events yet. Hire someone or send a message and they will show up here.";
    keyed(els.list, items, (e) => e.seq, (e) => h("li", {}, h("span", { class: "seq" }, e.seq), h("span", { class: "type" }, e.type), h("span", { class: "sum" }, summary(e))), () => "");
  },
};
