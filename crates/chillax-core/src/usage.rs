//! Token and cost accounting: the projection of `UsageRecorded` events and the text of the
//! per-ticket usage comment (ADR 0023). Pure; prices are applied by the runtime when it
//! records, never here, so history does not change when a price table does.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::engine::State;
use crate::model::{EmployeeId, TicketId};

/// Days of per-day totals kept.
pub const MAX_DAYS: usize = 90;
/// Hours kept in `by_hour` (one week), so the hourly series stays small.
pub const MAX_HOURS: usize = 24 * 7;
/// Length of the hourly series in [`UsageView`].
pub const HOURLY_POINTS: usize = 48;
/// Recent samples kept for the Usage views.
pub const MAX_RECENT: usize = 100;
/// The currency totals are kept in; others go to `other_currency_micros`.
pub const USD: &str = "USD";

/// Summed usage. Costs are millionths of a dollar.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageTotals {
    #[serde(default)]
    pub input_tokens: u64,
    /// Output plus thought tokens.
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cached_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
    /// Samples that carried tokens.
    #[serde(default)]
    pub turns: u32,
    /// Cost the backend reported, in USD.
    #[serde(default)]
    pub cost_micros: u64,
    /// Cost estimated from the price table, in USD.
    #[serde(default)]
    pub estimated_cost_micros: u64,
    /// Samples with tokens and no cost at all (no report, no price).
    #[serde(default)]
    pub unpriced_samples: u32,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub other_currency_micros: BTreeMap<String, u64>,
}

impl UsageTotals {
    fn add(&mut self, u: &UsageInput<'_>) {
        self.input_tokens = self.input_tokens.saturating_add(u.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(u.output_tokens);
        self.cached_tokens = self.cached_tokens.saturating_add(u.cached_tokens);
        self.cache_write_tokens = self.cache_write_tokens.saturating_add(u.cache_write_tokens);
        let has_tokens = u.has_tokens();
        if has_tokens {
            self.turns += 1;
        }
        match u.cost_micros {
            Some(c) if u.currency == USD && u.estimated => {
                self.estimated_cost_micros = self.estimated_cost_micros.saturating_add(c)
            }
            Some(c) if u.currency == USD => self.cost_micros = self.cost_micros.saturating_add(c),
            Some(c) => {
                let slot = self
                    .other_currency_micros
                    .entry(u.currency.to_string())
                    .or_default();
                *slot = slot.saturating_add(c);
            }
            None if has_tokens => self.unpriced_samples += 1,
            None => {}
        }
    }

    /// Input, output and cached tokens together (cache writes are not shown separately).
    pub fn tokens_total(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cached_tokens)
    }

    /// Any USD cost, reported or estimated.
    pub fn has_cost(&self) -> bool {
        self.total_cost_micros() > 0
    }

    /// Part of the USD cost is an estimate from the price table.
    pub fn is_estimated(&self) -> bool {
        self.estimated_cost_micros > 0
    }

    /// What was added since `base` was taken. Saturating, so a base from a different
    /// log (or a pruned day) never underflows.
    pub fn delta_since(&self, base: &UsageTotals) -> UsageTotals {
        let mut other = BTreeMap::new();
        for (cur, v) in &self.other_currency_micros {
            let d = v.saturating_sub(base.other_currency_micros.get(cur).copied().unwrap_or(0));
            if d > 0 {
                other.insert(cur.clone(), d);
            }
        }
        UsageTotals {
            input_tokens: self.input_tokens.saturating_sub(base.input_tokens),
            output_tokens: self.output_tokens.saturating_sub(base.output_tokens),
            cached_tokens: self.cached_tokens.saturating_sub(base.cached_tokens),
            cache_write_tokens: self
                .cache_write_tokens
                .saturating_sub(base.cache_write_tokens),
            turns: self.turns.saturating_sub(base.turns),
            cost_micros: self.cost_micros.saturating_sub(base.cost_micros),
            estimated_cost_micros: self
                .estimated_cost_micros
                .saturating_sub(base.estimated_cost_micros),
            unpriced_samples: self.unpriced_samples.saturating_sub(base.unpriced_samples),
            other_currency_micros: other,
        }
    }

