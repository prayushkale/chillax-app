import { h, fill, keyed } from "../dom.js";
import { send } from "../store.js";
import { confirmBox } from "../ui.js";
import { S, employee, statusLabel, transcript } from "../util.js";
import { viewShell } from "./common.js";
import { transcriptView } from "../transcript.js";
import { composer } from "../composer.js";

let els, who = null;
const drafts = new Map();

export default {
  id: "chat", title: "Chat", icon: "chat",
  mount(root) {
    const clear = h("button", { class: "btn small", type: "button", onclick: async () => {
      const e = employee(who);
      if (e && await confirmBox(`Clear chat with ${e.name}?`, "Their chat session ends and the next message starts fresh.", "Clear chat")) send({ type: "clear_chat", employee: who });
    } }, "Clear chat");
    const stop = h("button", { class: "btn small", type: "button", hidden: true, title: "Stop this turn. Their session and context stay, so you can say more.",
      onclick: () => { if (who) send({ type: "cancel_turn", employee: who }); } }, "Stop");
    const { head, body } = viewShell("Chat", stop, clear);
    body.className = "twocol";
    body.classList.add("flush", "clip");
    els = {
      people: h("div", { class: "people", role: "list", "aria-label": "People" }),
      clear, stop,
      none: h("p", { class: "empty pad" }, "Hire someone first."),
      tr: transcriptView(),
      comp: composer({ placeholder: "Message… Enter to send, Shift+Enter for a new line", label: "Message",
        onSend: async (text) => !!(who && await send({ type: "send_chat", to: who, text })) }),
    };
    body.append(h("div", {}, els.people, els.none), h("div", { class: "pane" }, els.tr.el, els.comp.el));
    body.firstChild.classList.add("col-fill");
    root.append(head, body);
  },
  open(id) { this.select(id); },
  select(id) {
    if (!id || id === who) return;
    if (who) drafts.set(who, els.comp.value);
    who = id;
    els.comp.value = drafts.get(id) || "";
    els.tr.reset();
  },
  focus() { els.comp.focus(); },
  update() {
    const s = S();
    if (!who || !employee(who)) { who = null; this.select(s.employees[0]?.id); }
    els.none.hidden = s.employees.length > 0;
    keyed(els.people, s.employees, (e) => e.id, (e) => h("button", { class: "person", type: "button", role: "listitem", "aria-current": String(e.id === who),
        onclick: () => { this.select(e.id); this.update(); els.comp.focus(); } },
      h("strong", {}, e.name), h("small", {}, `${e.role_title} · ${e.stopped && e.status === "idle" ? "stopped" : statusLabel[e.status]}`)),
      (e) => JSON.stringify([e, e.id === who]));
    const e = employee(who);
    els.comp.setDisabled(!e, "Hire someone first");
    const replying = !!e?.turn_open;
    // Stop shows while a turn runs or a reply is on its way (turn_open only starts with the first chunk).
    els.stop.hidden = !e || !(e.turn_open || e.reply_pending || e.status === "working");
    els.clear.disabled = !e || replying || !transcript(who).length;
    els.clear.title = !e ? "Hire someone first" : replying ? `${e.name} is replying` : !transcript(who).length ? "No chat to clear" : "";
    els.tr.update(e ? transcript(who) : [], e?.name, e && ["working", "starting"].includes(e.status) ? `${e.name} is working…` : "",
      e?.last_progress && !["working", "starting"].includes(e.status) ? "Context cleared — idle" : e ? `Say hi to ${e.name}. Their agent session starts on your first message.` : "");
  },
};
