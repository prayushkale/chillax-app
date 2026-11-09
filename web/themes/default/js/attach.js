// Image attachments: picker, drag and drop, paste.
import { h, fill } from "./dom.js";
import { api, send } from "./store.js";
import { toast } from "./ui.js";

const IMAGE_TYPES = ["image/png", "image/jpeg", "image/gif", "image/webp"];
const MAX_IMAGE_BYTES = 10 * 1024 * 1024;
const MAX_IMAGES = 6;

async function uploadImage(file) {
  if (!IMAGE_TYPES.includes(file.type)) { toast(`${file.name || "That file"} is not a png, jpeg, gif or webp image.`); return null; }
  if (file.size > MAX_IMAGE_BYTES) { toast(`${file.name || "That image"} is over 10 MB.`); return null; }
  let resp;
  try {
    resp = await api("/attachments", { method: "POST", headers: { "X-Filename": encodeURIComponent(file.name || "pasted-image.png") }, body: file });
  } catch { toast("Can't reach Chillax. Try again."); return null; }
  const body = await resp.json().catch(() => ({}));
  if (!resp.ok) { toast(body.error || `Upload failed (${resp.status})`); return null; }
  return body;
}
const imageFiles = (list) => [...(list || [])].filter((f) => f.type && f.type.startsWith("image/"));
const url = (a) => `/api/v1/attachments/${a.id}`;
export const attachmentLink = (a) => h("a", { href: url(a), target: "_blank", rel: "noopener noreferrer", title: `${a.name} (${Math.ceil(a.bytes / 1024)} KB)` },
  h("img", { class: "thumb", src: url(a), alt: a.name, loading: "lazy" }));

/** A comment box for one ticket. Created once per drawer so a draft survives live updates. */
export function commentBox(ticketId) {
  const staged = [];
  const text = h("textarea", { rows: 3, placeholder: "Comment. Paste or drop images here.", "aria-label": "Comment" });
  const chips = h("div", { class: "chips" });
  const draw = () => fill(chips, staged.map((a, i) => h("span", { class: "chip" }, a.name,
    h("button", { type: "button", title: "Remove", "aria-label": `Remove ${a.name}`, onclick: () => { staged.splice(i, 1); draw(); } }, "×"))));
  async function add(files) {
    for (const f of imageFiles(files)) {
      if (staged.length >= MAX_IMAGES) { toast(`A comment takes at most ${MAX_IMAGES} images.`); break; }
      const a = await uploadImage(f);
      if (a) { staged.push(a); draw(); }
    }
  }
  const picker = h("input", { type: "file", accept: IMAGE_TYPES.join(","), multiple: true, hidden: true, onchange: () => { add(picker.files); picker.value = ""; } });
  text.addEventListener("paste", (ev) => {
    const files = imageFiles(ev.clipboardData && ev.clipboardData.files);
    if (files.length) { ev.preventDefault(); add(files); }
  });
  const post = async () => {
    if (!text.value.trim() && !staged.length) { toast("Write something or attach an image."); return; }
    const r = await send({ type: "comment_ticket", id: ticketId, body: text.value.trim(), attachments: staged });
    if (r) { text.value = ""; staged.length = 0; draw(); }
  };
  text.addEventListener("keydown", (e) => { if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) { e.preventDefault(); post(); } });
  const box = h("div", { class: "comment-box" }, text, chips,
    h("div", { class: "actions" },
      h("button", { type: "button", class: "btn small", onclick: () => picker.click() }, "Attach image"), picker,
      h("button", { type: "button", class: "btn small primary", onclick: post, title: "Ctrl or Cmd + Enter" }, "Comment")));
  for (const ev of ["dragenter", "dragover"]) box.addEventListener(ev, (e) => { e.preventDefault(); box.classList.add("drop"); });
  box.addEventListener("dragleave", () => box.classList.remove("drop"));
  box.addEventListener("drop", (e) => { e.preventDefault(); box.classList.remove("drop"); add(e.dataTransfer.files); });
  return box;
}
