// Persona catalog search and the review dialog. Persona text comes from a
// public repository and is untrusted: it is only ever shown with textContent, and a persona
// is hired only through the review dialog.
import { h, fill, $, spinner } from "./dom.js";
import { store, apiJson, postJson, loadRoles } from "./store.js";
import { agentPicker, ensureAgent, agentFields } from "./agents.js";
import { confirmBox, toast } from "./ui.js";

let personaQuery = "";
let review = null; // { persona, draft, base, view, busy, error, name, roleId }
const roles = () => store.roles;
export async function loadCatalog() {
  try {
    const s = await apiJson("/catalog");
    if (!s.name) throw new Error("catalog disabled"); // an off catalog reports no name
    const parts = [`${s.name} @ ${(s.commit || "—").slice(0, 7)}`, `${s.personas} personas`];
    if (s.fetched_at) parts.push(`updated ${new Date(s.fetched_at).toLocaleString()}`);
    if (s.refreshing) parts.push("refreshing…");
    if (s.error) parts.push(`offline, using cache (${s.error})`);
    $("#catalog-status").textContent = parts.join(" · ");
    // A startup refresh may still be running: check back.
    if (s.refreshing) setTimeout(loadCatalog, 2000);
  } catch (err) {
    // Catalog disabled or unreachable: there is nothing to search, so hide the panel.
    $("#catalog-status").closest("section").hidden = true;
    return;
  }
  await searchPersonas();
}

let searchSeq = 0;
async function searchPersonas() {
  const seq = ++searchSeq;
  const q = personaQuery.trim();
  let list;
  if (!$("#personas").children.length) fill($("#personas"), h("li", { class: "empty" }, spinner("Searching personas"), "Searching personas…"));
  try {
    list = await apiJson(`/personas?limit=${q ? 50 : 10}&q=${encodeURIComponent(q)}`);
  } catch (err) {
    if (seq === searchSeq) fill($("#personas"), h("li", { class: "empty" }, err.message));
    return;
  }
  if (seq !== searchSeq) return; // a newer search is on its way
  fill($("#personas"), list.length ? list.map(personaItem)
    : h("li", { class: "empty" }, q ? "No personas match." : "No featured personas yet."));
}

function personaItem(p) {
  return h("li", { tabindex: 0, role: "button", onclick: () => openReview(p), onkeydown: (e) => { if (e.key === "Enter") openReview(p); }, title: p.vibe || p.description },
    h("span", { class: "name" }, `${p.emoji ? `${p.emoji} ` : ""}${p.name}`),
    h("span", { class: "pill" }, p.division),
    p.adopted_role ? h("span", { class: "pill" }, `adopted as ${p.adopted_role}`) : null,
    p.hires ? h("span", { class: "pill" }, `hired ${p.hires}×`) : null,
    h("span", { class: "desc" }, p.description));
}

// Only trusted roles (built-in or hand-written) can lend their policy to a persona.
const trustedRoles = () => roles().filter((r) => !r.source);

async function openReview(persona) {
  // The server picks the base (persona.suggested_base) unless the stakeholder changes it.
  const defaults = store.state?.hiring || {};
  review = { persona, draft: null, base: undefined, agent: { backend: defaults.backend || "", model: defaults.model || "" },
    view: "persona", busy: null, error: null, name: "", roleId: "" };
  $("#persona-dialog").showModal();
  await makeDraft();
}

/** "＋ New role based on…": pick any role, name the copy, then the usual review. */
export function openNewRole() {
  const first = roles().find((r) => r.id === "project-manager") || roles()[0];
  const pick = h("select", { "aria-label": "Based on" }, roles().map((r) => h("option", { value: r.id, selected: r === first }, r.title)));
  const idIn = h("input", { placeholder: "new role id, e.g. rust-developer", "aria-label": "Role id" });
  const titleIn = h("input", { placeholder: "Title, e.g. Rust Developer", "aria-label": "Title" });
  const err = h("span", { class: "error" });
  const close = () => $("#persona-dialog").close();
  fill($("#review"),
    h("header", {}, h("h2", {}, "New role based on an existing one")),
    h("p", { class: "muted" }, "The role's prompt becomes the text you review and revise. The new role keeps the policy (tools, permissions, states) of a trusted base role."),
    h("div", { class: "row" }, h("label", {}, "Based on: "), pick),
    h("div", { class: "row" }, idIn, titleIn),
    h("div", { class: "actions" },
      h("button", { type: "button", class: "primary", onclick: async () => {
        const newId = idIn.value.trim(), newTitle = titleIn.value.trim();
        if (!newId) { err.textContent = "The role needs an id"; return; }
        if (!newTitle) { err.textContent = "The role needs a title"; return; }
        const base = roles().find((r) => r.id === pick.value);
        const defaults = store.state?.hiring || {};
        review = { from: pick.value, newId, newTitle, base: undefined,
          persona: { name: newTitle, emoji: "", division: `based on ${base?.title || pick.value}`, catalog: "", path: "", description: base?.description || "" },
          draft: null, agent: { backend: defaults.backend || "", model: defaults.model || "" }, view: "persona", busy: null, error: null, name: "", roleId: "" };
        await makeDraft();
      } }, "Review"),
      h("button", { type: "button", onclick: close }, "Cancel"),
      err));
  $("#persona-dialog").showModal();
}

