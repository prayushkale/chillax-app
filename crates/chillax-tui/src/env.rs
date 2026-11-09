//! The Environments view: where the code is on its way from `dev`
//! through QA to a tagged release, and what waits for the stakeholder.

use chillax_core::{
    AppStatus, QaRound, QaRoundStatus, QaVerdict, Release, ReleaseStatus, RunningApp,
};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget, Wrap};

use crate::view::panel;
use crate::{App, link};

/// The four panels of the Environments view: dev, qa, prod, release candidate.
pub(crate) fn env_panels(area: Rect) -> [Rect; 4] {
    let [top, bottom] =
        Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(area);
    let [dev, qa] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(top);
    let [prod, candidate] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(bottom);
    [dev, qa, prod, candidate]
}

/// Split a panel's inner area into its text and its two app rows (status, address).
/// Drawing and clicks share this so a click lands on what was drawn.
pub(crate) fn env_panel_split(inner: Rect, has_app: bool) -> [Rect; 2] {
    let app_rows = if has_app { 2.min(inner.height) } else { 0 };
    Layout::vertical([Constraint::Min(0), Constraint::Length(app_rows)]).areas(inner)
}

impl App {
    pub(crate) fn render_environments(&self, area: Rect, buf: &mut Buffer) {
        let [dev, qa, prod, candidate] = env_panels(area);
        self.render_env_panel("dev", self.dev_lines(), dev, buf);
        self.render_env_panel("qa", self.qa_lines(), qa, buf);
        self.render_env_panel("prod", self.prod_lines(), prod, buf);
        Paragraph::new(self.candidate_lines())
            .block(panel("Release candidate"))
            .wrap(Wrap { trim: false })
            .render(candidate, buf);
    }

    /// A panel for one environment: its text, and at the bottom its app when Chillax runs
    /// one (status, then the address as a link).
    fn render_env_panel(&self, env: &str, lines: Vec<Line<'static>>, area: Rect, buf: &mut Buffer) {
        let block = panel(env);
        let inner = block.inner(area);
        block.render(area, buf);
        let app = self.state.app(env);
        let [text, app_area] = env_panel_split(inner, app.is_some());
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .render(text, buf);
        if let Some(app) = app {
            self.render_app(app, app_area, buf);
        }
    }

    /// The first running app's address, for the Dashboard's `o` key. Supervised apps come
    /// first within a label, `dev` first overall. Only local http(s) URLs qualify, even
    /// though a reporter's URL was already checked when it was reported.
    pub(crate) fn first_running_url(&self) -> Option<String> {
        self.state
            .running_apps()
            .into_iter()
            .filter(|a| a.is_running())
            .find_map(|a| {
                chillax_core::apps::normalize_reported_url(&a.url)
                    .ok()
                    .map(|(url, _)| url)
            })
    }

