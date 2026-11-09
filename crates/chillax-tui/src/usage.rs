//! The Usage tab: tokens and cost over the whole event log, derived by
//! `chillax_core::usage::view` so the TUI and the web themes agree.

use chillax_core::usage::{
    ContextRow, EmployeeRow, UsageHealth, UsageTotals, UsageView, format_money, format_tokens, view,
};
use chrono::Utc;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Sparkline, Widget};

use crate::App;
use crate::hire::truncate_line;
use crate::view::panel;

const DAYS: usize = 14;
const BAR: usize = 10;

impl App {
    pub(crate) fn render_usage(&self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let v = view(&self.state, Utc::now(), DAYS, None);
        if self.state.usage.total.turns == 0 && v.recent.is_empty() {
            self.note_scroll_max(0);
            Paragraph::new(vec![
                Line::from("No usage recorded yet."),
                Line::from("Usage appears here once an agent finishes a turn.").dark_gray(),
                Line::from(
                    "Not every agent reports tokens or cost; add [pricing.\"model\"] to the \
                     config to get estimates.",
                )
                .dark_gray(),
            ])
            .wrap(ratatui::widgets::Wrap { trim: false })
            .block(panel("Usage"))
            .render(area, buf);
            return;
        }
        let headline = self.headline_lines(&v);
        if area.height < 6 || area.width < 12 {
            self.note_scroll_max(0);
            Paragraph::new(headline).render(area, buf);
            return;
        }

        let (w, h) = (area.width, area.height);
        // The wide layout does not scroll, so it is used only while every ticket fits.
        if w >= 90 && h >= 24 && v.tickets.len() <= wide_ticket_rows(h) {
            self.note_scroll_max(0);
            self.draw_wide(&v, headline, area, buf);
            return;
        }

        // Narrow or short: stack the panels in one column and scroll. By model and Recent
        // go first when everything does not fit.
        let rows = |n: usize| (n.max(1) + 2) as u16;
        let mut parts: Vec<(Part, u16)> = vec![
            (Part::Headline, 5),
            (Part::Days, 5),
            (Part::Context, rows(v.context.len())),
            (Part::Employees, rows(v.employees.len())),
            (Part::Tickets, rows(v.tickets.len())),
        ];
        let base: u16 = parts.iter().map(|p| p.1).sum();
        let extra = [
            (Part::Models, rows(v.models.len())),
            (Part::Recent, rows(v.recent.len())),
        ];
        if base + extra.iter().map(|p| p.1).sum::<u16>() <= h {
            parts.extend(extra);
        }
        let total: u16 = parts.iter().map(|p| p.1).sum();
        let virtual_h = total.max(h);
        let max = (virtual_h - h) as usize;
        let scroll = self.note_scroll_max(max) as usize;

        let full = Rect::new(0, 0, w, virtual_h);
        let mut tmp = Buffer::empty(full);
        let mut y = 0;
        let mut headline = Some(headline);
        for (part, height) in parts {
            let r = Rect::new(0, y, w, height);
            y += height;
            match part {
                Part::Headline => Paragraph::new(headline.take().unwrap_or_default())
                    .block(panel("Usage · all time"))
                    .render(r, &mut tmp),
                _ => self.draw_part(part, &v, r, &mut tmp),
            }
        }
        for dy in 0..h {
            for dx in 0..w {
                if let Some(cell) = tmp.cell((dx, dy + scroll as u16)) {
                    buf[(area.x + dx, area.y + dy)] = cell.clone();
                }
            }
        }
    }

