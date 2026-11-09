//! Problems the project manager handles first (ADR 0032). The runner keeps a record per
//! deferred problem in memory, briefs the project manager a bounded number of times, and
//! tells the stakeholder with what was tried when that does not work. The rules of what is
//! briefed and when live in `chillax_core::{problems, dispatch}`; this is the memory and
//! the clock.

use std::time::{Duration, Instant};

use chillax_core::dispatch::{
    Context, KEY_PROMOTE, KEY_QA_CLOSE, ProblemCtx, app_claim, live_problems, pm_available,
};
use chillax_core::engine::Engine;
use chillax_core::limits::{self, FailureKind};
use chillax_core::problems::{self as core_problems, ProblemKind, ProblemNote};
use chillax_core::{Command, EmployeeId};

use crate::Dispatcher;

/// One problem the runner deferred.
pub(crate) struct ProblemRec {
    pub kind: ProblemKind,
    pub subject: String,
    pub title: String,
    pub detail: String,
    pub employee: Option<EmployeeId>,
    pub first_seen: Instant,
    pub briefed: u32,
    pub last_briefed: Option<Instant>,
    /// The stakeholder has been told.
    pub escalated: bool,
    /// The project manager said it is handled; it counts as solved once the key succeeds.
    pub resolved: bool,
    pub pm_summary: Option<String>,
}

impl ProblemRec {
    fn due(&self, max: u32, retry: Duration) -> bool {
        !self.escalated
            && !self.resolved
            && self.briefed < max
            && self.last_briefed.is_none_or(|t| t.elapsed() >= retry)
    }
}

/// Dispatcher actions that no ticket owns and the project manager may be able to fix.
/// Releases, deploys and the project manager's own briefings are never deferred.
pub(crate) fn deferrable_key(key: &str) -> bool {
    key == KEY_PROMOTE
        || key == KEY_QA_CLOSE
        || ["dev", "qa", "prod", "other"]
            .iter()
            .any(|env| key == app_claim(env))
}

/// Sign-in and usage-limit errors need a person (category 2): never deferred.
pub(crate) fn needs_a_person(error: &str) -> bool {
    limits::classify_failure(error, chrono::Utc::now(), crate::local_offset()) != FailureKind::Other
}

fn short(text: &str) -> String {
    text.chars().take(400).collect()
}

impl Dispatcher {
    /// Whether deferral is switched on at all.
    fn deferral_on(&self) -> bool {
        !self.live().problem_grace.is_zero() && self.live().problem_max_attempts > 0
    }

    fn halted_ctx(&self) -> Context {
        let (halted_backends, cooling_employees) = self.book_halts();
        Context {
            halted_backends,
            cooling_employees,
            ..Context::default()
        }
    }

    /// Tell the stakeholder about a problem the project manager could solve: hold it for
    /// him when deferral is on and a project manager can take it, otherwise alert at once
    /// exactly as before. A problem he already reported solved that comes back is
    /// escalated straight away, with what he said he did.
    pub(crate) async fn alert_or_defer(
        &self,
        key: &str,
        kind: ProblemKind,
        subject: &str,
        employee: Option<&str>,
        title: String,
        detail: String,
    ) {
        self.defer(key, kind, subject, employee, title, detail, false)
            .await;
    }