async function makeDraft() {
  await act("Preparing the review…", async () => {
    if (review.agent.backend) await ensureAgent(review.agent.backend);
    // A new role copied from an existing one needs no catalog.
    if (review.from) return postJson("/drafts", { from_role: review.from, role_id: review.newId, title: review.newTitle, ...agentFields(review.agent) });
    return postJson("/drafts", { persona: review.persona.slug, base_role: review.base, ...agentFields(review.agent) });
  });
  if (review?.draft) { review.roleId = review.draft.role_id; review.base = review.draft.base_role; }
  renderReview();
}

/** Move the draft onto another base role; revisions are kept. */
async function changeBase(id) {
  const d = review.draft;
  if (!d) { review.base = id; return makeDraft(); }
  await act("Changing the base role…", () => postJson(`/drafts/${d.id}/base`, { base_role: id }));
  if (review) { review.base = review.draft.base_role; renderReview(); }
}

/** Run one recruiter call with the dialog marked busy; keeps the old draft on errors. */
async function act(label, call) {
  review.busy = label;
  review.error = null;
  renderReview();
  try {
    const draft = await call();
    if (review) review.draft = draft;
  } catch (err) {
    if (review) review.error = err.message;
  }
  if (review) { review.busy = null; renderReview(); }
}

function closeReview() {
  if (review?.draft) apiJson(`/drafts/${review.draft.id}`, { method: "DELETE" }).catch(() => {});
  review = null;
  $("#persona-dialog").close();
}

function reviewLines(d, view) {
  if (view === "changes") {
    if (!d.revisions.length) return [h("span", { class: "muted" }, "No changes yet. Suggest one below.")];
    return d.diff.map((l) => h("span", { class: l.op === "added" ? "add" : l.op === "removed" ? "del" : "" },
      `${l.op === "added" ? "+ " : l.op === "removed" ? "- " : "  "}${l.text}`));
  }
  if (view === "prompt") return d.prompt.split("\n").map((l) => h("span", {}, l || " "));
  return d.persona_text.split("\n").map((l, i) => {
    const flags = d.findings.filter((f) => f.line === i + 1);
    const cls = flags.some((f) => f.severity === "warn") ? "warn" : flags.length ? "info" : "";
    return h("span", { class: cls, title: flags.map((f) => f.message).join("; ") || null },
      h("span", { class: "num" }, String(i + 1).padStart(3)), l || " ");
  });
}