    /// The Dashboard's "Running apps" panel: one row per app, supervised and reported
    /// together. `● dev  localhost:3000  @a1b2c3d` runs under Chillax;
    /// `◌ T-12  localhost:3001  pat` was reported by that employee.
    pub(crate) fn render_running_apps(&self, area: Rect, buf: &mut Buffer) {
        let block = panel("Running apps");
        let inner = block.inner(area);
        block.render(area, buf);
        let rows = self.state.running_apps();
        if rows.is_empty() {
            Paragraph::new(
                Line::from(
                    "No app running. Set [env.dev] run/port in chillax.toml, or a hire can report one.",
                )
                .dark_gray(),
            )
            .wrap(Wrap { trim: true })
            .render(inner, buf);
            return;
        }
        let label_width = rows
            .iter()
            .map(|r| r.label.chars().count())
            .max()
            .unwrap_or(0)
            .clamp(3, 12);
        for (row, y) in rows.iter().zip(inner.y..inner.bottom()) {
            let reported = matches!(row.source, chillax_core::AppSource::Reported(_));
            let (dot, color) = match (row.status, reported) {
                (AppStatus::Running, true) => ("◌", Color::Green),
                (AppStatus::Running, false) => ("●", Color::Green),
                (AppStatus::Starting, _) => ("◌", Color::Yellow),
                (AppStatus::Crashed, _) => ("✗", Color::Red),
                (AppStatus::Stopped, _) => ("○", Color::DarkGray),
            };
            let label: String = row.label.chars().take(label_width).collect();
            let head = format!("{dot} {label:<label_width$} ");
            let right = inner.right();
            buf.set_stringn(
                inner.x,
                y,
                &head,
                usize::from(inner.width),
                Style::new().fg(color),
            );
            let head_width = u16::try_from(head.chars().count()).unwrap_or(inner.width);
            let x = inner.x.saturating_add(head_width);
            let addr = row
                .url
                .split_once("://")
                .map_or(row.url.as_str(), |(_, rest)| rest)
                .trim_end_matches('/')
                .to_string();
            let style = if row.is_running() {
                Style::new().fg(Color::Cyan).underlined()
            } else {
                Style::new().fg(Color::DarkGray)
            };
            let used = link::put_link(
                buf,
                (x, y),
                right.saturating_sub(x),
                &addr,
                &row.url,
                style,
                row.is_running() && !self.no_hyperlinks,
            );
            let tail = match &row.source {
                chillax_core::AppSource::Reported(who) => self
                    .state
                    .employee(who)
                    .map_or_else(|| who.clone(), |e| e.employee.name.clone()),
                chillax_core::AppSource::Supervised => match (&row.commit, row.is_running()) {
                    (Some(c), true) => format!("@{}", chillax_core::apps::short(c)),
                    _ => row.status.label().to_string(),
                },
            };
            let tx = x.saturating_add(used).saturating_add(2);
            if tx < right {
                buf.set_stringn(
                    tx,
                    y,
                    &tail,
                    usize::from(right - tx),
                    Style::new().fg(Color::Gray),
                );
            }
        }
    }