    fn draw_wide(&self, v: &UsageView, headline: Vec<Line<'static>>, area: Rect, buf: &mut Buffer) {
        let [head, mid, rest] = Layout::vertical([
            Constraint::Length(5),
            Constraint::Length(8),
            Constraint::Fill(1),
        ])
        .areas(area);
        Paragraph::new(headline)
            .block(panel("Usage · all time"))
            .render(head, buf);
        let [days, context] =
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(mid);
        self.draw_part(Part::Days, v, days, buf);
        self.draw_part(Part::Context, v, context, buf);

        let (main, foot) = if rest.height >= 19 {
            let [a, b] = Layout::vertical([Constraint::Fill(1), Constraint::Length(8)]).areas(rest);
            (a, Some(b))
        } else {
            (rest, None)
        };
        let [emp, tick] =
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                .areas(main);
        self.draw_part(Part::Employees, v, emp, buf);
        self.draw_part(Part::Tickets, v, tick, buf);
        if let Some(foot) = foot {
            let [m, r] =
                Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)])
                    .areas(foot);
            self.draw_part(Part::Models, v, m, buf);
            self.draw_part(Part::Recent, v, r, buf);
        }
    }

    fn draw_part(&self, part: Part, v: &UsageView, area: Rect, buf: &mut Buffer) {
        let tickets_title = format!("By ticket ({})", v.tickets_total);
        let days_title = if self.usage_hourly {
            "Last 48 hours (tokens/hour)"
        } else {
            "Last 14 days (tokens/day)"
        };
        let title = match part {
            Part::Headline => "Usage · all time",
            Part::Days => days_title,
            Part::Context => "Context windows (live)",
            Part::Employees => "By employee",
            Part::Tickets => &tickets_title,
            Part::Models => "By model",
            Part::Recent => "Recent samples",
        };
        let block = panel(title);
        let inner = block.inner(area);
        block.render(area, buf);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        let w = inner.width as usize;
        let lines: Vec<Line<'static>> = match part {
            Part::Headline => Vec::new(),
            Part::Days => {
                self.draw_days(v, inner, buf);
                return;
            }
            Part::Context => context_lines(&v.context, w),
            Part::Employees => employee_lines(&v.employees, w),
            Part::Tickets => ticket_lines(v, w),
            Part::Models => v
                .models
                .iter()
                .map(|m| {
                    Line::from(format!(
                        "{:<20} {:>7} in {:>7} out {:>9}",
                        cut(&m.model, 20),
                        format_tokens(m.totals.input_tokens),
                        format_tokens(m.totals.output_tokens),
                        cost_cell(&m.totals)
                    ))
                })
                .collect(),
            Part::Recent => v
                .recent
                .iter()
                .map(|s| {
                    let cost = match s.sample.cost_micros {
                        Some(c) if s.sample.estimated => {
                            format!("~{}", format_money(c, &s.sample.currency))
                        }
                        Some(c) => format_money(c, &s.sample.currency),
                        None => "-".to_string(),
                    };
                    Line::from(format!(
                        "{} {:<10} {:<8} {:>7} {:>9}",
                        s.sample.at.format("%H:%M:%S"),
                        cut(&s.employee_name, 10),
                        s.sample.ticket.as_deref().unwrap_or("-"),
                        format_tokens(s.sample.input_tokens + s.sample.output_tokens),
                        cost
                    ))
                })
                .collect(),
        };
        let lines: Vec<Line<'static>> = if lines.is_empty() {
            vec![Line::from("Nothing yet.").dark_gray()]
        } else {
            lines.into_iter().map(|l| truncate_line(l, w)).collect()
        };
        Paragraph::new(lines).render(inner, buf);
    }

    fn draw_days(&self, v: &UsageView, inner: Rect, buf: &mut Buffer) {
        // (label, tokens) per bucket; the label is `MM-DD HH` or `MM-DD`.
        let (points, unit): (Vec<(&str, u64)>, &str) = if self.usage_hourly {
            let p = v.hourly.iter().map(|h| (h.hour.as_str(), h.tokens_total()));
            (p.collect(), "hour")
        } else {
            let p = v.daily.iter().map(|d| (d.date.as_str(), d.tokens_total()));
            (p.collect(), "day")
        };
        let data: Vec<u64> = points.iter().map(|p| p.1).collect();
        let max = data.iter().copied().max().unwrap_or(0);
        let [spark, labels] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(inner);
        Sparkline::default()
            .data(data)
            .style(Style::new().cyan())
            .render(spark, buf);
        let short = |d: &str| d.get(5..).unwrap_or(d).replace('T', " ");
        let first = points.first().map(|p| short(p.0)).unwrap_or_default();
        let last = points.last().map(|p| short(p.0)).unwrap_or_default();
        let peak = format!("peak {}/{unit}", format_tokens(max));
        let gap = (inner.width as usize).saturating_sub(first.len() + last.len() + peak.len());
        Paragraph::new(Line::from(format!(
            "{first}{}{peak}{}{last}",
            " ".repeat(gap / 2),
            " ".repeat(gap - gap / 2),
        )))
        .dark_gray()
        .render(labels, buf);
    }

    fn headline_lines(&self, v: &UsageView) -> Vec<Line<'static>> {
        let t = &v.total;
        let mut first = format!("Tokens {} · Cost {}", tokens_text(t), money(t),);
        if t.unpriced_samples > 0 {
            first.push_str(&format!(" · {} turns unpriced", t.unpriced_samples));
        }
        let mut other = other_currencies(t);
        if !other.is_empty() {
            other.insert_str(0, " · ");
            first.push_str(&other);
        }
        let second = format!(
            "Today (UTC) {} · 7 days {} · Burn/day {} · {}",
            short_cost(&v.windows.today, true),
            short_cost(&v.windows.last_7_days, true),
            format_tokens(v.burn.tokens_per_day_7d),
            if v.burn.cost_micros_per_day_7d > 0 {
                format_money(v.burn.cost_micros_per_day_7d, "USD")
            } else {
                "no cost".to_string()
            },
        );
        let (base, since) = match &self.usage_base {
            Some((b, at)) => (b.clone(), at.format("%H:%M").to_string()),
            None => (t.clone(), "now".to_string()),
        };
        let d = t.delta_since(&base);
        let updated = v.recent.first().map_or_else(
            || "-".to_string(),
            |s| s.sample.at.format("%H:%M:%S").to_string(),
        );
        let third = format!(
            "Since you opened this view ({since}): +{} in / +{} out · +{} · updated {updated}",
            format_tokens(d.input_tokens),
            format_tokens(d.output_tokens),
            if d.has_cost() {
                format_money(d.total_cost_micros(), "USD")
            } else {
                "$0".to_string()
            },
        );
        vec![
            Line::from(first),
            Line::from(second),
            Line::from(third).dark_gray(),
        ]
    }
}

