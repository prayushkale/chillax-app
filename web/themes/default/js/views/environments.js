import { h, fill, keyed } from "../dom.js";
import { send } from "../store.js";
import { S, liveTickets, ticketOf } from "../util.js";
import { viewShell, appLink, appState } from "./common.js";

let els, forceArmed = false, forceTimer = null, view = null;
const roundStatus = { starting: "starting", testing: "testing", env_failed: "env failed", closed: "closed" };
const releaseStatus = { gated: "awaiting approval", approved: "merging and tagging", tagged: "tagged", deployed: "deployed", deploy_failed: "deploy failed", rejected: "rejected" };
const verdictMark = { pass: ["✓", "ok"], fail: ["✗", "bad"], env: ["!", "warn"] };
const appStatus = { starting: "starting", running: "running", crashed: "crashed", stopped: "stopped" };

function retryTarget() {
  const s = S();
  const alert = s.inbox.find((i) => i.type === "env_alert");
  if (alert) return { env: alert.env, step: alert.step };
  const last = s.releases?.[s.releases.length - 1];
  return ["deploy_failed", "approved"].includes(last?.status) ? { env: "prod", step: "deploy" } : null;
}
const retryButton = (t) => h("button", { class: "btn small", onclick: () => send({ type: "retry_env", env: t.env, step: t.step }) }, `Retry ${t.env} ${t.step}`);

function promoteReason(roundOpen, waiting, candidate) {
  if (roundOpen) return "A QA round is open";
  if (candidate) return "A release is in flight";
  if (S().qa_requested) return "Promotion to qa is already requested";
  return waiting ? null : "Nothing is waiting for QA";
}

function appBlock(env) {
  const app = (S().apps || []).find((a) => a.env === env);
  if (!app) return null;
  const down = !app.wanted;
  const state = appState(app);
  const act = (type) => () => send({ type, env });
  return h("div", { class: "mt-8" },
    h("div", { class: "row" }, h("span", { class: `pill ${state}` }, appStatus[state] || state),
      app.commit && app.status === "running" ? h("span", { class: "mono faint" }, `@${String(app.commit).slice(0, 7)}`) : null,
      down ? h("span", { class: "muted" }, "stopped by you") : app.reason ? h("span", { class: "muted" }, app.reason) : null),
    h("p", {}, appLink(app)),
    h("div", { class: "actions" },
      down || app.status === "crashed" ? h("button", { class: "btn small", onclick: act("start_app") }, "Start") : null,
      !down && ["running", "starting"].includes(app.status) ? h("button", { class: "btn small", onclick: act("stop_app") }, "Stop") : null,
      !down && ["running", "starting", "crashed"].includes(app.status) ? h("button", { class: "btn small", onclick: act("restart_app") }, "Restart") : null));
}

function releaseEditor(release) {
  const version = h("input", { value: release.version, "aria-label": "Version" });
  const notes = h("textarea", { rows: 8, "aria-label": "Release notes" });
  notes.value = release.notes;
  const reason = h("input", { placeholder: "Why not? (optional)", "aria-label": "Reason" });
  const n = release.tickets.length;
  return h("div", { class: "form flush" },
    h("label", { class: "field" }, h("span", {}, "Version"), version),
    h("p", { class: "muted" }, `${n} ${n === 1 ? "ticket" : "tickets"}: ${release.tickets.join(", ") || "none"}`),
    release.known_issues?.length ? h("p", { class: "warn-text" }, `Known issues: ${release.known_issues.join(", ")}`) : null,
    h("label", { class: "field" }, h("span", {}, "Notes (prepended to CHANGELOG.md, used as the tag message)"), notes),
    h("div", { class: "actions" }, h("button", { class: "btn primary", onclick: () => version.value.trim() && notes.value.trim() && send({ type: "approve_release", id: release.id, version: version.value.trim(), notes: notes.value }) }, "Approve and release"),
      reason, h("button", { class: "btn", onclick: () => send({ type: "reject_release", id: release.id, reason: reason.value }) }, "Reject")));
}

