//! The background task: watches the event stream and keeps herdr up to date.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chillax_core::engine::State;
use chillax_core::notify::{HireState, agent_summary, compact, hire_summaries, notifications_for};
use chillax_core::{AgentState, AgentSummary, CompanyHandle, Event, Notification, Notifier};
use tokio::sync::broadcast::{
    self,
    error::{RecvError, TryRecvError},
};

use super::{Herdr, HiresMeta};

pub struct Options {
    /// Send the inbox, QA and project tokens beside the state.
    pub metadata: bool,
    pub notifications: bool,
    /// Quiet time that lets a burst of events settle into one report.
    pub debounce: Duration,
    /// Least time between two sends of the per-hire metadata; the last change waits.
    pub hires_interval: Duration,
    /// Resend unchanged hires metadata this often, before herdr's TTL expires it.
    pub hires_refresh: Duration,
}

/// Same key at most once per this long.
const KEY_WINDOW: Duration = Duration::from_secs(60);
/// At most [`BURST_MAX`] notifications per this long, whatever the keys.
const BURST_WINDOW: Duration = Duration::from_secs(10);
const BURST_MAX: usize = 5;

/// Keeps a flapping agent from spamming the stakeholder.
#[derive(Default)]
pub struct Throttle {
    by_key: HashMap<String, Instant>,
    recent: VecDeque<Instant>,
}

impl Throttle {
    /// True when a notification with `key` may go out now; records it if so.
    pub fn allow(&mut self, key: &str, now: Instant) -> bool {
        self.by_key
            .retain(|_, at| now.saturating_duration_since(*at) < KEY_WINDOW);
        while self
            .recent
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) >= BURST_WINDOW)
        {
            self.recent.pop_front();
        }
        if self.by_key.contains_key(key) || self.recent.len() >= BURST_MAX {
            return false;
        }
        self.by_key.insert(key.to_string(), now);
        self.recent.push_back(now);
        true
    }
}

/// Delivers notifications through `herdr notification show`.
pub struct HerdrNotifier {
    herdr: Arc<Herdr>,
    throttle: Mutex<Throttle>,
}

impl HerdrNotifier {
    pub fn new(herdr: Arc<Herdr>) -> Self {
        Self {
            herdr,
            throttle: Mutex::default(),
        }
    }
}

#[async_trait]
impl Notifier for HerdrNotifier {
    async fn notify(&self, n: Notification) {
        let allowed = self
            .throttle
            .lock()
            .expect("lock")
            .allow(&n.key, Instant::now());
        if allowed {
            self.herdr.show(&n).await;
        } else {
            tracing::debug!(key = %n.key, "herdr notification throttled");
        }
    }
}

type Tokens = BTreeMap<&'static str, Option<String>>;

/// Display tokens for the pane. A zero count is cleared rather than shown.
fn tokens(summary: &AgentSummary) -> Tokens {
    let count = |n: usize| (n > 0).then(|| n.to_string());
    BTreeMap::from([
        ("project", Some(summary.project.clone())),
        ("inbox", count(summary.open_inbox)),
        ("qa", count(summary.in_qa)),
        ("failed", count(summary.failed)),
    ])
}

/// What herdr was last told, so unchanged state is not re-sent.
#[derive(Default)]
struct Sent {
    state: Option<AgentState>,
    tokens: Option<Tokens>,
    /// The hires metadata herdr holds; `None` when it holds none.
    hires: Option<HiresMeta>,
    hires_at: Option<Instant>,
    /// A hires change is waiting out the minimum interval until then.
    hires_due: Option<Instant>,
}

impl Sent {
    /// When the loop must wake without an event: a deferred change or a refresh.
    fn wake_at(&self, options: &Options) -> Option<Instant> {
        if !options.metadata {
            return None;
        }
        self.hires_due.or_else(|| {
            self.hires
                .as_ref()
                .zip(self.hires_at)
                .map(|(_, at)| at + options.hires_refresh)
        })
    }
}

