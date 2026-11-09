// Shared UI helpers with no state: toast, modal dialogs, text prompts.
import { h, fill, $ } from "./dom.js";

let toastTimer;
export function toast(message) {
  const t = $("#toast");
  t.textContent = message;
  t.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { t.hidden = true; }, 5000);
}

/** Show a form in the shared modal; resolves to what `onSubmit` returned truthy, or null. */
export function modal(title, build) {
  const dlg = $("#modal");
  return new Promise((resolve) => {
    let done = false;
    const close = (v) => { if (done) return; done = true; dlg.close(); resolve(v ?? null); };
    const body = build(close);
    fill(dlg, h("form", { class: "form", method: "dialog", onsubmit: (e) => e.preventDefault() }, h("h2", {}, title), body));
    dlg.onclose = () => close(null);
    dlg.showModal();
    dlg.querySelector("input:not([type=hidden]), textarea, select")?.focus();
  });
}

/** Ask for a line of text; `required` keeps Confirm disabled until something is typed. */
export function askText({ title, label, confirm = "OK", required = false, multiline = false, danger = false, note = "" }) {
  return modal(title, (close) => {
    const input = multiline ? h("textarea", { rows: 3, "aria-label": label }) : h("input", { "aria-label": label });
    const ok = h("button", { type: "submit", class: `btn ${danger ? "danger" : "primary"}`, disabled: required }, confirm);
    input.addEventListener("input", () => { ok.disabled = required && !input.value.trim(); });
    input.addEventListener("keydown", (e) => {
      const send = multiline ? (e.metaKey || e.ctrlKey) : !e.shiftKey;
      if (e.key === "Enter" && send && !ok.disabled) { e.preventDefault(); close(input.value.trim()); }
    });
    ok.addEventListener("click", () => close(input.value.trim()));
    return [note ? h("p", { class: "muted" }, note) : null, h("label", { class: "field" }, h("span", {}, label), input),
      h("div", { class: "actions" }, ok, h("button", { type: "button", class: "btn", onclick: () => close(null) }, "Cancel"))];
  });
}

/** Yes/no question; resolves to true or false. */
export async function confirmBox(title, text, confirm = "Confirm", danger = true) {
  const r = await modal(title, (close) => [h("p", {}, text),
    h("div", { class: "actions" },
      h("button", { type: "submit", class: `btn ${danger ? "danger" : "primary"}`, onclick: () => close(true) }, confirm),
      h("button", { type: "button", class: "btn", onclick: () => close(null) }, "Cancel"))]);
  return r === true;
}
