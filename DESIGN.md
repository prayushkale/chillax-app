# DESIGN.md — Chillax default web theme

Visual and interaction direction for `web/themes/default/` (vanilla HTML/CSS and native ES
modules, no build step, no framework, no web fonts). Read it before changing `style.css`,
`index.html` or anything under `js/`. `style.css` holds the token values; if this file and
the CSS disagree, the CSS wins and this file gets fixed.

Mode: **Operate**. The stakeholder lives in this app all day: scanability, density and
keyboard speed beat decoration. Brand shows in details (the mint accent, the status dots,
the quiet motion), not in ornament.

## 1. Principles

1. **Dense, not cramped.** 13px body, 28px rows, 4px spacing grid. A board column shows
   10+ tickets on a laptop. Whitespace separates groups, never pads single rows.
2. **Dark first, light equal.** Dark is the default look; `prefers-color-scheme: light`
   swaps every token, and a sidebar toggle (System, Light, Dark) overrides it via
   `data-theme` on `<html>`, remembered in `localStorage` (`chillax.theme`). Dark surfaces get lighter with elevation; shadows only on floating
   layers (palette, drawer, toast).
3. **Quiet chrome, loud state.** Neutral greys for structure. Colour is reserved for state:
   mint = active/primary, amber = waiting on you, red = failed, green = done/ok, blue =
   information. Status always has a dot *and* a word.
4. **One source of truth per thing.** A ticket opens in the same side drawer from the
   board, inbox, palette, requests and sprint gate. A transcript renders the same in Chat
   and Live.
5. **Controls tell the truth.** An action is shown only when it is available; otherwise it
   is hidden or disabled with a `title` that says why.
6. **Stable under updates.** Live events must never steal focus or scroll. Lists are
   keyed; an item re-renders only when its data changed; text inputs are created once.
7. **Keyboard operable.** Everything reachable by Tab; `Ctrl/Cmd+K` opens a command
   palette; `g` + letter jumps to a view; `Esc` closes layers.

## 2. Layout

```
┌────────┬──────────────────────────────────────────────┬────────────┐
│ side   │ view header: title · view actions            │            │
│ nav    ├──────────────────────────────────────────────┤  drawer    │
│ 208px  │ view body (scrolls inside)                   │  (ticket)  │
│        │                                              │  440px     │
└────────┴──────────────────────────────────────────────┴────────────┘
```

There is no global topbar: each view draws its own 48px `.vhead` (title left, actions
right). The sidebar sits on `--bg`, one step quieter than the content panels.

- **Sidebar** 208px: company name, nav (Dashboard, Chat, Board, Live, Team, Inbox with
  badge, Environments, Events, Usage, Settings), then Refresh, Pause, the theme toggle and
  the connection dot.
  Below 900px it collapses to a 48px icon rail; below 600px it becomes a bottom bar.
- **Drawer** slides over the right edge (overlay below 1100px, docked above). Focus moves
  into it and returns to the opener on close.
- **Palette** centred, 560px, fuzzy search over views, tickets, people and actions.

## 3. Colour tokens

| Token | Dark | Light | Use |
|---|---|---|---|
| `--bg` | `#0d0f11` | `#f6f7f8` | App background |
| `--surface` | `#14171a` | `#ffffff` | Panels, cards, inputs |
| `--surface-2` | `#1a1e22` | `#eff1f3` | Column wells, code, hover base |
| `--hover` | `#21262b` | `#e6e9ec` | Row hover, pressed |
| `--line` | `#262c32` | `#dde1e5` | Hairlines |
| `--line-strong` | `#38404a` | `#c3c9cf` | Input borders |
| `--text` | `#e8ebee` | `#14181b` | Body |
| `--muted` | `#9aa3ad` | `#566069` | Secondary text (>= 7:1 dark, 6:1 light) |
| `--faint` | `#7b8590` | `#6b747d` | Tertiary, placeholders (>= 4.5:1) |
| `--accent` | `#4fd1b0` | `#0c7c66` | Primary, active nav, focus |
| `--accent-ink` | `#04120e` | `#ffffff` | Text on accent |
| `--accent-soft` | `#4fd1b01f` | `#0c7c661a` | Active nav / selection wash |
| `--ok` `--warn` `--bad` `--info` | green / amber / red / blue | darker equivalents | state text and dots |