    fn merge(&mut self, o: &UsageTotals) {
        self.input_tokens = self.input_tokens.saturating_add(o.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(o.output_tokens);
        self.cached_tokens = self.cached_tokens.saturating_add(o.cached_tokens);
        self.cache_write_tokens = self.cache_write_tokens.saturating_add(o.cache_write_tokens);
        self.turns = self.turns.saturating_add(o.turns);
        self.cost_micros = self.cost_micros.saturating_add(o.cost_micros);
        self.estimated_cost_micros = self
            .estimated_cost_micros
            .saturating_add(o.estimated_cost_micros);
        self.unpriced_samples = self.unpriced_samples.saturating_add(o.unpriced_samples);
        for (c, v) in &o.other_currency_micros {
            let slot = self.other_currency_micros.entry(c.clone()).or_default();
            *slot = slot.saturating_add(*v);
        }
    }

    /// Reported plus estimated USD cost.
    pub fn total_cost_micros(&self) -> u64 {
        self.cost_micros.saturating_add(self.estimated_cost_micros)
    }
}

/// What one employee's backend has told us.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmployeeUsage {
    #[serde(default)]
    pub totals: UsageTotals,
    /// Every turn the employee finished, reporting or not.
    #[serde(default)]
    pub turns_total: u32,
    /// Turns that produced a usage sample.
    #[serde(default)]
    pub sampled_turns: u32,
    #[serde(default)]
    pub reports_tokens: bool,
    #[serde(default)]
    pub reports_cost: bool,
    #[serde(default)]
    pub reports_context: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sample_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_model: Option<String>,
}

/// What a view should say about an employee's usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageHealth {
    Ok,
    /// Turns finished but the backend never reported usage.
    ReportsNothing,
    /// Tokens but no cost: set `[pricing."model"]` for an estimate.
    NoCost,
    /// No turns yet.
    Idle,
}

impl EmployeeUsage {
    pub fn health(&self) -> UsageHealth {
        if self.turns_total == 0 && self.sampled_turns == 0 {
            UsageHealth::Idle
        } else if self.sampled_turns == 0 {
            UsageHealth::ReportsNothing
        } else if self.totals.tokens_total() > 0
            && self.totals.total_cost_micros() == 0
            && self.totals.unpriced_samples > 0
        {
            UsageHealth::NoCost
        } else {
            UsageHealth::Ok
        }
    }
}

impl ContextFill {
    /// Percent of the window used, 0 to 100; 0 when the size is unknown.
    pub fn percent(&self) -> u8 {
        if self.size == 0 {
            return 0;
        }
        (self.used.saturating_mul(100) / self.size).min(100) as u8
    }
}

/// One day of the daily series.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DayPoint {
    pub date: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub cost_micros: u64,
    pub estimated_cost_micros: u64,
    pub turns: u32,
}

impl DayPoint {
    pub fn tokens_total(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cached_tokens)
    }
}

/// One hour of the hourly series.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HourPoint {
    /// UTC hour, `YYYY-MM-DDTHH`.
    pub hour: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub cost_micros: u64,
    pub estimated_cost_micros: u64,
    pub turns: u32,
}

impl HourPoint {
    pub fn tokens_total(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cached_tokens)
    }
}

impl UsageStats {
    /// Exactly `hours` points ending at the hour of `now` (UTC), oldest first; hours
    /// without usage are zero.
    pub fn hourly_series(&self, hours: usize, now: DateTime<Utc>) -> Vec<HourPoint> {
        (0..hours)
            .rev()
            .map(|back| {
                let key = (now - Duration::hours(back as i64))
                    .format("%Y-%m-%dT%H")
                    .to_string();
                let t = self.by_hour.get(&key).cloned().unwrap_or_default();
                HourPoint {
                    hour: key,
                    input_tokens: t.input_tokens,
                    output_tokens: t.output_tokens,
                    cached_tokens: t.cached_tokens,
                    cost_micros: t.cost_micros,
                    estimated_cost_micros: t.estimated_cost_micros,
                    turns: t.turns,
                }
            })
            .collect()
    }

