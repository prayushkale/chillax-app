// Usage: tokens and cost over the whole event log, from GET /api/v1/usage.
import { h, fill } from "../dom.js";
import { getJson, apiJson } from "../store.js";
import { relTime, fmtTokens, fmtMoney } from "../util.js";
import { viewShell, ticketLink } from "./common.js";

const NS = "http://www.w3.org/2000/svg";
const svg = (tag, attrs = {}, ...kids) => {
  const el = document.createElementNS(NS, tag);
  for (const [k, v] of Object.entries(attrs)) el.setAttribute(k, v);
  for (const k of kids) if (k != null) el.append(k);
  return el;
};
const text = (s) => document.createTextNode(String(s));

// The chart shows hours instead of days; kept across re-renders.
let hourly = false;
let els, data = null, baseline = null, inflight = false, again = false, failed = false, updatedAt = null;

const sum3 = (t) => t.input_tokens + t.output_tokens + t.cached_tokens;
const wrap = (table) => h("div", { class: "usage-scroll" }, table);
const num = (n) => h("td", { class: "num" }, fmtTokens(n));

/** `cost_micros` is what backends reported; `estimated_cost_micros` comes on top (core: total_cost_micros). */
const totalCost = (t) => (t.cost_micros || 0) + (t.estimated_cost_micros || 0);
function money(t) {
  const total = totalCost(t), est = t.estimated_cost_micros || 0;
  if (!total) return null;
  if (est && !t.cost_micros) return h("span", {}, `est. ${fmtMoney(total)}`);
  if (est) return h("span", {}, `${fmtMoney(total)} `, h("span", { class: "chip" }, `incl. est. ${fmtMoney(est)}`));
  return h("span", {}, fmtMoney(total));
}
const otherCurrency = (t) => {
  const o = Object.entries(t.other_currency_micros || {});
  return o.length ? h("div", { class: "s" }, "also: " + o.map(([c, m]) => `${(m / 1e6).toFixed(2)} ${c}`).join(", ")) : null;
};
const tile = (k, v, ...sub) => h("div", { class: "tile" }, h("div", { class: "k" }, k), h("div", { class: "v" }, v), ...sub.map((s) => (s instanceof Node ? s : h("div", { class: "s" }, s))));

function costTile(d) {
  const c = d.cost;
  const label = c.total_micros === 0 ? "—"
    : c.reported_micros === 0 && c.estimated ? `est. ${fmtMoney(c.total_micros)}`
    : c.estimated ? `${fmtMoney(c.total_micros)}` : fmtMoney(c.total_micros);
  return tile("Cost", label,
    c.estimated && c.reported_micros > 0 ? h("div", { class: "s" }, `incl. est. ${fmtMoney(c.estimated_micros)}`) : null,
    c.estimated ? h("span", { class: "chip" }, "est.") : null,
    c.unpriced_samples ? h("span", { class: "chip muted" }, `${c.unpriced_samples} unpriced`) : null,
    otherCurrency(d.total));
}

function sinceLine(d) {
  const b = baseline, t = d.total;
  const sat = (a, c) => Math.max(0, a - c);
  const di = sat(t.input_tokens, b.input_tokens), dout = sat(t.output_tokens, b.output_tokens), dc = sat(totalCost(t), totalCost(b));
  return h("p", { class: "usage-since" },
    `Since you opened this page: +${fmtTokens(di)} in / +${fmtTokens(dout)} out${dc ? ` · +${fmtMoney(dc)}` : ""} `,
    h("button", { class: "btn small", type: "button", onclick: () => { baseline = { ...data.total }; render(); } }, "Reset"));
}

