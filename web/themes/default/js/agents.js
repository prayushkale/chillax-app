// Coding agent, model and effort controls.
import { h, fill, spinner } from "./dom.js";
import { apiJson, postJson } from "./store.js";

export let agentList = [];
const modelCache = {};
const effortCache = {};
export async function loadAgents() {
  const note = h("span", { class: "muted", id: "agents-loading" }, spinner("Looking for agents"), "Looking for agents…");
  document.querySelector("#refresh")?.after(note);
  try { agentList = await apiJson("/agents").catch(() => []); } finally { note.remove(); }
}

function agentModels(backend) {
  modelCache[backend] ??= apiJson(`/agents/${encodeURIComponent(backend)}/models`).catch((err) => { delete modelCache[backend]; throw err; });
  return modelCache[backend];
}
/** Effort (thought level) choices; `[]` when the agent has none or listing them failed. */
function agentEfforts(backend) {
  effortCache[backend] ??= apiJson(`/agents/${encodeURIComponent(backend)}/efforts`).catch(() => { delete effortCache[backend]; return []; });
  return effortCache[backend];
}
/** Add an installed preset to chillax.toml before it is used. */
export async function ensureAgent(backend) {
  const b = agentList.find((x) => x.name === backend);
  if (b && !b.configured) { await postJson(`/agents/${encodeURIComponent(backend)}/enable`, {}); await loadAgents(); }
}
export const agentFields = ({ backend, model, effort }) => (backend ? { backend, ...(model ? { model } : {}), ...(effort ? { effort } : {}) } : {});

/** Agent, model and effort inputs starting at `initial`; the model field suggests the agent's models, and the effort select shows only when the agent offers levels. */
export function agentPicker(initial, { disabled = false, onchange } = {}) {
  const value = { backend: initial?.backend || "", model: initial?.model || "", effort: initial?.effort || "" };
  const listId = `models-${Math.random().toString(36).slice(2)}`;
  const datalist = h("datalist", { id: listId });
  const note = h("span", { class: "muted" });
  const modelInput = h("input", { disabled: disabled || !value.backend, list: listId, "aria-label": "Model", placeholder: "model (agent's default)", title: "Model on this agent; leave empty for its default",
    onchange: (e) => { value.model = e.target.value.trim(); resetEffort(); onchange?.(value); } });
  modelInput.value = value.model;
  const effortSelect = h("select", { hidden: true, disabled, title: "Effort (thought level) on this agent; the list is for the agent's default model", "aria-label": "Effort",
    onchange: (e) => { value.effort = e.target.value; onchange?.(value); } });
  /** Effort levels differ per backend and model, so a changed pick starts over. */
  function resetEffort() { value.effort = ""; effortSelect.value = ""; }
  function showEfforts(efforts) {
    const known = efforts.filter((x) => x.id !== "default");
    const extra = value.effort && !known.some((x) => x.id === value.effort) ? [{ id: value.effort, name: value.effort }] : [];
    fill(effortSelect, h("option", { value: "" }, "Effort: agent's default"), [...known, ...extra].map((x) => h("option", { value: x.id, selected: x.id === value.effort }, x.name)));
    effortSelect.hidden = !known.length;
  }
  const backendSelect = h("select", { disabled, title: "Coding agent", "aria-label": "Coding agent", onchange: async (e) => {
      value.backend = e.target.value; value.model = ""; modelInput.value = ""; modelInput.disabled = disabled || !value.backend;
      resetEffort(); effortSelect.hidden = true;
      onchange?.(value); await fillModels();
    } },
    h("option", { value: "" }, "Each role's own agent"),
    agentList.map((b) => h("option", { value: b.name, selected: b.name === value.backend }, b.configured ? `${b.title} (${b.name})` : `${b.title} — adds [agents.${b.name}] to chillax.toml`)));
  async function fillModels() {
    fill(datalist); note.textContent = ""; effortSelect.hidden = true;
    const b = agentList.find((x) => x.name === value.backend);
    if (!b?.configured) return;
    fill(note, spinner("Loading models"), `Starting ${b.title} to list its models…`);
    modelInput.disabled = true;
    try {
      const models = await agentModels(b.name);
      fill(datalist, models.filter((m) => m.id !== "default").map((m) => h("option", { value: m.id }, m.name)));
      note.textContent = "";
      showEfforts(await agentEfforts(b.name));
    } catch (err) { note.textContent = err.message; }
    modelInput.disabled = disabled || !value.backend;
  }
  fillModels();
  return { el: [backendSelect, modelInput, effortSelect, datalist, note], value };
}
