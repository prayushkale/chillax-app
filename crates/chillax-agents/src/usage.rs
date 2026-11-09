//! Per-session usage tally (ADR 0023). The notification handler feeds it context and
//! cost updates, the prompt loop turns it into one `UsageRecorded` per turn, and the
//! close paths flush what is left and post one comment. It lives in an `Arc` shared with
//! the `SessionHandle`, so it survives the session task being aborted.

use std::sync::Mutex;

use agent_client_protocol::schema::v1::Usage;
use chillax_core::usage::USD;
use chillax_core::{EventKind, PriceTable, TicketId};

use crate::convert::UsageReading;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Tokens {
    input: u64,
    output: u64,
    cached: u64,
    cache_write: u64,
}

impl Tokens {
    fn from_usage(u: &Usage) -> Self {
        Self {
            input: u.input_tokens,
            // Thought tokens are billed as output.
            output: u
                .output_tokens
                .saturating_add(u.thought_tokens.unwrap_or(0)),
            cached: u.cached_read_tokens.unwrap_or(0),
            cache_write: u.cached_write_tokens.unwrap_or(0),
        }
    }

    fn since(self, earlier: Self) -> Self {
        Self {
            input: self.input.saturating_sub(earlier.input),
            output: self.output.saturating_sub(earlier.output),
            cached: self.cached.saturating_sub(earlier.cached),
            cache_write: self.cache_write.saturating_sub(earlier.cache_write),
        }
    }

    fn is_empty(self) -> bool {
        self == Self::default()
    }
}

#[derive(Debug, Default)]
struct Inner {
    model: Option<String>,
    context: Option<(u64, u64)>,
    /// The session's cumulative cost as last reported, and its currency.
    cost_cumulative: Option<(u64, String)>,
    /// How much of `cost_cumulative` earlier samples already carried.
    cost_recorded: u64,
    /// Raw usage of the previous turn, for backends that report running totals.
    last_usage_raw: Option<Tokens>,
    /// A usage update arrived since the last sample.
    dirty: bool,
    // What this session recorded, for the closing comment.
    sum: Tokens,
    sum_cost: Option<u64>,
    sum_currency: String,
    sum_estimated: bool,
    samples: u32,
    commented: bool,
}

/// What the closing comment needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommentData {
    pub ticket: TicketId,
    pub employee: String,
    pub model: Option<String>,
    pub input: u64,
    pub output: u64,
    pub cached: u64,
    pub cost_micros: Option<u64>,
    pub currency: String,
    pub estimated: bool,
}

#[derive(Debug)]
pub(crate) struct SessionUsage {
    pub session_id: String,
    pub employee: String,
    pub ticket: Option<TicketId>,
    inner: Mutex<Inner>,
}

