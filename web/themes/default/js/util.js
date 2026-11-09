// Pure helpers over the state snapshot. Mirrors State::working_on / transcript in chillax-core.
import { h } from "./dom.js";
import { store } from "./store.js";

export const S = () => store.state;
export const employee = (id) => S().employees.find((e) => e.id === id);
export const nameOf = (id) => employee(id)?.name || id;
export const ticketOf = (id) => S().tickets.find((t) => t.id === id);
export const initials = (name) => String(name || "?").split(/\s+/).map((w) => w[0]).slice(0, 2).join("").toUpperCase();

export const statusLabel = {
  offline: "offline", starting: "starting", idle: "idle", working: "working",
  waiting_on_stakeholder: "waiting on you", failed: "failed",
};
export const statusEl = (s) => h("span", { class: `status ${s}` }, statusLabel[s] || s);

export const activeSprint = () => [...S().sprints].reverse().find((s) => !["rejected", "closed"].includes(s.status));
export const sprintInFlight = () => S().sprints.find((s) => ["proposed", "active"].includes(s.status));
/** Sprints that are over: released or closed, oldest first (mirrors State::past_sprints). */
export const pastSprints = () => S().sprints.filter((s) => ["released", "closed"].includes(s.status));
/**
 * Tickets the Board shows for a view: "active", "backlog" or a past sprint id
 * (mirrors State::board_tickets). Membership is `ticket.sprint`, not `sprint.tickets`.
 */
export const boardTickets = (view) => {
  const live = S().tickets.filter((t) => !t.dropped);
  if (view === "backlog") return live.filter((t) => !t.sprint);
  if (view === "active") { const id = sprintInFlight()?.id; return id ? live.filter((t) => t.sprint === id) : []; }
  return live.filter((t) => t.sprint === view);
};
export const liveTickets = () => S().tickets.filter((t) => !t.dropped);
export const lastState = () => S().workflow_states[S().workflow_states.length - 1];

export function droppable(t) {
  const checks = t.checks || [];
  return !t.dropped && t.state !== lastState() && !checks.includes("merged_to_dev") && !checks.includes("released");
}

export const currentTicket = (id) => S().tickets.find((t) =>
  (t.assignee === id && t.state === "in_progress") || (t.reviewer === id && t.state === "in_review"));

export function workingOn(e) {
  const t = currentTicket(e.id);
  const live = S().live?.[e.id];
  const working = e.status === "working" || e.turn_open;
  const step = live?.plan?.find((p) => p.status === "in_progress")?.content;
  const detail = (working ? step || live?.tools?.at(-1)?.title : null) || e.last_progress;
  return [t ? `${t.id} ${t.title}` : null, detail].filter(Boolean).join(" — ") || statusLabel[e.status];
}

/** Chat lines joined with live tool records by tool_call_id. */
export function transcript(id) {
  const live = S().live?.[id]?.tools || [];
  const used = new Set();
  const out = [];
  let n = 0;
  for (const line of S().chats[id] || []) {
    if (line.speaker.kind === "tool") {
      if (!line.tool_call_id) continue;
      const i = live.findLastIndex((t) => t.tool_call_id === line.tool_call_id);
      if (i >= 0) { used.add(i); out.push({ tool: live[i] }); continue; }
      const m = /^(.*) \[([^\]]*)\]$/.exec(line.text);
      out.push({ tool: { tool_call_id: line.tool_call_id, title: m ? m[1] : line.text, status: m ? m[2] : "", content: [] } });
    } else {
      out.push({ msg: line, key: `m${n++}` });
    }
  }
  live.forEach((t, i) => { if (!used.has(i)) out.push({ tool: t }); });
  return out;
}

export function relTime(iso) {
  if (!iso) return "";
  const s = Math.max(0, (Date.now() - new Date(iso).getTime()) / 1000);
  if (s < 60) return "just now";
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
}

const actorName = (a) => a.kind === "stakeholder" ? "you" : a.kind === "system" ? "chillax" : nameOf(a.id);
const short = (c) => String(c).slice(0, 7);
const oneLine = (t) => String(t).replace(/\s+/g, " ").trim();

export function inboxTitle(item) {
  switch (item.type) {
    case "permission": return `${nameOf(item.employee)} wants to: ${item.title}`;
    case "question": return `${nameOf(item.employee)} asks: ${item.question}`;
    case "sprint_gate": return `Sprint ${item.sprint} awaits approval`;
    case "release_gate": return `Release ${S().releases?.find((r) => r.id === item.release)?.version || item.release} awaits approval`;
    case "env_alert": return `The ${item.env} environment's ${item.step} failed`;
    case "alert": return item.title;
    default: return item.id;
  }
}

