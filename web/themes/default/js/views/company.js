import { h, fill } from "../dom.js";
import { send } from "../store.js";
import { S } from "../util.js";
import { viewShell } from "./common.js";
import { transcriptView } from "../transcript.js";
import { composer } from "../composer.js";

let els;

const clock = (iso) => new Date(iso).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });

/** The feed as transcript messages: "10:42 · Pat → Sam". */
function items(s) {
  const name = (id) => s.employees.find((e) => e.id === id)?.name || id;
  return (s.company_chat || []).map((c) => ({
    key: `c${c.seq}`,
    msg: {
      speaker: { kind: c.from ? "teammate" : "stakeholder", label: `${clock(c.ts)} · ${c.from_name} → ${c.to ? name(c.to) : "everyone"}` },
      text: c.text,
    },
  }));
}

export default {
  id: "company", title: "Company", icon: "chat",
  mount(root) {
    const { head, body } = viewShell("Company chat");
    body.classList.add("flush", "clip");
    const to = h("select", { "aria-label": "Send to" }, h("option", { value: "" }, "@all (everyone)"));
    els = {
      to,
      tr: transcriptView(),
      comp: composer({ placeholder: "Write to everyone… @name wakes that person. Enter to send", label: "Company chat message",
        onSend: async (text) => !!(await send({ type: "post_company_chat", text, to: to.value || null })) }),
    };
    els.bar = h("label", { class: "company-to" }, h("span", {}, "To"), to,
      h("small", { class: "muted" }, "A post to everyone wakes only people you @mention; the rest see it with their next prompt."));
    body.append(h("div", { class: "pane" }, els.tr.el, els.bar, els.comp.el));
    root.append(head, body);
  },
  focus() { els.comp.focus(); },
  update() {
    const s = S();
    const keep = els.to.value;
    const want = ["", ...s.employees.map((e) => e.id)].join("|");
    if (els.to._want !== want) {
      els.to._want = want;
      fill(els.to, h("option", { value: "" }, "@all (everyone)"), s.employees.map((e) => h("option", { value: e.id }, e.name)));
      els.to.value = [...els.to.options].some((o) => o.value === keep) ? keep : "";
    }
    els.tr.update(items(s), null, "", "Nothing yet. Write to everyone here, or watch teammates talk to each other.");
  },
};