fn names(hires: &[chillax_core::HireSummary], with_ticket: bool) -> String {
    hires
        .iter()
        .map(|h| match (&h.ticket, with_ticket) {
            (Some(t), true) => format!("{} {t}", h.name),
            _ => h.name.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The per-hire metadata, or `None` when there is nobody to show.
fn hires_meta(state: &State) -> Option<HiresMeta> {
    let hires = hire_summaries(state);
    if hires.is_empty() {
        return None;
    }
    let of = |wanted: HireState| -> Vec<_> {
        hires
            .iter()
            .filter(|h| h.state == wanted)
            .cloned()
            .collect()
    };
    let working = of(HireState::Working);
    let blocked = of(HireState::Blocked);
    let rest: Vec<_> = hires
        .iter()
        .filter(|h| !matches!(h.state, HireState::Working | HireState::Blocked))
        .cloned()
        .collect();
    let mut labels = Vec::new();
    if !working.is_empty() {
        labels.push(("working", names(&working, true)));
    }
    if !blocked.is_empty() {
        labels.push(("blocked", names(&blocked, false)));
    }
    if !rest.is_empty() {
        labels.push(("idle", compact(&rest, 100)));
    }
    Some(HiresMeta {
        hires: compact(&hires, 80),
        active: working.len(),
        labels,
        display_agent: if working.is_empty() {
            "chillax".into()
        } else {
            format!("chillax ({} working)", working.len())
        },
    })
}

/// Send the hires metadata when it changed (or is due a refresh), no sooner than
/// `hires_interval` after the last send; a change that has to wait is sent on a later tick.
async fn publish_hires(herdr: &Herdr, options: &Options, sent: &mut Sent, state: &State) {
    let wanted = hires_meta(state);
    let now = Instant::now();
    let refresh = sent.hires.is_some()
        && sent
            .hires_at
            .is_some_and(|at| now.saturating_duration_since(at) >= options.hires_refresh);
    if wanted == sent.hires && !refresh {
        sent.hires_due = None;
        return;
    }
    if let Some(at) = sent.hires_at {
        let next = at + options.hires_interval;
        if now < next {
            sent.hires_due = Some(next);
            return;
        }
    }
    sent.hires_due = None;
    let ok = match &wanted {
        Some(meta) => {
            // A label that no longer applies must not linger.
            let dropped = sent.hires.as_ref().is_some_and(|old| {
                old.labels
                    .iter()
                    .any(|(s, _)| !meta.labels.iter().any(|(n, _)| n == s))
            });
            if dropped {
                herdr.clear_hires(true).await;
            }
            herdr.report_hires(meta).await
        }
        None => herdr.clear_hires(false).await,
    };
    if ok {
        sent.hires = wanted;
        sent.hires_at = Some(now);
    }
}

async fn publish(herdr: &Herdr, options: &Options, sent: &mut Sent, state: &State) {
    let summary = agent_summary(state);
    if sent.state != Some(summary.state) && herdr.report_agent(summary.state).await {
        sent.state = Some(summary.state);
    }
    if options.metadata {
        let tokens = tokens(&summary);
        if sent.tokens.as_ref() != Some(&tokens) && herdr.report_metadata(&tokens).await {
            sent.tokens = Some(tokens);
        }
        publish_hires(herdr, options, sent, state).await;
    }
}

/// Report the current state, then follow `events` until the hub closes or the task is
/// aborted. `events` must have been subscribed before the first snapshot, so that replay
/// never notifies and nothing in between is missed.
pub async fn run(
    company: Arc<dyn CompanyHandle>,
    herdr: Arc<Herdr>,
    mut events: broadcast::Receiver<Event>,
    options: Options,
) {
    let notifier = HerdrNotifier::new(herdr.clone());
    let mut sent = Sent::default();
    let mut state = company.snapshot().await;
    let mut seen = state.last_seq;
    publish(&herdr, &options, &mut sent, &state).await;
    loop {
        let mut live: Vec<Event> = Vec::new();
        let mut lagged = false;
        let received = match sent.wake_at(&options) {
            Some(at) => tokio::select! {
                r = events.recv() => Some(r),
                () = tokio::time::sleep_until(tokio::time::Instant::from_std(at)) => None,
            },
            None => Some(events.recv().await),
        };
        let Some(received) = received else {
            // Nothing happened: a deferred hires change or a refresh is due.
            state = company.snapshot().await;
            publish(&herdr, &options, &mut sent, &state).await;
            continue;
        };
        match received {
            Ok(event) => live.push(event),
            Err(RecvError::Lagged(_)) => lagged = true,
            Err(RecvError::Closed) => return,
        }
        tokio::time::sleep(options.debounce).await;
        loop {
            match events.try_recv() {
                Ok(event) => live.push(event),
                Err(TryRecvError::Lagged(_)) => lagged = true,
                Err(TryRecvError::Empty | TryRecvError::Closed) => break,
            }
        }
        state = company.snapshot().await;
        // Events already inside the first snapshot are old news.
        live.retain(|e| e.seq > seen);
        seen = seen.max(state.last_seq);
        if options.notifications && !lagged {
            for event in &live {
                for n in notifications_for(event, &state) {
                    notifier.notify(n).await;
                }
            }
        }
        publish(&herdr, &options, &mut sent, &state).await;
    }
}
