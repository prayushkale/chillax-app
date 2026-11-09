// Project switcher: lists the other Chillax instances running on this machine
// (GET /api/v1/instances) and links to their dashboards. Each instance has its own
// origin, so nothing here fetches another one; choosing a project is plain navigation.
import { h, $, fill } from "./dom.js";
import { getJson } from "./store.js";

const REFRESH_MS = 15000;
let instances = [];
let current = null;
let panel = null;
let button = null;
let focusIndex = 0;

/** Where to go for `inst`, keeping the view the person is on. */
export function target(inst) { return inst.url + location.hash; }
export const isCurrent = (inst) => current != null && inst.root === current;

function items() { return [...panel.querySelectorAll("a.proj-item")]; }

function close(returnFocus = true) {
  if (!panel || panel.hidden) return;
  panel.hidden = true;
  button.setAttribute("aria-expanded", "false");
  if (returnFocus) button.focus();
}

function open() {
  panel.hidden = false;
  button.setAttribute("aria-expanded", "true");
  const list = items();
  focusIndex = Math.max(0, list.findIndex((a) => a.getAttribute("aria-current") === "true"));
  list[focusIndex]?.focus();
}

function onPanelKey(e) {
  const list = items();
  const at = list.indexOf(document.activeElement);
  if (e.key === "Escape") { e.preventDefault(); close(); }
  else if (e.key === "ArrowDown") { e.preventDefault(); list[(at + 1) % list.length]?.focus(); }
  else if (e.key === "ArrowUp") { e.preventDefault(); list[(at - 1 + list.length) % list.length]?.focus(); }
  else if (e.key === "Home") { e.preventDefault(); list[0]?.focus(); }
  else if (e.key === "End") { e.preventDefault(); list[list.length - 1]?.focus(); }
  else if (e.key === "Tab") close(false);
}

function build() {
  const brand = $(".brand");
  button = h("button", { type: "button", class: "proj-btn", "aria-haspopup": "true", "aria-expanded": "false", "aria-controls": "proj-panel", onclick: () => (panel.hidden ? open() : close()) },
    h("span", { class: "proj-name", id: "proj-name" }), h("span", { class: "proj-caret", "aria-hidden": "true" }, "▾"));
  panel = h("div", { id: "proj-panel", class: "proj-panel", role: "group", "aria-label": "Running projects", hidden: true, onkeydown: onPanelKey });
  const wrap = h("div", { class: "proj", id: "project-switch" }, button, panel);
  brand.append(wrap);
  document.addEventListener("click", (e) => { if (!wrap.contains(e.target)) close(false); });
  document.addEventListener("keydown", (e) => { if (e.key === "Escape" && panel && !panel.hidden && !panel.contains(document.activeElement)) close(); });
  return wrap;
}

function render() {
  const multi = instances.length > 1;
  const label = $("#company");
  let wrap = $("#project-switch");
  $(".brand").classList.toggle("multi", multi);
  if (!multi) {
    if (wrap) { close(false); wrap.hidden = true; }
    label.hidden = false;
    return;
  }
  wrap ??= build();
  wrap.hidden = false;
  label.hidden = true;
  const here = instances.find(isCurrent);
  $("#proj-name").textContent = here?.name || label.textContent;
  button.title = `${instances.length} projects running. Switch project`;
  fill(panel, h("div", { class: "proj-head" }, "Running projects"),
    instances.map((inst) => h("a", { class: `proj-item${isCurrent(inst) ? " here" : ""}`, href: target(inst), "aria-current": isCurrent(inst) ? "true" : null, title: inst.root },
      h("span", { class: "proj-item-name" }, inst.name, isCurrent(inst) ? h("span", { class: "proj-here" }, "this one") : null),
      h("span", { class: "proj-item-path" }, inst.root))));
}

async function load() {
  try {
    const body = await getJson("/instances");
    instances = Array.isArray(body.instances) ? body.instances : [];
    current = body.current ?? null;
  } catch {
    return; // keep what we had; the next tick tries again
  }
  render();
}

export function bindProjects() {
  load();
  setInterval(load, REFRESH_MS);
  window.addEventListener("focus", load);
  // Hash changes keep the links pointing at the current view.
  window.addEventListener("hashchange", () => { if (instances.length > 1) render(); });
}