function renderReview() {
  if (!review) return;
  const { persona, draft: d } = review;
  const busy = Boolean(review.busy);
  const suggestion = $("#review-suggestion")?.value || "";

  // The agent is fixed per draft (revisions run on it); changing it starts a new one.
  const redraft = async (agent) => {
    if (d?.revisions.length && !(await confirmBox("Start a new review?", "This starts a new review and drops your revisions.", "Start new review", false))) {
      renderReview();
      return;
    }
    if (d) apiJson(`/drafts/${d.id}`, { method: "DELETE" }).catch(() => {});
    review.agent = { ...agent };
    review.draft = null;
    await makeDraft();
  };
  const baseSelect = h("select", { disabled: busy, onchange: (e) => changeBase(e.target.value) },
    trustedRoles().map((r) => h("option", { value: r.id, selected: r.id === review.base },
      r.id === persona.suggested_base ? `${r.title} (suggested)` : r.title)));
  const picker = agentPicker(d ? { backend: d.backend, model: d.model } : review.agent,
    { disabled: busy, onchange: (agent) => redraft(agent) });

  const warns = d ? d.findings.filter((f) => f.severity === "warn") : [];
  const notes = d ? d.findings.length - warns.length : 0;
  const last = d?.revisions[d.revisions.length - 1];

  const textarea = h("textarea", { id: "review-suggestion", rows: 2, disabled: busy || !d,
    placeholder: "Suggest a change in plain words, e.g. \"Focus on Rust and axum; drop the Ollama parts\"" });
  textarea.value = suggestion;
  const nameInput = h("input", { placeholder: "Name, e.g. Bea", oninput: (e) => { review.name = e.target.value; } });
  nameInput.value = review.name;
  const idInput = h("input", { placeholder: "role id", oninput: (e) => { review.roleId = e.target.value; } });
  idInput.value = review.roleId;

  fill($("#review"),
    h("header", {},
      h("h2", {}, `${persona.emoji ? `${persona.emoji} ` : ""}${persona.name}`),
      h("span", { class: "muted" }, review.from ? persona.division : `${persona.division} · ${persona.catalog}/${persona.path}`)),
    h("p", { class: "muted" }, persona.description),
    h("div", { class: "row" },
      h("label", {}, "Base role — its tools, permissions and workflow states: "), baseSelect),
    h("div", { class: "row" },
      h("label", {}, "Agent — revisions run on it, and the new hire works on it: "), picker.el),
    d ? h("div", { class: `findings${warns.length ? "" : " clean"}` },
      warns.length
        ? [h("strong", {}, `⚠ ${warns.length} warning${warns.length === 1 ? "" : "s"}, ${notes} notes. Read the highlighted lines.`),
           h("ul", {}, warns.map((f) => h("li", {}, `Line ${f.line}: ${f.message} — ${f.excerpt}`)))]
        : h("span", {}, `✓ No suspicious instructions found (${notes} notes: links, installs). Still worth a read.`)) : null,
    d?.project_files?.length ? h("p", { class: "muted" }, `Points the hire at: ${d.project_files.join(", ")}`) : null,
    last ? h("p", { class: "revision" }, `✎ ${d.revisions.length} revision(s). Last: "${last.instruction}"\n${last.summary}`) : null,
    d ? h("div", { class: "views" },
      [["persona", "Persona"], ["changes", "Changes"], ["prompt", "Full prompt"]].map(([key, label]) =>
        h("button", { type: "button", class: review.view === key ? "active" : "", onclick: () => { review.view = key; renderReview(); } }, label))) : null,
    d ? h("pre", {}, reviewLines(d, review.view)) : null,
    textarea,
    h("div", { class: "actions" },
      h("button", { type: "button", disabled: busy || !d, onclick: () => {
        const instruction = $("#review-suggestion").value.trim();
        if (!instruction) return;
        act(`Revising with your ${d.backend}${d.model ? ` · ${d.model}` : ""} agent (no tools, no repo access; uses credits)…`, async () => {
          const next = await postJson(`/drafts/${d.id}/revise`, { instruction });
          $("#review-suggestion").value = "";
          review.view = "changes";
          return next;
        });
      } }, "Ask agent to revise"),
      h("button", { type: "button", disabled: busy || !d?.project_files?.length,
        title: d?.project_files?.length ? `Uses excerpts of ${d.project_files.join(", ")}` : "No plan files (plan.md, AGENTS.md, …) found in the project",
        onclick: () => act(`Tailoring to the project with your ${d.backend}${d.model ? ` · ${d.model}` : ""} agent (no tools; uses credits)…`, async () => {
          const next = await postJson(`/drafts/${d.id}/tailor`, {});
          review.view = "changes";
          return next;
        }) }, "Tailor to project"),
      h("button", { type: "button", disabled: busy || !d?.revisions.length, onclick: () =>
        act("Undoing…", () => postJson(`/drafts/${d.id}/undo`, {})) }, "Undo"),
      review.busy ? h("span", { class: "busy" }, spinner(review.busy), review.busy) : null,
      review.error ? h("span", { class: "error" }, `⚠ ${review.error}`) : null),
    h("hr", {}),
    h("div", { class: "row" }, nameInput, idInput),
    persona.adopted_role && persona.adopted_role === review.roleId.trim()
      ? h("p", { class: "muted" }, `Replaces the role adopted earlier as ${persona.adopted_role}; its employees get this prompt in their next session.`) : null,
    h("div", { class: "actions" },
      h("button", { type: "button", class: "primary", disabled: busy || !d, onclick: adopt }, "Adopt & hire"),
      h("button", { type: "button", disabled: busy, onclick: closeReview }, "Cancel")));
}

async function adopt() {
  const { draft: d, persona } = review;
  const name = review.name.trim(), roleId = review.roleId.trim();
  if (!name) { review.error = "Give your new hire a name"; renderReview(); return; }
  if (!roleId) { review.error = "The role needs an id"; renderReview(); return; }
  review.busy = "Adopting the role…";
  review.error = null;
  renderReview();
  try {
    await postJson(`/drafts/${d.id}/adopt`, { role_id: roleId, overwrite: persona.adopted_role === roleId,
      hire: { name, backend: d.backend, ...(d.model ? { model: d.model } : {}) } });
    review = null;
    $("#persona-dialog").close();
    await loadRoles(); searchPersonas();
  } catch (err) {
    review.busy = null;
    review.error = err.message;
    renderReview();
  }
}


export function bindCatalog() {
  let timer = null;
  $("#persona-search").addEventListener("input", (e) => {
    personaQuery = e.target.value;
    clearTimeout(timer);
    timer = setTimeout(searchPersonas, 150);
  });
  $("#catalog-refresh").addEventListener("click", async () => {
    $("#catalog-status").textContent = "Refreshing…";
    try { await postJson("/catalog/refresh", {}); } catch (err) { toast(err.message); }
    loadCatalog();
  });
  $("#persona-dialog").addEventListener("cancel", (e) => { e.preventDefault(); if (!review?.busy) closeReview(); });
}