    /// Like [`Self::alert_or_defer`] for a problem that raised no alert before: when it
    /// cannot be deferred, nothing is said (a teammate's crash only backs the ticket off).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn defer_quietly(
        &self,
        key: &str,
        kind: ProblemKind,
        subject: &str,
        employee: Option<&str>,
        title: String,
        detail: String,
    ) {
        self.defer(key, kind, subject, employee, title, detail, true)
            .await;
    }

    #[allow(clippy::too_many_arguments)]
    async fn defer(
        &self,
        key: &str,
        kind: ProblemKind,
        subject: &str,
        employee: Option<&str>,
        title: String,
        detail: String,
        quiet: bool,
    ) {
        let engine = self.hub.engine().await;
        let defer = self.deferral_on() && pm_available(&engine, &self.halted_ctx());
        let escalate = {
            let mut book = self.book.lock().expect("book lock poisoned");
            match book.problems.get_mut(key) {
                Some(rec) if rec.escalated => return,
                Some(rec) if rec.resolved => {
                    rec.escalated = true;
                    Some((rec.title.clone(), rec.detail.clone(), rec.briefed))
                }
                Some(rec) => {
                    rec.detail = detail;
                    return;
                }
                None if defer => {
                    book.problems.insert(
                        key.to_string(),
                        ProblemRec {
                            kind,
                            subject: subject.to_string(),
                            title,
                            detail,
                            employee: employee.map(str::to_string),
                            first_seen: Instant::now(),
                            briefed: 0,
                            last_briefed: None,
                            escalated: false,
                            resolved: false,
                            pm_summary: None,
                        },
                    );
                    return;
                }
                None => None,
            }
        };
        match escalate {
            Some((title, detail, attempts)) => {
                let summary = self.summary_of(key);
                self.raise_tried(title, detail, summary, attempts).await;
            }
            None if quiet => {}
            None => {
                let _ = self.send(Command::RaiseAlert { title, detail }).await;
            }
        }
    }

    fn summary_of(&self, key: &str) -> Option<String> {
        self.book
            .lock()
            .expect("book lock poisoned")
            .problems
            .get(key)
            .and_then(|r| r.pm_summary.clone())
    }

    async fn raise_tried(
        &self,
        title: String,
        detail: String,
        summary: Option<String>,
        attempts: u32,
    ) {
        let detail = format!(
            "{detail}\n\nThe project manager tried: {} ({attempts} attempts).",
            summary.as_deref().unwrap_or("no summary")
        );
        let _ = self.send(Command::RaiseAlert { title, detail }).await;
    }

    /// The deferred problems for the decision, or `None` when deferral is off.
    pub(crate) fn problem_ctx(&self, engine: &Engine) -> Option<ProblemCtx> {
        if !self.deferral_on() {
            return None;
        }
        let max = self.live().problem_max_attempts;
        let retry = self.live().problem_retry;
        let book = self.book.lock().expect("book lock poisoned");
        let notes: Vec<ProblemNote> = book
            .problems
            .iter()
            .filter(|(_, r)| !r.escalated && !r.resolved)
            .map(|(key, r)| ProblemNote {
                key: key.clone(),
                kind: r.kind,
                subject: r.subject.clone(),
                detail: r.detail.clone(),
                employee: r.employee.clone(),
                first_seen_secs: r.first_seen.elapsed().as_secs(),
                attempts: r.briefed,
            })
            .collect();
        let escalated = book
            .problems
            .iter()
            .filter(|(_, r)| r.escalated || r.resolved)
            .map(|(key, _)| key.clone())
            .collect();
        // Failed teammates the runner has not recorded yet are owed a first briefing.
        let unrecorded = core_problems::problems(engine, &[], usize::MAX)
            .iter()
            .any(|p| !book.problems.contains_key(&p.key));
        let due = unrecorded || book.problems.values().any(|r| r.due(max, retry));
        Some(ProblemCtx {
            due,
            notes,
            escalated,
        })
    }

    /// Brief the project manager with the problems that are due. A chat that cannot be
    /// delivered is logged and tried again after the retry wait; it never alerts.
    pub(crate) async fn handle_problems(&self, pm: &str) -> Result<(), String> {
        let engine = self.hub.engine().await;
        let ctx = Context {
            problem: self.problem_ctx(&engine),
            ..Context::default()
        };
        let max = self.live().problem_max_attempts;
        let retry = self.live().problem_retry;
        let found = live_problems(&engine, &ctx);
        let due: Vec<_> = {
            let mut book = self.book.lock().expect("book lock poisoned");
            let mut due = Vec::new();
            for mut p in found {
                let rec = book
                    .problems
                    .entry(p.key.clone())
                    .or_insert_with(|| ProblemRec {
                        kind: p.kind,
                        subject: p.subject.clone(),
                        title: format!("{} needs attention", p.subject),
                        detail: p.detail.clone(),
                        employee: None,
                        first_seen: Instant::now(),
                        briefed: 0,
                        last_briefed: None,
                        escalated: false,
                        resolved: false,
                        pm_summary: None,
                    });
                if rec.due(max, retry) {
                    rec.briefed += 1;
                    rec.last_briefed = Some(Instant::now());
                    p.attempts = rec.briefed;
                    due.push(p);
                }
            }
            due
        };
        if let Some(text) = core_problems::briefing(&due, max)
            && let Err(error) = self
                .send(Command::SendChat {
                    to: pm.to_string(),
                    text,
                })
                .await
        {
            tracing::warn!(%pm, %error, "could not send the problem briefing");
        }
        Ok(())
    }

    /// Hand problems whose time is up to the stakeholder, with what was tried.
    pub(crate) async fn escalate_problems(&self) {
        let max = self.live().problem_max_attempts;
        let retry = self.live().problem_retry;
        let grace = self.live().problem_grace;
        if self.deferral_on() {
            let ready: Vec<(String, String, Option<String>, u32)> = {
                let mut book = self.book.lock().expect("book lock poisoned");
                book.problems
                    .values_mut()
                    .filter(|r| !r.escalated && !r.resolved)
                    .filter(|r| {
                        r.first_seen.elapsed() >= grace
                            || (r.briefed >= max
                                && r.last_briefed.is_some_and(|t| t.elapsed() >= retry))
                    })
                    .map(|r| {
                        r.escalated = true;
                        (
                            r.title.clone(),
                            r.detail.clone(),
                            r.pm_summary.clone(),
                            r.briefed,
                        )
                    })
                    .collect()
            };
            for (title, detail, summary, attempts) in ready {
                self.raise_tried(title, detail, summary, attempts).await;
            }
        }
    }

    /// The project manager reported a problem handled: remember what he did.
    pub(crate) fn problem_resolved(&self, key: &str, summary: &str) {
        let mut book = self.book.lock().expect("book lock poisoned");
        if let Some(rec) = book.problems.get_mut(key) {
            rec.resolved = true;
            rec.pm_summary = Some(short(summary));
        }
    }

    /// The project manager asked for another try: forget the wait, the failure counts and
    /// the idle count of whatever `key` names, so the next tick runs it again.
    pub(crate) fn retry_action(&self, key: &str) {
        let ticket = key.strip_prefix("stuck-").unwrap_or(key);
        let mut book = self.book.lock().expect("book lock poisoned");
        for k in [key, ticket] {
            book.cooling.remove(k);
            book.reported.remove(k);
            book.failures.remove(k);
        }
        book.idle.retain(|(_, t), _| t != ticket);
    }

    /// The key's action succeeded or its cause is gone: forget the problem.
    pub(crate) fn problem_solved(&self, key: &str) {
        self.book
            .lock()
            .expect("book lock poisoned")
            .problems
            .remove(key);
    }
}