    /// Exactly `days` points ending at `today` (UTC), oldest first; days without
    /// usage are zero.
    pub fn daily_series(&self, days: usize, today: NaiveDate) -> Vec<DayPoint> {
        (0..days)
            .rev()
            .map(|back| {
                let date = today - Duration::days(back as i64);
                let key = date.format("%Y-%m-%d").to_string();
                let t = self.by_day.get(&key).cloned().unwrap_or_default();
                DayPoint {
                    date: key,
                    input_tokens: t.input_tokens,
                    output_tokens: t.output_tokens,
                    cached_tokens: t.cached_tokens,
                    cost_micros: t.cost_micros,
                    estimated_cost_micros: t.estimated_cost_micros,
                    turns: t.turns,
                }
            })
            .collect()
    }

    /// Everything in the last `days` days, today included.
    pub fn window(&self, days: usize, today: NaiveDate) -> UsageTotals {
        let mut sum = UsageTotals::default();
        for back in 0..days {
            let key = (today - Duration::days(back as i64))
                .format("%Y-%m-%d")
                .to_string();
            if let Some(t) = self.by_day.get(&key) {
                sum.merge(t);
            }
        }
        sum
    }

    /// The `n` costliest tickets, then by tokens, then by id.
    pub fn top_tickets(&self, n: usize) -> Vec<(&TicketId, &UsageTotals)> {
        let mut all: Vec<_> = self.by_ticket.iter().collect();
        all.sort_by(|a, b| {
            b.1.total_cost_micros()
                .cmp(&a.1.total_cost_micros())
                .then(b.1.tokens_total().cmp(&a.1.tokens_total()))
                .then(a.0.cmp(b.0))
        });
        all.truncate(n);
        all
    }
}

/// How full a live session's context window is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextFill {
    pub used: u64,
    pub size: u64,
    pub session: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket: Option<TicketId>,
    pub at: DateTime<Utc>,
}

/// One recorded sample, for the "recent" list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageSample {
    pub seq: u64,
    pub at: DateTime<Utc>,
    pub employee: EmployeeId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket: Option<TicketId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub session: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_micros: Option<u64>,
    pub currency: String,
    pub estimated: bool,
}

/// The `State.usage` projection. Bounded: `by_day` keeps [`MAX_DAYS`], `recent`
/// [`MAX_RECENT`], `context` only live sessions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageStats {
    #[serde(default)]
    pub total: UsageTotals,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub by_ticket: BTreeMap<TicketId, UsageTotals>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub by_employee: BTreeMap<EmployeeId, EmployeeUsage>,
    /// Key "unknown" when the backend never named a model.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub by_model: BTreeMap<String, UsageTotals>,
    /// Keyed by UTC date, `YYYY-MM-DD`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub by_day: BTreeMap<String, UsageTotals>,
    /// Keyed by UTC hour, `YYYY-MM-DDTHH`; keeps the last [`MAX_HOURS`] active hours.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub by_hour: BTreeMap<String, UsageTotals>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub context: BTreeMap<EmployeeId, ContextFill>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent: Vec<UsageSample>,
}

/// The fields of a `UsageRecorded` event, borrowed.
pub struct UsageInput<'a> {
    pub employee: &'a str,
    pub ticket: Option<&'a str>,
    pub session: &'a str,
    pub model: Option<&'a str>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub cache_write_tokens: u64,
    pub cost_micros: Option<u64>,
    pub currency: &'a str,
    pub estimated: bool,
    pub context_used: Option<u64>,
    pub context_size: Option<u64>,
}

impl UsageInput<'_> {
    fn has_tokens(&self) -> bool {
        self.input_tokens + self.output_tokens + self.cached_tokens + self.cache_write_tokens > 0
    }
}