#[derive(Clone, Copy)]
enum Part {
    Headline,
    Days,
    Context,
    Employees,
    Tickets,
    Models,
    Recent,
}

/// How many ticket rows the wide layout's ticket panel holds in a terminal `h` rows high.
fn wide_ticket_rows(h: u16) -> usize {
    let rest = h.saturating_sub(13);
    let main = if rest >= 19 { rest - 8 } else { rest };
    main.saturating_sub(2) as usize
}

fn cut(s: &str, w: usize) -> String {
    truncate_line(Line::from(s.to_string()), w).to_string()
}

fn tokens_text(t: &UsageTotals) -> String {
    format!(
        "in {} · out {} · cached {}",
        format_tokens(t.input_tokens),
        format_tokens(t.output_tokens),
        format_tokens(t.cached_tokens)
    )
}

/// The cost of `t`: reported as is, "est. $x" when all of it is estimated, and
/// "$x (incl. est. $y)" when mixed. Never claims precision it does not have.
fn money(t: &UsageTotals) -> String {
    if !t.has_cost() {
        return "no cost data".to_string();
    }
    let total = format_money(t.total_cost_micros(), "USD");
    if !t.is_estimated() {
        total
    } else if t.cost_micros == 0 {
        format!("est. {total}")
    } else {
        format!(
            "{total} (incl. est. {})",
            format_money(t.estimated_cost_micros, "USD")
        )
    }
}