All text pairs are AA. Colour never carries meaning alone.

## 4. Type, space, shape

- `--font`: `ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif`; `--mono`:
  `ui-monospace, "SF Mono", Menlo, Consolas, monospace`.
- Scale: 11px (labels, pills), 12px (meta, tables), 13px (body, controls), 15px (drawer and
  view titles), 20px (empty states only). Weights 400/500/600. Uppercase labels 11px with
  .04em tracking, `--faint`.
- Spacing 4/8/12/16/24. Radii 4 (pills, inputs), 6 (cards), 10 (palette, dialogs).
- Row height 28px; board card padding 8px; controls 28px high, 24px for `small`.

## 5. Components

- **Status dot + label**: 8px dot (`working` mint pulsing, `idle` green, `waiting` amber,
  `failed` red, `offline` faint, `starting` blue). Pulse stops under reduced motion.
- **Pill**: 11px, `--surface-2` fill, optional tint for kind (story blue, bug red, task
  muted) and sprint/PR state.
- **Board column header**: state dot (first faint, middle accent, last green), name, count.
  Empty columns show a dashed "Nothing here" box at the top, not the bottom.
- **Board card**: top row id (mono, faint) and assignee avatar; title (2 lines max, 500
  weight); one wrapping row of pills (kind, estimate, epic, then tests, review, merged,
  blocked badges). Click or Enter opens the drawer. Selected card has an accent left edge.
- **Stat tile** (`.stat`, `.tile`): 11px uppercase label, 22px tabular number, 12px
  sub-line. A clickable tile is a button; `.warn` tints the number amber when something
  needs the stakeholder (Inbox not empty, loop paused).
- **Empty state** (`.empty-state`): bold one-line title, one muted sentence saying what
  will appear or what to do, at most one button. Used for Inbox, Usage and Settings (no
  team yet). Inline "nothing yet" lines inside panels stay plain `.empty` text.
- **Theme toggle**: ghost button in the sidebar foot cycling System, Light, Dark. Hidden
  in the bottom bar (below 600px), where the OS preference applies.
- **Transcript** (Chat, Live): assistant text as plain paragraphs, stakeholder messages as
  right-tinted blocks, tool calls as one-line `<details>` rows: kind glyph, title, status;
  expanding shows output or a +/- diff in mono. The last tool call with output is open.
- **Composer**: auto-growing textarea (up to 12 lines), Enter sends, Shift+Enter newline,
  a hint line, Send disabled while empty.
- **Drawer sections**: header (id, kind, title), facts grid, PR link, description,
  acceptance criteria, blockers, failures and bug links, comments with thumbnails, comment
  box (picker, drag-drop, paste), actions (assign, move, edit, drop).
- **Dialogs** use `<dialog>` (focus trap and Esc from the platform).
- **Toast** bottom-centre, `role=status`, 5s.

## 6. Motion

120ms ease-out for hover and colour, 180ms slide for the drawer and palette. Nothing
loops except the working-dot pulse. `prefers-reduced-motion: reduce` removes all of it.

## 7. Accessibility

- Visible 2px `--accent` focus ring (`:focus-visible`), offset 1px, on every control.
- Landmarks: `nav`, `main`, `aside`; live regions for toast and connection; drawer and
  palette are dialogs with labelled titles; icon-only buttons carry `aria-label`.
- Hit targets 28px minimum (24px for dense inline actions that have a 28px row).
- Contrast verified per token table; text on tinted pills uses `--text`.

## 8. Do and don't

- Do reuse tokens; a literal colour in a rule is a bug.
- Do keep copy short and concrete ("Plan next sprint", not "Get started").
- Don't add shadows, gradients, glass or card-in-card nesting. Don't add an icon font;
  glyphs are inline SVG paths or Unicode already in the system fonts.
- No inline `style` attributes in `js/` except values computed from data (progress
  widths, textarea height). Use a class from the helpers block at the end of `style.css`.
- Don't re-render a region that holds focus or a half-typed value.
