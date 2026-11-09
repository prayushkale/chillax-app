// Pieces shared by several views.
import { h } from "../dom.js";
import { openTicket } from "../drawer.js";
import { nameOf, initials, relTime, S } from "../util.js";
import { send, store } from "../store.js";
import { askText, toast } from "../ui.js";
import { go } from "../nav.js";

export function viewShell(title, ...actions) {
  const head = h("header", { class: "vhead" }, h("h1", {}, title), h("span", { class: "spacer" }), ...actions);
  const body = h("div", { class: "vbody" });
  return { head, body };
}

export const assigneeChip = (id) => id ? h("span", { class: "avatar", title: nameOf(id) }, initials(nameOf(id))) : null;

export const ticketLink = (t, label) => h("a", { href: "#", onclick: (e) => { e.preventDefault(); openTicket(t.id, e.currentTarget); } }, label || t.id);

const LOCAL_URL = /^https?:\/\/(localhost|127\.0\.0\.1|\[::1\])(:\d+)?(\/|$)/i;

/** A link to an app. A reported app (it has an `employee`) links only to a local address. */
export function appLink(app) {
  const url = String(app.url || "");
  return (app.employee ? LOCAL_URL.test(url) : /^https?:\/\//.test(url))
    ? h("a", { href: app.url, target: "_blank", rel: "noopener noreferrer" }, app.url.replace(/^https?:\/\//, ""))
    : h("span", { class: "muted" }, app.url);
}
export const appState = (app) => (!app.wanted ? "stopped" : app.status);

const ENV_ORDER = ["dev", "qa", "prod"];
const rank = (label) => { const i = ENV_ORDER.indexOf(label); return i < 0 ? ENV_ORDER.length : i; };

/** Supervised and reported apps as one list, grouped by label (dev, qa, prod, the rest). */
export function runningApps(s) {
  const rows = [
    ...(s.apps || []).map((a) => ({ label: a.env, url: a.url, status: appState(a), commit: a.commit, since: a.since, supervised: a })),
    ...(s.reported_apps || []).map((a) => ({ label: a.label, url: a.url, status: "running", ticket: a.ticket, employee: a.employee, note: a.note, since: a.since, reported: a })),
  ];
  return rows.map((r, i) => [r, i]).sort((x, y) => rank(x[0].label) - rank(y[0].label) || x[1] - y[1]).map((x) => x[0]);
}

/** One row of the dashboard's Running apps panel. */
export function appRow(r) {
  const who = r.employee ? ["by ", nameOf(r.employee)] : null;
  const what = r.commit ? h("code", {}, `@${String(r.commit).slice(0, 7)}`)
    : r.ticket ? h("a", { href: "#", onclick: (e) => { e.preventDefault(); openTicket(r.ticket, e.currentTarget); } }, r.ticket) : null;
  return h("div", { class: "row approw" },
    h("span", { class: "pill" }, r.label),
    appLink(r.reported ? { url: r.url, employee: r.employee } : r.supervised),
    h("span", { class: `pill ${r.status}` }, r.status),
    what,
    who ? h("span", { class: "muted" }, ...who) : null,
    r.since ? h("span", { class: "faint" }, relTime(r.since)) : null,
    r.note ? h("span", { class: "muted" }, r.note) : null);
}

/** Ask the project manager to plan a new sprint, then open the chat with them. */
export async function startNewSprint() {
  const note = await askText({ title: "Start a new sprint", label: "What should the next release deliver? (optional)", confirm: "Talk to the project manager", multiline: true });
  if (note === null) return;
  const s = S();
  const pm = s.employees.find((e) => (store.roles.find((x) => x.id === e.role)?.owns_states || []).includes(s.workflow_states[0]))
    || s.employees.find((e) => e.role === "project-manager");
  if (!await send({ type: "request_sprint_planning", ...(note ? { note } : {}) })) return;
  if (pm) go("chat", pm.id); else toast("Hire a Project Manager to plan the sprint");
}