impl UsageStats {
    /// Fold one `UsageRecorded` event. `at` is the event's timestamp.
    pub fn record(&mut self, seq: u64, at: DateTime<Utc>, u: &UsageInput<'_>) {
        self.total.add(u);
        if let Some(ticket) = u.ticket {
            self.by_ticket.entry(ticket.to_string()).or_default().add(u);
        }
        self.by_model
            .entry(u.model.unwrap_or("unknown").to_string())
            .or_default()
            .add(u);
        self.by_day
            .entry(at.format("%Y-%m-%d").to_string())
            .or_default()
            .add(u);
        while self.by_day.len() > MAX_DAYS {
            self.by_day.pop_first();
        }
        self.by_hour
            .entry(at.format("%Y-%m-%dT%H").to_string())
            .or_default()
            .add(u);
        while self.by_hour.len() > MAX_HOURS {
            self.by_hour.pop_first();
        }

        let has_tokens = u.has_tokens();
        let e = self.by_employee.entry(u.employee.to_string()).or_default();
        e.totals.add(u);
        e.last_sample_at = Some(at);
        if u.model.is_some() {
            e.last_model = u.model.map(str::to_string);
        }
        if has_tokens {
            e.sampled_turns += 1;
            e.reports_tokens = true;
        }
        if u.cost_micros.is_some() && !u.estimated {
            e.reports_cost = true;
        }
        if let (Some(used), Some(size)) = (u.context_used, u.context_size) {
            e.reports_context = true;
            self.context.insert(
                u.employee.to_string(),
                ContextFill {
                    used,
                    size,
                    session: u.session.to_string(),
                    ticket: u.ticket.map(str::to_string),
                    at,
                },
            );
        }

        self.recent.push(UsageSample {
            seq,
            at,
            employee: u.employee.to_string(),
            ticket: u.ticket.map(str::to_string),
            model: u.model.map(str::to_string),
            session: u.session.to_string(),
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cached_tokens: u.cached_tokens,
            cost_micros: u.cost_micros,
            currency: u.currency.to_string(),
            estimated: u.estimated,
        });
        if self.recent.len() > MAX_RECENT {
            let excess = self.recent.len() - MAX_RECENT;
            self.recent.drain(..excess);
        }
    }

    /// An agent finished a turn, whether or not it reported usage.
    pub fn turn_ended(&mut self, employee: &str) {
        self.by_employee
            .entry(employee.to_string())
            .or_default()
            .turns_total += 1;
    }

    /// The employee's session is gone, so its context fill is stale.
    pub fn session_closed(&mut self, employee: &str) {
        self.context.remove(employee);
    }
}

/// `12.4k`, `3.1M`, or the plain number below a thousand.
pub fn format_tokens(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}k", n as f64 / 1_000.0),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

/// `$0.18`, `$0.0012` below a cent, or `0.18 EUR` for other currencies.
pub fn format_money(micros: u64, currency: &str) -> String {
    let value = if micros < 10_000 {
        format!("{:.4}", micros as f64 / 1e6)
    } else {
        format!("{:.2}", micros as f64 / 1e6)
    };
    if currency == USD {
        format!("${value}")
    } else {
        format!("{value} {currency}")
    }
}