function cards() {
  const s = S();
  const tickets = liveTickets();
  const open = tickets.filter((t) => t.branch && t.state !== s.workflow_states.at(-1) && !(t.checks || []).includes("merged_to_dev")).length;
  const waiting = tickets.filter((t) => t.state === "in_qa" && !(t.checks || []).includes("qa_passed")).length;
  const round = s.qa;
  const roundOpen = round && round.status !== "closed";
  const releases = s.releases || [];
  const candidate = releases.find((r) => ["gated", "approved", "tagged"].includes(r.status));
  const lastTagged = [...releases].reverse().find((r) => ["tagged", "deployed", "deploy_failed"].includes(r.status));
  const retry = retryTarget();
  const why = promoteReason(roundOpen, waiting, candidate);
  const apps = (s.apps || []).filter((a) => a);

  const dev = { key: "dev", sig: [open, waiting, s.qa_requested, why, apps.find((a) => a.env === "dev")], build: () => h("section", { class: "panel" }, h("h3", {}, "dev"), h("div", { class: "panel-b" },
    h("p", {}, `${open} open ${open === 1 ? "branch" : "branches"}`), h("p", {}, `${waiting} merged to dev, waiting for QA`),
    s.qa_requested ? h("p", { class: "warn-text" }, "Promotion to qa requested.") : null,
    h("div", { class: "actions mt-8" }, h("button", { class: "btn primary small", disabled: !!why, title: why || "Fast-forward qa to dev and start a QA round", onclick: () => send({ type: "promote_to_qa" }) }, "Promote to QA")),
    why ? h("p", { class: "faint" }, why) : null, appBlock("dev"))) };

  const qa = { key: "qa", sig: [round, apps.find((a) => a.env === "qa"), s.tickets.map((t) => t.title)], build: () => h("section", { class: "panel" }, h("h3", {}, "qa"), h("div", { class: "panel-b" },
    round ? [h("p", {}, h("strong", {}, `Round ${round.number}`), " · ", h("span", { class: `pill ${round.status}` }, roundStatus[round.status] || round.status), h("span", { class: "mono faint" }, ` ${String(round.commit).slice(0, 7)}`)),
      round.status === "env_failed" ? h("p", { class: "bad-text" }, "The environment failed; retry once it is fixed.") : null,
      h("ul", { class: "plain verdicts" }, round.tickets.map((id) => {
        const v = round.verdicts?.[id];
        const [mark, tone] = verdictMark[v] || ["…", "muted"];
        return h("li", {}, h("span", { class: `mark ${tone}`, "aria-hidden": "true" }, mark), h("strong", {}, id), v || "pending", ticketOf(id) ? h("span", { class: "muted" }, ticketOf(id).title) : null);
      }))] : h("p", { class: "empty" }, "No QA round yet."),
    appBlock("qa"))) };

  const prod = { key: "prod", sig: [lastTagged, apps.find((a) => a.env === "prod")], build: () => h("section", { class: "panel" }, h("h3", {}, "prod"), h("div", { class: "panel-b" },
    lastTagged ? h("p", {}, h("strong", {}, lastTagged.tag || lastTagged.version), " · ", h("span", { class: `pill ${lastTagged.status}` }, releaseStatus[lastTagged.status])) : h("p", { class: "empty" }, "Nothing released yet."),
    appBlock("prod"))) };

  const gate = { key: "gate", sig: [candidate, roundOpen, forceArmed, retry], build: () => h("section", { class: "panel wide" }, h("h3", {}, "Release candidate"), h("div", { class: "panel-b" },
    candidate ? [h("p", {}, h("strong", {}, candidate.version), " · ", h("span", { class: `pill ${candidate.status}` }, releaseStatus[candidate.status])),
      candidate.status === "gated" ? releaseEditor(candidate) : h("pre", { class: "out" }, candidate.notes),
      candidate.status === "approved" ? [h("p", { class: "warn-text" }, "Chillax could not finish this release yet. Check the inbox for why, fix it, then retry; or reject the release."),
        h("div", { class: "actions" }, retry ? retryButton(retry) : null, h("button", { class: "btn small", onclick: () => send({ type: "reject_release", id: candidate.id, reason: "" }) }, "Reject release"))] : null]
    : [h("p", { class: "empty" }, "No release candidate."),
      h("div", { class: "actions" },
        h("button", { class: "btn small", disabled: roundOpen, title: roundOpen ? "A QA round is open" : "Bundle what QA signed off into a release for you to approve", onclick: () => send({ type: "open_release_gate", force: false }) }, "Prepare release"),
        forceArmed ? [h("span", { class: "warn-text" }, "Ships tickets that failed QA as known issues. Sure?"),
          h("button", { class: "btn small danger", onclick: () => { forceArmed = false; send({ type: "open_release_gate", force: true }); view.update(); } }, "Yes, release with known issues"),
          h("button", { class: "btn small", onclick: () => { forceArmed = false; view.update(); } }, "Cancel")]
        : h("button", { class: "btn small", disabled: roundOpen, title: roundOpen ? "A QA round is open" : "Also lists the tickets that failed QA as known issues", onclick: () => { forceArmed = true; clearTimeout(forceTimer); forceTimer = setTimeout(() => { forceArmed = false; view.update(); }, 8000); view.update(); } }, "Release with known issues…")),
      roundOpen ? h("p", { class: "faint" }, "A QA round is open; you can prepare the release once it closes.") : null])) };
  return { list: [dev, qa, prod, gate], retry };
}

export default view = {
  id: "environments", title: "Environments", icon: "env",
  mount(root) {
    const { head, body } = viewShell("Environments");
    els = { retry: h("div", { class: "banner bad", hidden: true }), grid: h("div", { class: "envgrid" }) };
    body.append(els.retry, els.grid);
    root.append(head, body);
  },
  update() {
    const { list, retry } = cards();
    els.retry.hidden = !retry;
    if (retry) fill(els.retry, h("span", {}, h("strong", {}, `${retry.env} ${retry.step}`), " failed."), retryButton(retry));
    keyed(els.grid, list, (c) => c.key, (c) => c.build(), (c) => JSON.stringify(c.sig));
  },
};