    fn render_app(&self, app: &RunningApp, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }
        let selected =
            self.state.apps.len() > 1 && self.selected_app().is_some_and(|s| s.env == app.env);
        let (dot, color) = match app.status {
            AppStatus::Running => ("●", Color::Green),
            AppStatus::Starting => ("◌", Color::Yellow),
            AppStatus::Crashed => ("✗", Color::Red),
            AppStatus::Stopped => ("○", Color::DarkGray),
        };
        let mut detail = app.status.label().to_string();
        if app.status == AppStatus::Running
            && let Some(commit) = &app.commit
        {
            detail.push_str(&format!(" @{}", chillax_core::apps::short(commit)));
        }
        if let Some(reason) = &app.reason {
            detail.push_str(&format!(": {reason}"));
        }
        if !app.wanted {
            detail = "stopped by you".to_string();
        }
        let marker = if selected { "▸ " } else { "  " };
        let line = Line::from(vec![
            Span::styled(marker, Style::new().bold()),
            Span::styled(dot, Style::new().fg(color)),
            Span::raw(format!(" {detail}")),
        ]);
        Paragraph::new(line).render(Rect { height: 1, ..area }, buf);
        if area.height > 1 {
            link::put_link(
                buf,
                (area.x + 2, area.y + 1),
                area.width.saturating_sub(2),
                &app.url,
                &app.url,
                Style::new().fg(Color::Cyan).underlined(),
                !self.no_hyperlinks,
            );
        }
    }

    fn dev_lines(&self) -> Vec<Line<'static>> {
        let s = &self.state;
        let open = s
            .tickets
            .iter()
            .filter(|t| {
                t.branch.is_some() && t.state != "done" && !t.checks.contains("merged_to_dev")
            })
            .count();
        let waiting = s.waiting_for_qa().len();
        let mut lines = vec![
            Line::from(format!("{open} open branches")),
            Line::from(format!("{waiting} merged to dev, waiting for QA")),
        ];
        if s.qa_requested {
            lines.push(Line::from("promotion to qa requested").yellow());
        }
        if waiting > 0 && s.open_round().is_none() && !s.qa_requested {
            lines.push(Line::from(vec![
                " p ".blue().bold(),
                "promote to qa".into(),
            ]));
        }
        lines
    }

    fn qa_lines(&self) -> Vec<Line<'static>> {
        let Some(round) = &self.state.qa else {
            return vec![Line::from("No QA round yet.").dark_gray()];
        };
        let mut lines = vec![Line::from(vec![
            Span::styled(format!("Round {}", round.number), Style::new().bold()),
            Span::raw(format!(
                " · {} · {}",
                round_status(round),
                round.commit.chars().take(7).collect::<String>()
            )),
        ])];
        if round.status == QaRoundStatus::EnvFailed {
            lines.push(Line::from("The environment failed; press r to retry.").red());
        }
        for id in &round.tickets {
            let (mark, word) = match round.verdicts.get(id) {
                Some(QaVerdict::Pass) => ("✓".green(), "pass"),
                Some(QaVerdict::Fail) => ("✗".red(), "fail"),
                Some(QaVerdict::Env) => ("!".yellow(), "env"),
                None => ("…".dark_gray(), "pending"),
            };
            let title = self.state.ticket(id).map_or("", |t| t.title.as_str());
            lines.push(Line::from(vec![
                mark,
                Span::raw(format!(" {id} {word}  {title}")),
            ]));
        }
        lines
    }

    fn prod_lines(&self) -> Vec<Line<'static>> {
        let last = self.state.releases.iter().rev().find(|r| {
            matches!(
                r.status,
                ReleaseStatus::Tagged | ReleaseStatus::Deployed | ReleaseStatus::DeployFailed
            )
        });
        match last {
            None => vec![Line::from("Nothing released yet.").dark_gray()],
            Some(r) => {
                let mut lines = vec![Line::from(format!(
                    "{} · {}",
                    r.tag.as_deref().unwrap_or(&r.version),
                    release_status(r)
                ))];
                if r.status == ReleaseStatus::DeployFailed {
                    lines.push(Line::from(vec![
                        " r ".blue().bold(),
                        "retry the deploy".into(),
                    ]));
                }
                lines
            }
        }
    }

    fn candidate_lines(&self) -> Vec<Line<'static>> {
        let Some(r) = self.state.open_release() else {
            let mut lines = vec![Line::from("No release candidate.").dark_gray()];
            if self.state.open_round().is_some() {
                lines.push(
                    Line::from("A QA round is open; prepare the release once it closes.").yellow(),
                );
                return lines;
            }
            lines.extend([
                Line::from(vec![" g ".blue().bold(), "Prepare release".bold()]),
                Line::from("  Bundle what QA signed off into a release for you to approve.")
                    .dark_gray(),
                Line::from(vec![
                    " f ".blue().bold(),
                    "Release with known issues".bold(),
                ]),
                Line::from("  Same, but tickets that failed QA ship as listed known issues.")
                    .dark_gray(),
            ]);
            return lines;
        };
        let mut lines = vec![
            Line::from(format!("{} · {}", r.version, release_status(r))).bold(),
            Line::from(format!("tickets: {}", r.tickets.join(", "))),
        ];
        if !r.known_issues.is_empty() {
            lines.push(Line::from(format!("known issues: {}", r.known_issues.join(", "))).yellow());
        }
        if r.status == ReleaseStatus::Gated {
            lines.push(Line::from(vec![
                " a ".blue().bold(),
                "approve  ".into(),
                " x ".blue().bold(),
                "reject".into(),
            ]));
        }
        if r.status == ReleaseStatus::Approved {
            lines.push(Line::from("If the alert says it cannot proceed, fix that, then:").yellow());
            lines.push(Line::from(vec![
                " r ".blue().bold(),
                "retry  ".into(),
                " x ".blue().bold(),
                "reject".into(),
            ]));
        }
        lines.extend(
            r.notes
                .lines()
                .map(|l| Line::from(l.to_string()).dark_gray()),
        );
        lines
    }
}

fn round_status(round: &QaRound) -> &'static str {
    match round.status {
        QaRoundStatus::Starting => "starting",
        QaRoundStatus::Testing => "testing",
        QaRoundStatus::EnvFailed => "env failed",
        QaRoundStatus::Closed => "closed",
    }
}

fn release_status(r: &Release) -> &'static str {
    match r.status {
        ReleaseStatus::Gated => "awaiting approval",
        ReleaseStatus::Approved => "merging and tagging",
        ReleaseStatus::Tagged => "tagged",
        ReleaseStatus::Deployed => "deployed",
        ReleaseStatus::DeployFailed => "deploy failed",
        ReleaseStatus::Rejected => "rejected",
    }
}