function chart(d) {
  const days = (hourly ? d.hourly : d.daily).map((x) => ({ ...x, date: hourly ? x.hour.replace("T", " ") + ":00" : x.date }));
  const unit = hourly ? "hour" : "day";
  const vals = days.map((x) => x.input_tokens + x.output_tokens + x.cached_tokens);
  const max = Math.max(...vals, 1);
  const narrow = els.body.clientWidth < 600, W = narrow ? 420 : 960, H = narrow ? 220 : 240, padL = 8, padR = 8, top = 24, bot = 28, bw = (W - padL - padR) / days.length;
  const base = H - bot, ph = base - top;
  const g = svg("svg", { viewBox: `0 0 ${W} ${H}`, class: "bars", role: "img", "aria-label": `Tokens per ${unit} for the last ${days.length} ${unit}s, peak ${fmtTokens(max)}` });
  g.append(svg("text", { x: padL, y: 12, class: "axis" }, text(`${fmtTokens(max)} tokens/${unit}`)));
  days.forEach((x, i) => {
    const hgt = vals[i] ? Math.max(2, (vals[i] / max) * ph) : 0;
    const bx = padL + i * bw + 2, w = Math.max(2, bw - 4);
    const today = i === days.length - 1;
    const tip = `${x.date}: ${fmtTokens(x.input_tokens)} in / ${fmtTokens(x.output_tokens)} out / ${fmtTokens(x.cached_tokens)} cached, ${x.turns} turns${totalCost(x) ? `, ${fmtMoney(totalCost(x))}${x.estimated_cost_micros ? " (est.)" : ""}` : ""}`;
    const grp = svg("g", { tabindex: "0", "aria-label": tip }, svg("title", {}, text(tip)));
    if (hgt) grp.append(svg("rect", { x: bx, y: base - hgt, width: w, height: hgt, rx: 3, class: today ? "bar today" : "bar" }));
    grp.append(svg("rect", { x: bx, y: top, width: w, height: ph, fill: "transparent" }));
    g.append(grp);
    if (today || (days.length - 1 - i) % (hourly ? 6 : 2) === 0) g.append(svg("text", { x: bx + w / 2, y: H - 8, "text-anchor": "middle", class: today ? "axis today" : "axis" }, text(today ? (hourly ? "now" : "today") : hourly ? x.date.slice(5, 10) + " " + x.date.slice(11, 13) + "h" : x.date.slice(5))));
  });
  g.append(svg("line", { x1: padL, x2: W - padR, y1: base, y2: base, class: "baseline" }));
  const table = h("table", { class: "sr-only" }, h("caption", {}, `Tokens per ${unit}`),
    h("tbody", {}, days.map((x, i) => h("tr", {}, h("th", {}, x.date), h("td", {}, `${vals[i]} tokens`), h("td", {}, `${x.turns} turns`)))));
  const toggle = h("button", {
    class: "btn small", type: "button", title: "Switch the chart between per hour and per day",
    onclick: () => { hourly = !hourly; render(); },
  }, hourly ? "tokens/day" : "tokens/hour");
  return h("section", { class: "panel" }, h("h3", {}, `Tokens per ${unit} (UTC, last ${days.length} ${unit}s) `, toggle), h("div", { class: "panel-b" }, g, table));
}

function ctxRow(c) {
  const pct = Math.round(c.percent);
  const lvl = pct > 90 ? "bad" : pct >= 70 ? "warn" : "ok";
  return h("div", { class: "ctxrow" },
    h("span", { class: "ctxname" }, c.name),
    h("div", { class: `ctxbar ${lvl}`, role: "img", "aria-label": `${c.name} context ${pct}% used` }, h("div", { style: `width:${Math.min(100, Math.max(0, pct))}%` })),
    h("span", { class: "ctxtxt" }, `${pct}% · ${fmtTokens(c.used)} / ${fmtTokens(c.size)}`, c.ticket ? " · " : null, c.ticket ? ticketLink({ id: c.ticket }) : null));
}

function stateCell(e) {
  switch (e.state) {
    case "reports_nothing": return h("span", { class: "chip warn" }, `reports no usage (${e.turns_total} turns)`);
    case "no_cost": return h("span", { class: "chip" }, `tokens only · set [pricing."${e.model || "model"}"] for an estimate`);
    case "idle": return h("span", { class: "muted" }, "idle");
    default: return h("span", { class: "muted" }, "ok");
  }
}

const th = (...c) => h("thead", {}, h("tr", {}, c.map((x) => h("th", {}, x))));

function employees(d) {
  const rows = d.employees.map((e) => h("tr", {},
    h("td", {}, h("strong", {}, e.name), " ", h("span", { class: "muted" }, e.role_title)),
    h("td", { class: "muted" }, [e.backend, e.model].filter(Boolean).join(" · ")),
    num(e.totals.input_tokens), num(e.totals.output_tokens), num(e.totals.cached_tokens),
    h("td", { class: "num" }, money(e.totals) || h("span", { class: "faint" }, "—"), otherCurrency(e.totals)),
    h("td", { class: "num" }, e.turns_total),
    h("td", {}, stateCell(e))));
  return h("section", { class: "panel" }, h("h3", {}, "By employee"),
    wrap(h("table", { class: "tbl usage-table" }, th("Employee", "Backend", "In", "Out", "Cached", "Cost", "Turns", "Note"), h("tbody", {}, rows))));
}

function tickets(d) {
  const rows = d.tickets.map((t) => h("tr", {}, h("td", {}, ticketLink(t)), h("td", {}, t.title || h("span", { class: "faint" }, "(no title)")),
    num(t.totals.input_tokens), num(t.totals.output_tokens), h("td", { class: "num" }, money(t.totals) || h("span", { class: "faint" }, "—"))));
  return h("section", { class: "panel" }, h("h3", {}, "By ticket"),
    d.tickets.length ? wrap(h("table", { class: "tbl usage-table" }, th("Ticket", "Title", "In", "Out", "Cost"), h("tbody", {}, rows))) : h("p", { class: "empty pad" }, "No ticket-attributed usage."));
}

