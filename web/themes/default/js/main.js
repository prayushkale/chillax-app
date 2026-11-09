// Chillax default theme. Talks only to the public /api/v1 API (REST + SSE).
import { h, fill, $, $$, hydrateIcons, icon } from "./dom.js";
import { store, subscribe, refresh, connect, loadRoles, send, getJson, loadError } from "./store.js";
import { views, onNav, parse, go } from "./nav.js";
import { S } from "./util.js";
import { closeDrawer, updateDrawer, isOpen, openTicket } from "./drawer.js";
import { bindPalette, openPalette } from "./palette.js";
import { loadAgents } from "./agents.js";
import { bindProjects } from "./projects.js";
import { newTicket } from "./views/board.js";
import dashboard from "./views/dashboard.js";
import chat from "./views/chat.js";
import company from "./views/company.js";
import board from "./views/board.js";
import live from "./views/live.js";
import team from "./views/team.js";
import inbox from "./views/inbox.js";
import environments from "./views/environments.js";
import events from "./views/events.js";
import settings from "./views/settings.js";
import usage from "./views/usage.js";
import "./crew.js";
import { applyTheme, nextTheme, savedTheme } from "./theme.js";

views.push(dashboard, chat, company, board, live, team, inbox, environments, events, usage, settings);
const sections = new Map();
let active = null;

function buildShell() {
  const main = $("#main");
  for (const v of views) {
    const el = h("section", { class: "view", id: `view-${v.id}`, "aria-label": v.title, hidden: true });
    main.append(el);
    v.mount(el);
    sections.set(v.id, el);
  }
  fill($("#nav"), views.map((v) => h("button", { type: "button", "data-view": v.id, title: v.title, onclick: () => go(v.id) },
    icon(v.icon), h("span", { class: "lbl" }, v.title), v.id === "inbox" ? h("span", { class: "count", id: "inbox-count", hidden: true }) : null)));
  hydrateIcons();
  $("#palette-kbd").textContent = /Mac|iPhone|iPad/.test(navigator.platform) ? "⌘ K" : "Ctrl K";
}

function show({ id, arg }) {
  const v = views.find((x) => x.id === id);
  if (active !== id) {
    active = id;
    for (const [vid, el] of sections) { el.hidden = vid !== id; el.classList.toggle("active", vid === id); }
    $$("#nav button").forEach((b) => { if (b.dataset.view === id) b.setAttribute("aria-current", "page"); else b.removeAttribute("aria-current"); });
    try { localStorage.setItem("chillax.view", id); } catch { /* storage blocked */ }
  }
  if (arg && v.open) v.open(arg);
  if (store.state) { v.update(); document.title = `${v.title} · ${store.state.company || "Chillax"}`; }
  if (arg && id === "chat") v.focus?.();
}

function renderChrome() {
  const s = store.state;
  if (!s) return;
  $("#company").textContent = s.company || "Chillax";
  const cur = views.find((x) => x.id === active);
  if (cur) document.title = `${cur.title} · ${s.company || "Chillax"}`;
  const n = s.inbox.length;
  const badge = $("#inbox-count");
  badge.hidden = n === 0;
  badge.textContent = n;
  const paused = !!s.dispatch_paused;
  fill($("#pause"), icon(paused ? "play" : "pause"), h("span", { class: "lbl" }, paused ? "Resume" : "Pause"));
  $("#pause").classList.toggle("paused", paused);
  $("#pause").title = paused ? "Resume the dev loop" : "Stop the dev loop: no new work starts and ticket sessions close";
  $("#paused-banner").hidden = !paused;
  const noTeam = !s.employees.length;
  $("#refresh").disabled = noTeam;
  $("#refresh").title = noTeam ? "Nobody is hired yet" : "Refresh agents: close idle sessions, forget waits and backoff, check rate-limited backends now";
  $("#loading")?.remove();
}

function renderConn() {
  const el = $("#conn");
  el.className = `conn ${store.conn}`;
  el.querySelector(".lbl").textContent = { live: "Live", connecting: "Connecting…", reconnecting: "Reconnecting…" }[store.conn];
  el.title = store.conn === "live" ? "Receiving live updates" : "Live updates paused; retrying automatically";
}

function renderError() {
  const msg = loadError();
  const box = $("#load-error");
  box.hidden = !msg;
  if (msg) fill(box, h("span", {}, msg), h("button", { class: "btn small", type: "button", onclick: () => refresh() }, "Retry"));
}

subscribe((kind) => {
  if (kind === "conn") return renderConn();
  if (kind === "error") return renderError();
  if (!store.state) return;
  if (kind === "state") {
    renderChrome();
    const v = views.find((x) => x.id === active);
    if (v) v.update();
    updateDrawer();
  } else if (kind === "event") {
    // Only the views that show the raw stream need to repaint on every event.
    if (active === "events") views.find((x) => x.id === "events").update();
  }
});

document.addEventListener("keydown", (e) => {
  if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") { e.preventDefault(); openPalette(); return; }
  const typing = /^(INPUT|TEXTAREA|SELECT)$/.test(document.activeElement?.tagName) || document.activeElement?.isContentEditable;
  if (e.key === "Escape" && !document.querySelector("dialog[open]") && isOpen()) { closeDrawer(); return; }
  if (typing || e.metaKey || e.ctrlKey || e.altKey || document.querySelector("dialog[open]")) return;
  if (pendingG && Date.now() - pendingG < 1200) {
    pendingG = 0;
    const map = { d: "dashboard", c: "chat", m: "company", b: "board", l: "live", t: "team", i: "inbox", e: "environments", v: "events", s: "settings", u: "usage" };
    if (map[e.key]) { e.preventDefault(); go(map[e.key]); }
    return;
  }
  if (e.key === "g") pendingG = Date.now();
  else if (e.key === "n") { e.preventDefault(); go("board"); newTicket(); }
  else if (e.key === "/") { const f = $(`#view-${active} input[type=search]`); if (f) { e.preventDefault(); f.focus(); } }
});
let pendingG = 0;

let theme = savedTheme();
function renderTheme() {
  const b = $("#theme");
  const name = theme[0].toUpperCase() + theme.slice(1);
  fill(b, icon({ system: "monitor", light: "sun", dark: "moon" }[theme]), h("span", { class: "lbl" }, `Theme: ${name}`));
  b.title = `Theme: ${name}. Click for ${nextTheme(theme)}`;
  b.setAttribute("aria-label", b.title);
}
$("#theme").addEventListener("click", () => { theme = nextTheme(theme); applyTheme(theme); renderTheme(); });
renderTheme();

$("#drawer-close").addEventListener("click", closeDrawer);
$("#refresh").addEventListener("click", () => send({ type: "kick" }));
$("#pause").addEventListener("click", () => send({ type: store.state?.dispatch_paused ? "resume_dispatch" : "pause_dispatch" }));
document.addEventListener("drawer-changed", () => { if (active === "board") views.find((x) => x.id === "board").update(); });

(async function start() {
  buildShell();
  bindPalette();
  bindProjects();
  onNav(show);
  let first = parse();
  if (!location.hash) { try { const saved = localStorage.getItem("chillax.view"); if (saved && views.some((v) => v.id === saved)) first = { id: saved, arg: null }; } catch { /* ignore */ } }
  show(first);
  renderConn();
  await Promise.all([loadRoles(), refresh()]);
  getJson("/health").then((hl) => { store.health = hl; }).catch(() => {});
  connect();
  await loadAgents();
  // The pickers were drawn before the agent list arrived; redraw the views that hold them.
  views.find((x) => x.id === active)?.update();
})();
