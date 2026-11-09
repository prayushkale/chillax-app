// Multi-line composer: grows with its text, Enter sends, Shift+Enter adds a line.
import { h } from "./dom.js";

export function composer({ placeholder, label, onSend, button = "Send" }) {
  const area = h("textarea", { rows: 1, placeholder, "aria-label": label || placeholder });
  const send = h("button", { type: "button", class: "btn primary", disabled: true }, button);
  const grow = () => {
    area.style.height = "auto";
    area.style.height = `${Math.min(area.scrollHeight + 2, 220)}px`;
    send.disabled = area.disabled || !area.value.trim();
  };
  const submit = async () => {
    const text = area.value.trim();
    if (!text || area.disabled) return;
    const keep = area.value;
    area.value = "";
    grow();
    const ok = await onSend(text);
    if (ok === false) { area.value = keep; grow(); }
  };
  area.addEventListener("input", grow);
  area.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey && !e.isComposing) { e.preventDefault(); submit(); }
  });
  send.addEventListener("click", submit);
  const el = h("div", { class: "composer" }, area, send);
  return {
    el, area,
    get value() { return area.value; },
    set value(v) { area.value = v; grow(); },
    setDisabled(d, why) { area.disabled = d; area.title = d ? why || "" : ""; grow(); },
    focus: () => area.focus(),
  };
}