/** 1234 -> "1.2k", 2500000 -> "2.5M" (mirrors core's format_tokens). */
export function fmtTokens(n) {
  n = Number(n) || 0;
  if (n >= 1e6) return `${(n / 1e6).toFixed(1)}M`;
  if (n >= 1e3) return `${(n / 1e3).toFixed(n >= 1e4 ? 0 : 1)}k`;
  return String(n);
}
/** Integer micros -> "$0.18" (USD; other currencies print their code). */
export function fmtMoney(micros, currency = "USD") {
  const v = (Number(micros) || 0) / 1e6;
  const d = v > 0 && v < 0.01 ? 4 : 2;
  return currency === "USD" ? `$${v.toFixed(d)}` : `${v.toFixed(d)} ${currency}`;
}

/** One-line description of an event for the log and the activity feed. */
export function summary(e) {
  switch (e.type) {
    case "employee_hired": return `hired ${e.employee.name} as ${e.employee.role_title}`;
    case "role_adopted": return `adopted role ${e.title}${e.source ? ` from ${e.source.catalog}` : ""}${e.base_role ? ` on ${e.base_role}` : ""}`;
    case "employee_let_go": return `fired ${e.employee}`;
    case "employee_agent_changed": return `${nameOf(e.employee)} now runs on ${[e.backend, e.model, e.effort].filter(Boolean).join(" · ")}`;
    case "hiring_defaults_set": return `new hires use ${e.backend || "each role's agent"}${e.model ? ` · ${e.model}` : ""}`;
    case "chat_message_sent": return `${actorName(e.actor)} → ${nameOf(e.to)}: ${e.text}`;
    case "agent_message_chunk": return `${nameOf(e.employee)}: ${e.text}`;
    case "agent_tool_call": return `${nameOf(e.employee)} ⚙ ${e.title}`;
    case "agent_tool_call_updated": return `${nameOf(e.employee)} ⚙ ${e.status || "updated"}`;
    case "agent_turn_ended": return `${nameOf(e.employee)} finished (${e.stop_reason})`;
    case "agent_started": return `${nameOf(e.employee)} online${e.agent ? ` (${e.agent})` : ""}`;
    case "agent_stopped": return `${nameOf(e.employee)}'s session stopped`;
    case "agent_failed": return `${nameOf(e.employee)} failed: ${e.error}`;
    case "permission_requested": return `${e.id} ${nameOf(e.employee)} asks: ${e.title}`;
    case "permission_resolved": return `${e.id} → ${e.choice || "cancelled"}${e.automatic ? " (policy)" : ""}`;
    case "question_asked": return `${e.id} ${nameOf(e.employee)} asks: ${e.question}`;
    case "question_answered": return `${e.id} answered`;
    case "epic_created": return `epic ${e.epic.id} ${e.epic.title}`;
    case "ticket_created": return `${e.ticket.kind} ${e.ticket.id} ${e.ticket.title}`;
    case "max_concurrent_agents_changed": return `agent limit ${e.n} (this machine fits ${e.hardware_max})`;
    case "ticket_prioritized": return `${e.id} ${e.urgent ? "marked urgent" : "no longer urgent"}`;
    case "ticket_updated": return `${e.id} edited`;
    case "ticket_transitioned": return `${e.id} ${e.from} → ${e.to}`;
    case "ticket_assigned": return `${e.id} → ${e.assignee ? nameOf(e.assignee) : "unassigned"}`;
    case "ticket_commented": return `${e.id} 💬 ${e.body}${e.attachments?.length ? ` (+${e.attachments.length} image${e.attachments.length > 1 ? "s" : ""})` : ""}`;
    case "sprint_proposed": return `sprint ${e.sprint.id} proposed (${e.sprint.tickets.length} tickets)`;
    case "sprint_approved": return `sprint ${e.id} approved`;
    case "sprint_rejected": return `sprint ${e.id} rejected`;
    case "sprint_closed": return `sprint ${e.id} closed (${e.carried.length} tickets carried over)`;
    case "sprint_planning_requested": return "next sprint planning requested";
    case "sprint_planning_briefed": return "project manager briefed to plan the sprint";
    case "request_submitted": return `request ${e.id}: ${e.text}`;
    case "request_briefed": return `project manager briefed on ${e.id}`;
    case "request_abandoned": return `request ${e.id} abandoned`;
    case "request_ticket_linked": return `${e.ticket} filed for ${e.request}`;
    case "ticket_dropped": return `${e.id} dropped: ${e.reason}`;
    case "tracker_linked": return `${e.id} linked to ${e.external.key || e.external.id}`;
    case "employee_context_cleared": return `${nameOf(e.employee)} finished ${e.ticket}; context cleared`;
    case "progress_reported": return `${nameOf(e.employee)}: ${e.summary}`;
    case "ticket_blockers_set": return e.blocked_by.length ? `${e.id} blocked by ${e.blocked_by.join(", ")}` : `${e.id} is no longer blocked`;
    case "ticket_handed_off": return `${e.id} handed off ${nameOf(e.from)} → ${nameOf(e.to)}`;
    case "work_started": return `${nameOf(e.employee)} started ${e.id} on ${e.branch}`;
    case "review_requested": return `${nameOf(e.employee)} requests review of ${e.id}: ${e.summary}`;
    case "tests_run": return `${e.id} tests ${e.skipped ? "skipped (no test command)" : e.passed ? "passed" : "failed"}`;
    case "review_started": return `${e.id} in review with ${nameOf(e.reviewer)}`;
    case "review_submitted": return e.verdict === "approve" ? `${nameOf(e.reviewer)} approved ${e.id}` : `${nameOf(e.reviewer)} requested changes on ${e.id}`;
    case "branch_merged": return `${e.id} merged into dev (${short(e.commit)})`;
    case "merge_failed": return `${e.id} could not merge: ${e.reason}`;
    case "branch_rebased": return `${e.id} ${e.outcome === "rebased" ? "rebased onto dev" : e.outcome === "deferred" ? "rebase deferred" : "rebase conflicted"}`;
    case "branch_synced": return `${e.id} ${({ merged: "synced with dev", tests_failed: "tests failed after syncing with dev", conflict: "conflicts with dev", resolved: "conflict resolved", unresolved: "conflict still unresolved" })[e.outcome] || e.outcome}`;
    case "conflict_briefed": return `${e.id} conflict handed to its developer`;
    case "qa_promotion_requested": return "promotion to qa requested";
    case "qa_round_started": return `QA round ${e.round.number} started on ${short(e.round.commit)} (${e.round.tickets.length} tickets)`;
    case "env_command_run": return `qa ${e.step} ${e.skipped ? "skipped (no command)" : e.ok ? "passed" : "failed"}`;
    case "qa_verdict_recorded": return `${nameOf(e.employee)}: ${e.id} ${e.verdict}`;
    case "bug_filed": return `bug ${e.bug} filed against ${e.story}`;
    case "triage_briefed": return `project manager briefed on failure ${e.fingerprint}`;
    case "triage_bug_filed": return `bug ${e.bug} filed for failure ${e.fingerprint}`;
    case "triage_abandoned": return `no bug was filed for failure ${e.fingerprint}`;
    case "qa_round_closed": return `QA round ${e.number} closed`;
    case "alert_raised": return `${e.id} ⚠ ${e.title}`;
    case "alert_dismissed": return `${e.id} dismissed`;
    case "env_retry_requested": return `retry of ${e.env} ${e.step} requested`;
    case "release_proposed": return `release ${e.release.version} awaits approval (${e.release.tickets.length} tickets)`;
    case "release_approved": return `release ${e.version} approved`;
    case "release_rejected": return `release ${e.id} rejected${e.reason ? `: ${e.reason}` : ""}`;
    case "release_tagged": return `tagged ${e.tag} (${short(e.commit)})`;
    case "deploy_run": return `deploy ${e.skipped ? "skipped (no command)" : e.ok ? "succeeded" : "failed"}`;
    case "dispatch_paused": return "dev loop paused";
    case "dispatch_resumed": return "dev loop resumed";
    case "dispatch_kicked": return "dispatch refreshed";
    case "usage_refresh_requested": return "usage refresh requested";
    case "usage_refreshed": return "usage refreshed";
    case "backend_limited": return `${e.backend} is rate limited${e.until ? ` until ${new Date(e.until).toLocaleTimeString()}` : ""}`;
    case "backend_recovered": return `${e.backend} is available again`;
    case "agent_plan_updated": return `${nameOf(e.employee)} plan: ${e.entries.length} steps`;
    case "agent_tool_call_content": return `${nameOf(e.employee)} ⚙ ${e.tool_call_id} output`;
    case "apps_configured": return `${e.apps.length} apps configured`;
    case "app_requested": return `${e.env} app: ${String(e.want).toLowerCase()} requested`;
    case "app_reported": return e.down ? `${nameOf(e.employee)} reports ${e.env} stopped${e.ticket ? ` (${e.ticket})` : ""}` : `${nameOf(e.employee)} reports ${e.env} at ${e.url}${e.ticket ? ` (${e.ticket})` : ""}`;
    case "app_started": return `${e.env} app running at ${e.url} (${short(e.commit)})`;
    case "app_stopped": return `${e.env} app stopped${e.reason ? `: ${oneLine(e.reason)}` : ""}`;
    case "app_crashed": return `${e.env} app crashed ${e.exit == null ? "without an exit code" : `with exit ${e.exit}`}${e.will_retry ? "; retrying" : "; giving up"}`;
    case "app_restarted": return `${e.env} app restarting: ${oneLine(e.reason || "")}`;
    case "usage_recorded": return `${nameOf(e.employee)} used ${fmtTokens(e.input_tokens)} in / ${fmtTokens(e.output_tokens)} out${e.cost_micros != null ? ` · ${e.estimated ? "~" : ""}${fmtMoney(e.cost_micros, e.currency || "USD")}` : ""}`;
    default: return "";
  }
}
export const NOISY = ["agent_message_chunk", "agent_tool_call_content", "agent_plan_updated", "agent_tool_call_updated"];
