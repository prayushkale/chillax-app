// State store: GET /api/v1/state, kept fresh by the SSE stream (throttled refresh).
import { toast } from "./ui.js";

export const api = (path, init) => fetch(`/api/v1${path}`, init);
export async function getJson(path) {
  const resp = await api(path);
  if (!resp.ok) throw new Error(`${path} returned ${resp.status}`);
  return resp.json();
}
export async function apiJson(path, init) {
  const resp = await api(path, init);
  const body = resp.status === 204 ? {} : await resp.json().catch(() => ({}));
  if (!resp.ok) throw new Error(body.error || `Request failed (${resp.status})`);
  return body;
}
export const postJson = (path, body) => apiJson(path, {
  method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body),
});

export const store = { state: null, roles: [], events: [], conn: "connecting", health: null };
const MAX_EVENTS = 500;
const subs = new Set();
/** fn(kind) with kind "state", "event" or "conn". */
export const subscribe = (fn) => { subs.add(fn); return () => subs.delete(fn); };
const notify = (kind) => subs.forEach((fn) => { try { fn(kind); } catch (err) { console.error(err); } });

/** Send a stakeholder command. Resolves to the response body, or null after a toast. */
export async function send(command) {
  let resp;
  try {
    resp = await api("/commands", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(command) });
  } catch {
    toast("Can't reach Chillax. Check that it is still running, then try again.");
    return null;
  }
  const body = await resp.json().catch(() => ({}));
  if (!resp.ok) { toast(body.error || `Request failed (${resp.status})`); return null; }
  return body;
}

let inflight = false, dirty = false, lastRun = 0, timer = null;
export function scheduleRefresh() {
  if (timer) return;
  const wait = Math.max(60, 250 - (Date.now() - lastRun));
  timer = setTimeout(() => { timer = null; refresh(); }, wait);
}
export async function refresh() {
  if (inflight) { dirty = true; return; }
  inflight = true;
  try {
    store.state = await getJson("/state");
    setError(null);
    notify("state");
  } catch {
    setError(store.state ? "Couldn't refresh. What you see may be out of date." : "Couldn't load your company.");
  } finally {
    inflight = false;
    lastRun = Date.now();
    if (dirty) { dirty = false; scheduleRefresh(); }
  }
}
let errorMsg = null;
export const loadError = () => errorMsg;
function setError(m) { errorMsg = m; notify("error"); }

export async function loadRoles() {
  try { store.roles = await getJson("/roles"); notify("state"); } catch { /* keep the old list */ }
}

function setConn(mode) { store.conn = mode; notify("conn"); }

let source = null, retry = 0, retryTimer = null;
/** SSE with our own backoff so a dead server is retried quietly and state resyncs. */
export function connect() {
  clearTimeout(retryTimer);
  const last = store.events.at(-1)?.seq;
  const since = last ?? Math.max(0, (store.state?.last_seq || 0) - 80);
  setConn(retry ? "reconnecting" : "connecting");
  source?.close();
  source = new EventSource(`/api/v1/events?since=${since}`);
  source.addEventListener("open", () => { const was = retry; retry = 0; setConn("live"); if (was) refresh(); });
  source.addEventListener("error", () => {
    source.close();
    setConn("reconnecting");
    retry += 1;
    retryTimer = setTimeout(connect, Math.min(15000, 500 * 2 ** Math.min(retry, 5)));
  });
  source.addEventListener("chillax", (msg) => {
    let event;
    try { event = JSON.parse(msg.data); } catch { return; }
    const tail = store.events.at(-1);
    if (tail && event.seq <= tail.seq) return;
    store.events.push(event);
    if (store.events.length > MAX_EVENTS) store.events.shift();
    notify("event");
    scheduleRefresh();
    if (event.type === "role_adopted") loadRoles();
  });
}