/// Tokens and cost on one short phrase, for the windows line.
fn short_cost(t: &UsageTotals, with_tokens: bool) -> String {
    let cost = if t.has_cost() {
        format!(
            "{}{}",
            if t.is_estimated() { "~" } else { "" },
            format_money(t.total_cost_micros(), "USD")
        )
    } else {
        "-".to_string()
    };
    if with_tokens {
        format!("{} tok {cost}", format_tokens(t.tokens_total()))
    } else {
        cost
    }
}

fn other_currencies(t: &UsageTotals) -> String {
    let parts: Vec<String> = t
        .other_currency_micros
        .iter()
        .map(|(c, m)| format_money(*m, c))
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!("also: {}", parts.join(", "))
    }
}

fn cost_cell(t: &UsageTotals) -> String {
    if !t.has_cost() {
        return "-".to_string();
    }
    format!(
        "{}{}",
        if t.is_estimated() { "~" } else { "" },
        format_money(t.total_cost_micros(), "USD")
    )
}

fn context_lines(rows: &[ContextRow], w: usize) -> Vec<Line<'static>> {
    rows.iter()
        .map(|c| {
            let filled = (c.percent as usize * BAR).div_ceil(100).min(BAR);
            let style = match c.percent {
                0..70 => Style::new().green(),
                70..=90 => Style::new().yellow(),
                _ => Style::new().red(),
            };
            truncate_line(
                Line::from(vec![
                    Span::raw(format!("{:<10} ", cut(&c.name, 10))),
                    Span::styled("█".repeat(filled), style),
                    Span::styled("░".repeat(BAR - filled), Style::new().dark_gray()),
                    Span::raw(format!(
                        " {}% · {}/{}",
                        c.percent,
                        format_tokens(c.used),
                        format_tokens(c.size)
                    )),
                    Span::styled(
                        c.ticket
                            .as_deref()
                            .map(|t| format!(" {t}"))
                            .unwrap_or_default(),
                        Style::new().dark_gray(),
                    ),
                ]),
                w,
            )
        })
        .collect()
}

fn employee_lines(rows: &[EmployeeRow], w: usize) -> Vec<Line<'static>> {
    rows.iter()
        .map(|e| {
            let backend = match &e.model {
                Some(m) => format!("{}·{m}", e.backend),
                None => e.backend.clone(),
            };
            let head = format!("{:<12} {:<20} ", cut(&e.name, 12), cut(&backend, 20));
            let stats = format!(
                "{:>7} {:>7} {:>9} {:>4}t",
                format_tokens(e.totals.input_tokens),
                format_tokens(e.totals.output_tokens),
                cost_cell(&e.totals),
                e.sampled_turns
            );
            let dim = |s: String| Span::styled(s, Style::new().dark_gray());
            let tail = match e.state {
                UsageHealth::Ok => Span::raw(stats),
                UsageHealth::ReportsNothing => {
                    dim(format!("reports no usage ({} turns)", e.turns_total))
                }
                UsageHealth::NoCost => Span::raw(format!(
                    "{:>7} {:>7} ",
                    format_tokens(e.totals.input_tokens),
                    format_tokens(e.totals.output_tokens)
                )),
                UsageHealth::Idle => dim("no turns yet".to_string()),
            };
            let mut spans = vec![Span::raw(head), tail];
            if e.state == UsageHealth::NoCost {
                spans.push(dim(
                    "tokens only · set [pricing.\"model\"] for an estimate".to_string()
                ));
            }
            truncate_line(Line::from(spans), w)
        })
        .collect()
}

fn ticket_lines(v: &UsageView, w: usize) -> Vec<Line<'static>> {
    let lines: Vec<Line<'static>> = v
        .tickets
        .iter()
        .map(|t| {
            let tail = format!(
                "{:>9} {:>7}",
                cost_cell(&t.totals),
                format_tokens(t.totals.tokens_total())
            );
            let title_w = w.saturating_sub(9 + 1 + tail.len() + 1);
            let title = t.title.as_deref().unwrap_or("");
            truncate_line(
                Line::from(format!(
                    "{:<9}{:<title_w$} {tail}",
                    cut(&t.id, 9),
                    cut(title, title_w)
                )),
                w,
            )
        })
        .collect();
    lines
}
