import { h, fill, keyed } from "../dom.js";
import { store } from "../store.js";
import { S, nameOf, statusEl, workingOn, activeSprint, sprintInFlight, liveTickets, inboxTitle, summary, NOISY, ticketOf } from "../util.js";
import { viewShell, runningApps, appRow, startNewSprint } from "./common.js";
import { go } from "../nav.js";
import { send } from "../store.js";
import { openTicket } from "../drawer.js";

let els;
export default {
  id: "dashboard", title: "Dashboard", icon: "dashboard",
  mount(root) {
    const { head, body } = viewShell("Dashboard");
    els = {
      stats: h("div", { class: "stats" }),
      apps: h("div", { class: "panel-b" }),
      appsEmpty: h("p", { class: "empty pad" }, "Nothing is running yet. Start dev in chillax.toml or ask a developer to report their server."),
      team: h("div", { class: "list" }),
      teamEmpty: h("p", { class: "empty pad" }, "Nobody yet. Hire a Project Manager on the Team page."),
      sprint: h("div", { class: "panel-b" }),
      alerts: h("div", { class: "list" }),
      alertsEmpty: h("p", { class: "empty pad" }, "Nothing needs you right now."),
      feed: h("ol", { class: "log compact" }),
      feedEmpty: h("p", { class: "empty pad" }, "Waiting for events…"),
      clear: h("button", { class: "btn small", type: "button", onclick: () => send({ type: "clear_activity" }) }, "Clear"),
    };
    body.append(els.stats,
      h("section", { class: "panel mb-12" }, h("h3", {}, "Running apps"), els.apps, els.appsEmpty),
      h("div", { class: "grid2 two" },
        h("section", { class: "panel" }, h("h3", {}, "Team", h("button", { class: "btn small", onclick: () => go("team") }, "Open team")), els.team, els.teamEmpty),
        h("div", { class: "col-stack" },
          h("section", { class: "panel" }, h("h3", {}, "Sprint"), els.sprint),
          h("section", { class: "panel" }, h("h3", {}, "Needs you"), els.alerts, els.alertsEmpty)),
        h("section", { class: "panel wide" }, h("h3", {}, "Recent activity", els.clear), els.feed, els.feedEmpty)));
    root.append(head, body);
  },
  update() {
    const s = S();
    const working = s.employees.filter((e) => e.status === "working" || e.turn_open).length;
    const sprint = activeSprint();
    const tickets = sprint ? sprint.tickets.map(ticketOf).filter((t) => t && !t.dropped) : [];
    const done = tickets.filter((t) => t.state === s.workflow_states.at(-1)).length;
    const stat = (k, v, sub, onclick, tone = "") => h("button", { class: `stat ${tone}`, type: "button", onclick }, h("div", { class: "k" }, k), h("div", { class: "v" }, v), h("div", { class: "s" }, sub));
    fill(els.stats,
      stat("Team", `${working} / ${s.employees.length}`, "working now", () => go("team")),
      stat("Sprint", sprint ? `${done} / ${tickets.length}` : "—", sprint ? `${sprint.name} · ${sprint.status}` : `${liveTickets().filter((t) => t.state !== s.workflow_states.at(-1)).length} in backlog`, () => go("board")),
      stat("Inbox", s.inbox.length, s.inbox.length ? "needs you" : "all clear", () => go("inbox"), s.inbox.length ? "warn" : ""),
      stat("Dev loop", s.dispatch_paused ? "Paused" : "Running", Object.keys(s.backend_limits || {}).length ? `${Object.keys(s.backend_limits).join(", ")} rate limited` : "no limits", () => go("environments"), s.dispatch_paused ? "warn" : ""));

    const apps = runningApps(s);
    els.appsEmpty.hidden = apps.length > 0;
    fill(els.apps, apps.map(appRow));

    els.teamEmpty.hidden = s.employees.length > 0;
    keyed(els.team, s.employees, (e) => e.id, (e) => h("button", { class: "item", type: "button", onclick: () => go("live", e.id) },
      h("div", { class: "grow" }, h("div", {}, h("strong", {}, e.name), " ", h("span", { class: "muted" }, e.role_title)), h("div", { class: "muted t" }, workingOn(e))), statusEl(e.status)),
      (e) => JSON.stringify([e, workingOn(e)]));

    const planning = s.planning;
    fill(els.sprint, sprint ? [
      h("div", { class: "row" }, h("strong", {}, `${sprint.id} ${sprint.name}`), h("span", { class: `pill ${sprint.status}` }, sprint.status)),
      sprint.goal ? h("p", { class: "muted" }, sprint.goal) : null,
      h("p", { class: "sprint-count" }, `${done} of ${tickets.length} done`),
      h("progress", { max: tickets.length || 1, value: done }),
    ] : h("p", { class: "empty" }, "No sprint running."),
    sprintInFlight() ? null
      : planning && !planning.briefed ? h("p", { class: "muted" }, "Waiting for the project manager to take the planning request.")
      : planning ? h("p", { class: "muted" }, "The project manager is planning the next sprint.")
      : h("button", { class: "btn small primary mt-8", type: "button", onclick: startNewSprint }, "Start a new sprint"));

    els.alertsEmpty.hidden = s.inbox.length > 0;
    keyed(els.alerts, s.inbox, (i) => i.id, (i) => h("button", { class: "item", type: "button", onclick: () => go("inbox") },
      h("span", { class: "kind" }, i.type.replace(/_/g, " ")), h("span", { class: "grow t" }, inboxTitle(i))));

    const cleared = s.activity_cleared_seq || 0;
    const recent = store.events.filter((e) => e.seq > cleared && !NOISY.includes(e.type)).slice(-10).reverse();
    els.clear.disabled = !recent.length;
    els.clear.title = recent.length ? "Hide activity so far; the Events page keeps everything" : "No activity to clear";
    els.feedEmpty.hidden = recent.length > 0;
    keyed(els.feed, recent, (e) => e.seq, (e) => h("li", {}, summary(e)), () => "");
  },
};
