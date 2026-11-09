// Light / dark / system switch. "system" removes data-theme so prefers-color-scheme decides.
const KEY = "chillax.theme";
export const MODES = ["system", "light", "dark"];

export function savedTheme() {
  try {
    const v = localStorage.getItem(KEY);
    return MODES.includes(v) ? v : "system";
  } catch { return "system"; /* storage blocked */ }
}

export function applyTheme(mode) {
  const root = document.documentElement;
  if (mode === "system") root.removeAttribute("data-theme"); else root.setAttribute("data-theme", mode);
  try {
    if (mode === "system") localStorage.removeItem(KEY); else localStorage.setItem(KEY, mode);
  } catch { /* storage blocked */ }
}

/** The mode after `mode` in the system -> light -> dark cycle. */
export const nextTheme = (mode) => MODES[(MODES.indexOf(mode) + 1) % MODES.length];