function models(d) {
  const rows = d.models.map((m) => h("tr", {}, h("td", {}, m.model), num(m.totals.input_tokens), num(m.totals.output_tokens), num(m.totals.cached_tokens), h("td", { class: "num" }, money(m.totals) || h("span", { class: "faint" }, "—"))));
  return h("section", { class: "panel" }, h("h3", {}, "By model"),
    d.models.length ? wrap(h("table", { class: "tbl usage-table" }, th("Model", "In", "Out", "Cached", "Cost"), h("tbody", {}, rows))) : h("p", { class: "empty pad" }, "No model reported."));
}

function recent(d) {
  const items = d.recent.slice(0, 20); // the server sends newest first
  return h("section", { class: "panel" }, h("h3", {}, "Recent samples"),
    items.length ? h("ol", { class: "log compact usage-recent" }, items.map((r) => h("li", {},
      h("span", { class: "sum" }, `${r.employee_name} · ${fmtTokens(r.input_tokens)} in / ${fmtTokens(r.output_tokens)} out${r.cost_micros != null ? ` · ${r.estimated ? "~" : ""}${fmtMoney(r.cost_micros, r.currency || "USD")}` : ""}${r.model ? ` · ${r.model}` : ""}${r.ticket ? ` · ${r.ticket}` : ""}`),
      h("span", { class: "faint" }, relTime(r.at))))) : h("p", { class: "empty pad" }, "No samples yet."));
}

function render() {
  const d = data;
  if (!d) { fill(els.body, h("p", { class: "empty" }, failed ? "Could not load usage." : "Loading…")); return; }
  const noData = d.total.turns === 0 && !d.recent.length;
  if (noData) {
    fill(els.body, h("div", { class: "empty-state usage-empty" }, h("strong", {}, "No usage recorded yet"), h("p", { class: "muted" }, "Usage appears here once an agent finishes a turn. Not every agent reports tokens or cost; add [pricing.\"model\"] to chillax.toml to get estimates.")));
    return;
  }
  const w = d.windows, t = d.total;
  const todayT = w.today, week = w.last_7_days;
  fill(els.body,
    h("div", { class: "tiles" },
      tile("Total tokens", fmtTokens(sum3(t)), `${fmtTokens(t.input_tokens)} in / ${fmtTokens(t.output_tokens)} out / ${fmtTokens(t.cached_tokens)} cached`, "all time"),
      costTile(d),
      tile("Today (UTC)", fmtTokens(sum3(todayT)), totalCost(todayT) ? fmtMoney(totalCost(todayT)) : `${todayT.turns} turns`),
      tile("Last 7 days", fmtTokens(sum3(week)), totalCost(week) ? fmtMoney(totalCost(week)) : `${week.turns} turns`),
      tile("Burn rate", `${fmtTokens(d.burn.tokens_per_day_7d)}/day`, d.burn.cost_micros_per_day_7d ? `${fmtMoney(d.burn.cost_micros_per_day_7d)}/day` : "7-day average")),
    sinceLine(d),
    chart(d),
    h("section", { class: "panel" }, h("h3", {}, "Context windows"),
      d.context.length ? h("div", { class: "panel-b ctxlist" }, d.context.map(ctxRow)) : h("p", { class: "empty pad" }, "No live session reports its context window.")),
    employees(d),
    h("div", { class: "grid2 two" }, tickets(d), models(d)),
    recent(d),
    h("p", { class: "muted usage-foot" }, `Updated ${new Date(updatedAt).toISOString().slice(11, 19)} UTC · UTC days · all time since the event log began`));
  if (failed) els.body.prepend(h("p", { class: "usage-err", role: "alert" }, "Could not refresh usage; showing the last data."));
}

const QUERY = "days=14";

/** `refresh` first asks the server to record what live sessions still owe (POST /usage/refresh). */
async function load(refresh = false) {
  if (inflight) { again = true; return; }
  inflight = true;
  try {
    data = refresh
      ? await apiJson(`/usage/refresh?${QUERY}`, { method: "POST" })
      : await getJson(`/usage?${QUERY}`);
    failed = false;
    updatedAt = Date.now();
    baseline ||= { ...data.total };
  } catch { failed = true; }
  inflight = false;
  render();
  if (again) { again = false; load(); }
}

window.addEventListener("resize", () => { if (data && els.body.offsetParent) render(); });

export default {
  id: "usage", title: "Usage", icon: "usage",
  mount(root) {
    const refreshBtn = h("button", {
      type: "button", class: "btn small",
      title: "Record what running agents have reported so far, then reload these numbers",
      onclick: async () => {
        refreshBtn.disabled = true;
        refreshBtn.textContent = "Refreshing…";
        refreshBtn.setAttribute("aria-busy", "true");
        await load(true);
        refreshBtn.disabled = false;
        refreshBtn.textContent = "Refresh";
        refreshBtn.removeAttribute("aria-busy");
      },
    }, "Refresh");
    const { head, body } = viewShell("Usage", refreshBtn);
    els = { body };
    root.append(head, body);
  },
  update() {
    if (!els.body.closest(".view")?.classList.contains("active")) return;
    load();
  },
};