/// The `/api/v1/usage` body: the derivations every frontend would otherwise repeat.
#[derive(Debug, Clone, Serialize)]
pub struct UsageView {
    pub generated_at: DateTime<Utc>,
    pub days: usize,
    pub last_seq: u64,
    pub total: UsageTotals,
    pub cost: CostSummary,
    pub windows: Windows,
    pub burn: Burn,
    pub daily: Vec<DayPoint>,
    /// The last [`HOURLY_POINTS`] hours, zero-filled, oldest first.
    pub hourly: Vec<HourPoint>,
    pub employees: Vec<EmployeeRow>,
    pub tickets: Vec<TicketRow>,
    pub tickets_total: usize,
    pub models: Vec<ModelRow>,
    pub context: Vec<ContextRow>,
    pub recent: Vec<RecentRow>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CostSummary {
    pub total_micros: u64,
    pub reported_micros: u64,
    pub estimated_micros: u64,
    pub estimated: bool,
    pub unpriced_samples: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct Windows {
    pub today: UsageTotals,
    pub last_7_days: UsageTotals,
    pub last_30_days: UsageTotals,
}

/// Seven-day sum over seven, empty days counting as zero.
#[derive(Debug, Clone, Serialize)]
pub struct Burn {
    pub tokens_per_day_7d: u64,
    pub cost_micros_per_day_7d: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct EmployeeRow {
    pub id: EmployeeId,
    pub name: String,
    pub role_title: String,
    pub backend: String,
    pub model: Option<String>,
    pub totals: UsageTotals,
    pub turns_total: u32,
    pub sampled_turns: u32,
    pub reports_tokens: bool,
    pub reports_cost: bool,
    pub reports_context: bool,
    pub last_sample_at: Option<DateTime<Utc>>,
    pub state: UsageHealth,
}

#[derive(Debug, Clone, Serialize)]
pub struct TicketRow {
    pub id: TicketId,
    pub title: Option<String>,
    pub totals: UsageTotals,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelRow {
    pub model: String,
    pub totals: UsageTotals,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContextRow {
    pub employee: EmployeeId,
    pub name: String,
    pub used: u64,
    pub size: u64,
    pub percent: u8,
    pub ticket: Option<TicketId>,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecentRow {
    #[serde(flatten)]
    pub sample: UsageSample,
    pub employee_name: String,
}

/// Build the Usage view of `state` as of `now` (UTC days are cut from it).
pub fn view(state: &State, now: DateTime<Utc>, days: usize, top: Option<usize>) -> UsageView {
    let u = &state.usage;
    let today = now.date_naive();
    let name_of = |id: &str| {
        state
            .employee(id)
            .map_or_else(|| id.to_string(), |e| e.employee.name.clone())
    };
    let week = u.window(7, today);

    let mut employees: Vec<EmployeeRow> = state
        .employees
        .iter()
        .map(|e| (e.employee.id.clone(), Some(e)))
        .chain(
            u.by_employee
                .keys()
                .filter(|id| state.employee(id).is_none())
                .map(|id| (id.clone(), None)),
        )
        .map(|(id, e)| {
            let eu = u.by_employee.get(&id).cloned().unwrap_or_default();
            EmployeeRow {
                name: e.map_or_else(|| id.clone(), |e| e.employee.name.clone()),
                role_title: e.map(|e| e.employee.role_title.clone()).unwrap_or_default(),
                backend: e.map(|e| e.employee.backend.clone()).unwrap_or_default(),
                model: eu
                    .last_model
                    .clone()
                    .or_else(|| e.and_then(|e| e.employee.model.clone())),
                state: eu.health(),
                turns_total: eu.turns_total,
                sampled_turns: eu.sampled_turns,
                reports_tokens: eu.reports_tokens,
                reports_cost: eu.reports_cost,
                reports_context: eu.reports_context,
                last_sample_at: eu.last_sample_at,
                totals: eu.totals,
                id,
            }
        })
        .collect();
    employees.sort_by(|a, b| {
        b.totals
            .total_cost_micros()
            .cmp(&a.totals.total_cost_micros())
            .then(b.totals.tokens_total().cmp(&a.totals.tokens_total()))
            .then(a.id.cmp(&b.id))
    });

    UsageView {
        generated_at: now,
        days,
        last_seq: state.last_seq,
        cost: CostSummary {
            total_micros: u.total.total_cost_micros(),
            reported_micros: u.total.cost_micros,
            estimated_micros: u.total.estimated_cost_micros,
            estimated: u.total.is_estimated(),
            unpriced_samples: u.total.unpriced_samples,
        },
        total: u.total.clone(),
        windows: Windows {
            today: u.window(1, today),
            last_30_days: u.window(30, today),
            last_7_days: week.clone(),
        },
        burn: Burn {
            tokens_per_day_7d: week.tokens_total() / 7,
            cost_micros_per_day_7d: week.total_cost_micros() / 7,
        },
        daily: u.daily_series(days, today),
        hourly: u.hourly_series(HOURLY_POINTS, now),
        employees,
        tickets: u
            .top_tickets(top.unwrap_or(usize::MAX))
            .into_iter()
            .map(|(id, t)| TicketRow {
                title: state.ticket(id).map(|t| t.title.clone()),
                id: id.clone(),
                totals: t.clone(),
            })
            .collect(),
        tickets_total: u.by_ticket.len(),
        models: u
            .by_model
            .iter()
            .map(|(m, t)| ModelRow {
                model: m.clone(),
                totals: t.clone(),
            })
            .collect(),
        context: u
            .context
            .iter()
            .map(|(id, c)| ContextRow {
                employee: id.clone(),
                name: name_of(id),
                used: c.used,
                size: c.size,
                percent: c.percent(),
                ticket: c.ticket.clone(),
                at: c.at,
            })
            .collect(),
        recent: u
            .recent
            .iter()
            .rev()
            .take(20)
            .map(|s| RecentRow {
                employee_name: name_of(&s.employee),
                sample: s.clone(),
            })
            .collect(),
    }
}

/// What the usage comment on a closed ticket session says.
pub struct UsageComment<'a> {
    pub employee: &'a str,
    pub model: Option<&'a str>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub cost_micros: Option<u64>,
    pub currency: &'a str,
    pub estimated: bool,
    /// Everything recorded on the ticket so far, in USD, and whether any is estimated.
    pub ticket_total_micros: u64,
    pub ticket_total_estimated: bool,
}

pub fn format_usage_comment(c: &UsageComment<'_>) -> String {
    let mut tokens = format!(
        "Usage: {} in / {} out",
        format_tokens(c.input_tokens),
        format_tokens(c.output_tokens)
    );
    if c.cached_tokens > 0 {
        tokens.push_str(&format!(" ({} cached)", format_tokens(c.cached_tokens)));
    }
    let mut parts = vec![tokens];
    match c.cost_micros {
        Some(m) if c.estimated => {
            parts.push(format!("~{} (estimated)", format_money(m, c.currency)))
        }
        Some(m) => parts.push(format_money(m, c.currency)),
        None => parts.push(match c.model {
            Some(model) => format!("cost unknown (no price for model {model})"),
            None => "cost unknown".to_string(),
        }),
    }
    parts.push(c.employee.to_string());
    if let (Some(model), true) = (c.model, c.cost_micros.is_some()) {
        parts.push(model.to_string());
    }
    if c.ticket_total_micros > 0 {
        let tilde = if c.ticket_total_estimated { "~" } else { "" };
        parts.push(format!(
            "ticket total {tilde}{}",
            format_money(c.ticket_total_micros, USD)
        ));
    }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).expect("valid").to_utc()
    }

    fn input<'a>(employee: &'a str, ticket: Option<&'a str>) -> UsageInput<'a> {
        UsageInput {
            employee,
            ticket,
            session: "s1",
            model: Some("m"),
            input_tokens: 100,
            output_tokens: 10,
            cached_tokens: 5,
            cache_write_tokens: 0,
            cost_micros: Some(1_000),
            currency: "USD",
            estimated: false,
            context_used: None,
            context_size: None,
        }
    }

    #[test]
    fn totals_span_employees_and_tickets() {
        let mut s = UsageStats::default();
        let t = at("2026-10-24T10:00:00Z");
        s.record(1, t, &input("dev", Some("T-1")));
        s.record(2, t, &input("dev", Some("T-2")));
        s.record(3, t, &input("qa", Some("T-1")));
        assert_eq!(s.total.input_tokens, 300);
        assert_eq!(s.total.cost_micros, 3_000);
        assert_eq!(s.total.turns, 3);
        assert_eq!(s.by_ticket["T-1"].input_tokens, 200);
        assert_eq!(s.by_employee["dev"].totals.cost_micros, 2_000);
        assert_eq!(s.by_employee["dev"].sampled_turns, 2);
        assert!(s.by_employee["dev"].reports_cost);
        assert_eq!(s.by_model["m"].turns, 3);
    }

    #[test]
    fn days_bucket_by_utc_and_are_pruned() {
        let mut s = UsageStats::default();
        s.record(1, at("2026-10-24T23:59:59Z"), &input("dev", None));
        s.record(2, at("2026-10-25T00:00:01Z"), &input("dev", None));
        assert_eq!(s.by_day["2026-10-24"].turns, 1);
        assert_eq!(s.by_day["2026-10-25"].turns, 1);
        for d in 0..100 {
            let ts = at("2027-01-01T00:00:00Z") + chrono::Duration::days(d);
            s.record(10 + d as u64, ts, &input("dev", None));
        }
        assert_eq!(s.by_day.len(), MAX_DAYS);
        assert!(!s.by_day.contains_key("2026-10-24"));
    }

    #[test]
    fn hours_bucket_roll_over_and_are_pruned() {
        let mut s = UsageStats::default();
        s.record(1, at("2026-10-24T10:05:00Z"), &input("dev", None));
        s.record(2, at("2026-10-24T10:55:00Z"), &input("dev", None));
        assert_eq!(s.by_hour.len(), 1);
        assert_eq!(s.by_hour["2026-10-24T10"].turns, 2);
        s.record(3, at("2026-10-24T11:00:00Z"), &input("dev", None));
        s.record(4, at("2026-10-25T00:00:00Z"), &input("dev", None));
        assert_eq!(s.by_hour["2026-10-24T11"].turns, 1);
        assert_eq!(s.by_hour["2026-10-25T00"].turns, 1);
        let mut s = UsageStats::default();
        for h in 0..169 {
            let ts = at("2026-10-01T00:00:00Z") + chrono::Duration::hours(h);
            s.record(h as u64, ts, &input("dev", None));
        }
        assert_eq!(s.by_hour.len(), MAX_HOURS);
        assert!(!s.by_hour.contains_key("2026-10-01T00"));
    }

    #[test]
    fn view_without_top_lists_all_tickets() {
        let mut state = State::default();
        let t = at("2026-10-24T10:00:00Z");
        for n in 0..15 {
            let id = format!("T-{n}");
            state.usage.record(n, t, &input("dev", Some(&id)));
        }
        let now = at("2026-10-24T11:00:00Z");
        assert_eq!(view(&state, now, 14, None).tickets.len(), 15);
        let v = view(&state, now, 14, Some(3));
        assert_eq!((v.tickets.len(), v.tickets_total), (3, 15));
        assert_eq!(v.hourly.len(), HOURLY_POINTS);
        assert_eq!(v.hourly[HOURLY_POINTS - 2].turns, 15);
    }

    #[test]
    fn hourly_series_is_zero_filled_oldest_first() {
        let mut s = UsageStats::default();
        s.record(1, at("2026-10-24T10:30:00Z"), &input("dev", None));
        let series = s.hourly_series(3, at("2026-10-24T11:10:00Z"));
        let hours: Vec<_> = series.iter().map(|p| p.hour.as_str()).collect();
        assert_eq!(hours, ["2026-10-24T09", "2026-10-24T10", "2026-10-24T11"]);
        assert_eq!(series[0].turns, 0);
        assert_eq!(series[1].turns, 1);
        assert_eq!(series[1].tokens_total(), 115);
    }

    #[test]
    fn estimated_unpriced_and_foreign_costs_are_kept_apart() {
        let mut s = UsageStats::default();
        let t = at("2026-10-24T10:00:00Z");
        let mut est = input("dev", None);
        est.estimated = true;
        s.record(1, t, &est);
        let mut unknown = input("dev", None);
        unknown.cost_micros = None;
        unknown.model = None;
        s.record(2, t, &unknown);
        let mut eur = input("dev", None);
        eur.currency = "EUR";
        s.record(3, t, &eur);
        assert_eq!(s.total.estimated_cost_micros, 1_000);
        assert_eq!(s.total.cost_micros, 0);
        assert_eq!(s.total.unpriced_samples, 1);
        assert_eq!(s.total.other_currency_micros["EUR"], 1_000);
        assert_eq!(s.by_model["unknown"].unpriced_samples, 1);
        assert!(s.by_employee["dev"].reports_cost);
    }

    #[test]
    fn flush_samples_with_no_tokens_are_not_turns() {
        let mut s = UsageStats::default();
        let mut flush = input("dev", Some("T-1"));
        flush.input_tokens = 0;
        flush.output_tokens = 0;
        flush.cached_tokens = 0;
        s.record(1, at("2026-10-24T10:00:00Z"), &flush);
        assert_eq!(s.total.turns, 0);
        assert_eq!(s.total.cost_micros, 1_000);
        assert!(!s.by_employee["dev"].reports_tokens);
    }

    #[test]
    fn recent_is_capped_and_context_clears_on_close() {
        let mut s = UsageStats::default();
        let mut u = input("dev", None);
        u.context_used = Some(10);
        u.context_size = Some(100);
        for i in 0..150 {
            s.record(i, at("2026-10-24T10:00:00Z"), &u);
        }
        assert_eq!(s.recent.len(), MAX_RECENT);
        assert_eq!(s.recent[0].seq, 50);
        assert_eq!(s.context["dev"].used, 10);
        assert!(s.by_employee["dev"].reports_context);
        s.session_closed("dev");
        assert!(s.context.is_empty());
    }

    #[test]
    fn turns_total_counts_every_turn() {
        let mut s = UsageStats::default();
        s.turn_ended("dev");
        s.turn_ended("dev");
        s.record(1, at("2026-10-24T10:00:00Z"), &input("dev", None));
        assert_eq!(s.by_employee["dev"].turns_total, 2);
        assert_eq!(s.by_employee["dev"].sampled_turns, 1);
    }

    #[test]
    fn old_state_json_without_usage_still_loads() {
        let stats: UsageStats = serde_json::from_str("{}").expect("empty parses");
        assert_eq!(stats, UsageStats::default());
        assert!(
            !serde_json::to_string(&stats)
                .expect("ser")
                .contains("by_ticket")
        );
    }

    #[test]
    fn token_and_money_formatting() {
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(12_449), "12.4k");
        assert_eq!(format_tokens(2_500_000), "2.5M");
        assert_eq!(format_money(180_000, "USD"), "$0.18");
        assert_eq!(format_money(1_200, "USD"), "$0.0012");
        assert_eq!(format_money(180_000, "EUR"), "0.18 EUR");
    }

    fn comment() -> UsageComment<'static> {
        UsageComment {
            employee: "dev",
            model: Some("claude-sonnet-5"),
            input_tokens: 12_400,
            output_tokens: 3_100,
            cached_tokens: 2_200,
            cost_micros: Some(180_000),
            currency: "USD",
            estimated: true,
            ticket_total_micros: 520_000,
            ticket_total_estimated: true,
        }
    }

    #[test]
    fn comment_for_an_estimated_cost() {
        assert_eq!(
            format_usage_comment(&comment()),
            "Usage: 12.4k in / 3.1k out (2.2k cached) · ~$0.18 (estimated) · dev · claude-sonnet-5 · ticket total ~$0.52"
        );
    }

    #[test]
    fn comment_for_a_reported_cost_has_no_tilde() {
        let c = UsageComment {
            estimated: false,
            ticket_total_estimated: false,
            cached_tokens: 0,
            ..comment()
        };
        assert_eq!(
            format_usage_comment(&c),
            "Usage: 12.4k in / 3.1k out · $0.18 · dev · claude-sonnet-5 · ticket total $0.52"
        );
    }

    #[test]
    fn comment_without_a_cost_says_why() {
        let c = UsageComment {
            cost_micros: None,
            cached_tokens: 0,
            ticket_total_micros: 0,
            ..comment()
        };
        assert_eq!(
            format_usage_comment(&c),
            "Usage: 12.4k in / 3.1k out · cost unknown (no price for model claude-sonnet-5) · dev"
        );
        let c = UsageComment { model: None, ..c };
        assert!(format_usage_comment(&c).contains("cost unknown ·"));
    }

    #[test]
    fn comment_in_another_currency_prints_the_code() {
        let c = UsageComment {
            currency: "EUR",
            estimated: false,
            ..comment()
        };
        assert!(format_usage_comment(&c).contains("0.18 EUR"));
    }
}
