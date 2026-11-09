// Tiny hash router: #/board, #/live/dev. Views register themselves in main.js.
export const views = [];
const listeners = new Set();
export const onNav = (fn) => listeners.add(fn);
export function parse() {
  const [id, arg] = location.hash.replace(/^#\/?/, "").split("/");
  return { id: views.some((v) => v.id === id) ? id : "dashboard", arg: arg ? decodeURIComponent(arg) : null };
}
export function go(id, arg) {
  const next = `#/${id}${arg ? `/${encodeURIComponent(arg)}` : ""}`;
  if (location.hash === next) listeners.forEach((fn) => fn(parse())); else location.hash = next;
}
window.addEventListener("hashchange", () => listeners.forEach((fn) => fn(parse())));