impl SessionUsage {
    pub fn new(session_id: String, employee: String, ticket: Option<TicketId>) -> Self {
        Self {
            session_id,
            employee,
            ticket,
            inner: Mutex::default(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("usage lock poisoned")
    }

    pub fn set_model(&self, model: Option<String>) {
        self.lock().model = model;
    }

    /// A `usage_update` notification arrived.
    pub fn on_update(&self, reading: UsageReading) {
        let mut t = self.lock();
        t.dirty = true;
        t.context = Some((reading.context_used, reading.context_size));
        if let Some(cost) = reading.cost_micros {
            // A new currency starts a new base: the old one's remainder is not comparable.
            if t.cost_cumulative
                .as_ref()
                .is_some_and(|(_, c)| *c != reading.currency)
            {
                t.cost_recorded = 0;
            }
            t.cost_cumulative = Some((cost, reading.currency));
        }
    }

    /// The sample for a finished turn, or `None` when the backend reported nothing.
    pub fn take_turn_sample(
        &self,
        usage: Option<&Usage>,
        cumulative: bool,
        prices: &PriceTable,
    ) -> Option<EventKind> {
        let mut t = self.lock();
        let raw = usage.map(Tokens::from_usage);
        let tokens = match raw {
            Some(raw) if cumulative => {
                let delta = raw.since(t.last_usage_raw.unwrap_or_default());
                t.last_usage_raw = Some(raw);
                delta
            }
            Some(raw) => raw,
            None => Tokens::default(),
        };
        if tokens.is_empty() && !t.dirty {
            return None;
        }
        t.dirty = false;

        let (cost_micros, currency, estimated) =
            if let Some((cum, currency)) = t.cost_cumulative.clone() {
                // Reported cost wins, even when it is zero.
                let delta = cum.saturating_sub(t.cost_recorded);
                t.cost_recorded = t.cost_recorded.max(cum);
                (Some(delta), currency, false)
            } else if !tokens.is_empty()
                && let Some(micros) = t.model.as_deref().and_then(|m| {
                    prices.estimate(
                        m,
                        tokens.input,
                        tokens.output,
                        tokens.cached,
                        tokens.cache_write,
                    )
                })
            {
                (Some(micros), USD.to_string(), true)
            } else {
                (None, USD.to_string(), false)
            };
        Some(t.sample(
            &self.session_id,
            &self.employee,
            self.ticket.clone(),
            tokens,
            cost_micros,
            currency,
            estimated,
            true,
        ))
    }

    /// Cost reported after the last sample (a late update, or an aborted turn), as a final
    /// zero-token sample.
    pub fn take_flush(&self) -> Option<EventKind> {
        let mut t = self.lock();
        let (cum, currency) = t.cost_cumulative.clone()?;
        if cum <= t.cost_recorded {
            return None;
        }
        let delta = cum - t.cost_recorded;
        t.cost_recorded = cum;
        Some(t.sample(
            &self.session_id,
            &self.employee,
            self.ticket.clone(),
            Tokens::default(),
            Some(delta),
            currency,
            false,
            false,
        ))
    }

    /// The one comment for this session, once. `None` for a chat, a session that recorded
    /// nothing, or one that was already commented.
    pub fn take_comment(&self) -> Option<CommentData> {
        let ticket = self.ticket.clone()?;
        let mut t = self.lock();
        if t.commented || t.samples == 0 {
            return None;
        }
        t.commented = true;
        Some(CommentData {
            ticket,
            employee: self.employee.clone(),
            model: t.model.clone(),
            input: t.sum.input,
            output: t.sum.output,
            cached: t.sum.cached,
            cost_micros: t.sum_cost,
            currency: if t.sum_currency.is_empty() {
                USD.to_string()
            } else {
                t.sum_currency.clone()
            },
            estimated: t.sum_estimated,
        })
    }
}

impl Inner {
    #[allow(clippy::too_many_arguments)]
    fn sample(
        &mut self,
        session: &str,
        employee: &str,
        ticket: Option<TicketId>,
        tokens: Tokens,
        cost_micros: Option<u64>,
        currency: String,
        estimated: bool,
        with_context: bool,
    ) -> EventKind {
        self.samples += 1;
        self.sum.input = self.sum.input.saturating_add(tokens.input);
        self.sum.output = self.sum.output.saturating_add(tokens.output);
        self.sum.cached = self.sum.cached.saturating_add(tokens.cached);
        self.sum.cache_write = self.sum.cache_write.saturating_add(tokens.cache_write);
        if let Some(c) = cost_micros {
            self.sum_cost = Some(self.sum_cost.unwrap_or(0).saturating_add(c));
            self.sum_currency = currency.clone();
            self.sum_estimated |= estimated;
        }
        let (context_used, context_size) = match self.context.filter(|_| with_context) {
            Some((used, size)) => (Some(used), Some(size)),
            None => (None, None),
        };
        EventKind::UsageRecorded {
            employee: employee.to_string(),
            ticket,
            session: session.to_string(),
            model: self.model.clone(),
            input_tokens: tokens.input,
            output_tokens: tokens.output,
            cached_tokens: tokens.cached,
            cache_write_tokens: tokens.cache_write,
            cost_micros,
            currency,
            estimated,
            context_used,
            context_size,
        }
    }
}

#[cfg(test)]
mod tests {
    use chillax_core::ModelPrice;

    use super::*;

    fn tally(ticket: Option<&str>) -> SessionUsage {
        SessionUsage::new("s1".into(), "dev".into(), ticket.map(str::to_string))
    }

    fn reading(cost: Option<u64>, currency: &str) -> UsageReading {
        UsageReading {
            context_used: 10,
            context_size: 100,
            cost_micros: cost,
            currency: currency.into(),
        }
    }

    fn cost_of(kind: Option<EventKind>) -> Option<u64> {
        match kind {
            Some(EventKind::UsageRecorded { cost_micros, .. }) => cost_micros,
            other => panic!("expected a sample, got {other:?}"),
        }
    }

    fn usage(i: u64, o: u64, c: u64) -> Usage {
        Usage::new(i + o, i, o).cached_read_tokens(c)
    }

    fn prices() -> PriceTable {
        PriceTable::new(&std::collections::BTreeMap::from([(
            "m".to_string(),
            ModelPrice {
                input_micros_per_mtok: 1_000_000,
                output_micros_per_mtok: 2_000_000,
                cached_micros_per_mtok: 1_000_000,
                cache_write_micros_per_mtok: 1_000_000,
            },
        )]))
    }

    #[test]
    fn nothing_reported_records_nothing() {
        let t = tally(Some("T-1"));
        assert!(
            t.take_turn_sample(None, false, &PriceTable::default())
                .is_none()
        );
        assert!(t.take_flush().is_none());
        assert!(t.take_comment().is_none());
    }

    #[test]
    fn cumulative_cost_becomes_per_turn_deltas() {
        let t = tally(Some("T-1"));
        let none = PriceTable::default();
        t.on_update(reading(Some(250_000), "USD"));
        assert_eq!(
            cost_of(t.take_turn_sample(None, false, &none)),
            Some(250_000)
        );
        t.on_update(reading(Some(500_000), "USD"));
        assert_eq!(
            cost_of(t.take_turn_sample(None, false, &none)),
            Some(250_000)
        );
        // A cost that goes down is clamped, never negative.
        t.on_update(reading(Some(400_000), "USD"));
        assert_eq!(cost_of(t.take_turn_sample(None, false, &none)), Some(0));
        t.on_update(reading(Some(600_000), "USD"));
        assert_eq!(
            cost_of(t.take_turn_sample(None, false, &none)),
            Some(100_000)
        );
    }

    #[test]
    fn a_late_update_is_flushed_once() {
        let t = tally(Some("T-1"));
        let none = PriceTable::default();
        t.on_update(reading(Some(250_000), "USD"));
        t.take_turn_sample(None, false, &none);
        t.on_update(reading(Some(300_000), "USD"));
        assert_eq!(cost_of(t.take_flush()), Some(50_000));
        assert!(t.take_flush().is_none());
    }

    #[test]
    fn a_new_currency_starts_a_new_base() {
        let t = tally(None);
        let none = PriceTable::default();
        t.on_update(reading(Some(1_000_000), "USD"));
        t.take_turn_sample(None, false, &none);
        t.on_update(reading(Some(200_000), "EUR"));
        match t.take_turn_sample(None, false, &none) {
            Some(EventKind::UsageRecorded {
                cost_micros,
                currency,
                ..
            }) => {
                assert_eq!(cost_micros, Some(200_000));
                assert_eq!(currency, "EUR");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn tokens_without_a_reported_cost_are_estimated_when_priced() {
        let t = tally(None);
        t.set_model(Some("m".into()));
        match t.take_turn_sample(Some(&usage(1_000_000, 500_000, 0)), false, &prices()) {
            Some(EventKind::UsageRecorded {
                cost_micros,
                estimated,
                input_tokens,
                ..
            }) => {
                assert_eq!(cost_micros, Some(2_000_000));
                assert!(estimated);
                assert_eq!(input_tokens, 1_000_000);
            }
            other => panic!("{other:?}"),
        }
        // No price for the model: tokens recorded, cost unknown.
        let t = tally(None);
        t.set_model(Some("other".into()));
        assert_eq!(
            cost_of(t.take_turn_sample(Some(&usage(5, 5, 0)), false, &prices())),
            None
        );
    }

    #[test]
    fn thought_tokens_fold_into_output() {
        let t = tally(None);
        let u = usage(10, 5, 0).thought_tokens(7u64);
        match t.take_turn_sample(Some(&u), false, &PriceTable::default()) {
            Some(EventKind::UsageRecorded { output_tokens, .. }) => assert_eq!(output_tokens, 12),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn running_totals_become_deltas_when_configured() {
        let t = tally(None);
        let none = PriceTable::default();
        let tokens = |k: Option<EventKind>| match k {
            Some(EventKind::UsageRecorded { input_tokens, .. }) => input_tokens,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            tokens(t.take_turn_sample(Some(&usage(100, 10, 0)), true, &none)),
            100
        );
        assert_eq!(
            tokens(t.take_turn_sample(Some(&usage(250, 30, 0)), true, &none)),
            150
        );
        // A total that goes backwards (a reset) is clamped to nothing.
        assert!(
            t.take_turn_sample(Some(&usage(50, 5, 0)), true, &none)
                .is_none()
        );
    }

    #[test]
    fn the_comment_fires_once_and_only_for_ticket_sessions() {
        let t = tally(Some("T-1"));
        let none = PriceTable::default();
        t.on_update(reading(Some(250_000), "USD"));
        t.take_turn_sample(Some(&usage(10, 5, 2)), false, &none);
        let c = t.take_comment().expect("a comment");
        assert_eq!((c.input, c.output, c.cached), (10, 5, 2));
        assert_eq!(c.cost_micros, Some(250_000));
        assert!(t.take_comment().is_none());

        let chat = tally(None);
        chat.take_turn_sample(Some(&usage(10, 5, 2)), false, &none);
        assert!(chat.take_comment().is_none());
    }
}
