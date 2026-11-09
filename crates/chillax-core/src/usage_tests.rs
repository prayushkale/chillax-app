//! Usage events through the engine: projection, and old logs still loading.

use crate::dev_loop_tests::{dev, ready_ticket};
use crate::engine::{Engine, State};
use crate::event::{Actor, Event, EventKind};

fn usage(employee: &str, ticket: &str, cost: Option<u64>) -> EventKind {
    EventKind::UsageRecorded {
        employee: employee.into(),
        ticket: Some(ticket.into()),
        session: "s".into(),
        model: Some("m".into()),
        input_tokens: 10,
        output_tokens: 2,
        cached_tokens: 0,
        cache_write_tokens: 0,
        cost_micros: cost,
        currency: "USD".into(),
        estimated: false,
        context_used: Some(5),
        context_size: Some(50),
    }
}

fn apply(e: &mut Engine, actor: Actor, kind: EventKind) {
    let event = Event {
        seq: e.state.last_seq + 1,
        ts: chrono::DateTime::from_timestamp(0, 0).expect("epoch is valid"),
        actor,
        kind,
    };
    e.apply(&event);
}

#[test]
fn usage_events_fold_into_the_projection() {
    let (mut e, id) = ready_ticket();
    apply(&mut e, dev("dev"), usage("dev", &id, Some(500)));
    apply(&mut e, dev("dev"), usage("dev", &id, None));
    apply(
        &mut e,
        dev("dev"),
        EventKind::AgentTurnEnded {
            employee: "dev".into(),
            stop_reason: "end_turn".into(),
            ticket: None,
        },
    );
    let u = &e.state.usage;
    assert_eq!(u.total.cost_micros, 500);
    assert_eq!(u.total.unpriced_samples, 1);
    assert_eq!(u.by_ticket[&id].input_tokens, 20);
    assert_eq!(u.by_employee["dev"].turns_total, 1);
    assert_eq!(u.by_employee["dev"].sampled_turns, 2);
    assert_eq!(u.by_day["1970-01-01"].turns, 2);
    assert_eq!(u.context["dev"].size, 50);

    apply(
        &mut e,
        Actor::System,
        EventKind::AgentStopped {
            employee: "dev".into(),
        },
    );
    assert!(e.state.usage.context.is_empty());
}

#[test]
fn old_events_and_state_without_usage_deserialize() {
    let event: Event = serde_json::from_str(
        r#"{"seq":1,"ts":"2026-10-24T10:00:00Z","actor":{"kind":"system"},"type":"usage_recorded","employee":"dev"}"#,
    )
    .expect("minimal usage event parses");
    match event.kind {
        EventKind::UsageRecorded {
            currency,
            input_tokens,
            ..
        } => {
            assert_eq!(currency, "USD");
            assert_eq!(input_tokens, 0);
        }
        other => panic!("unexpected {other:?}"),
    }
    let (e, _) = ready_ticket();
    let mut json = serde_json::to_value(&e.state).expect("serializes");
    json.as_object_mut().expect("object").remove("usage");
    let state: State = serde_json::from_value(json).expect("state without usage loads");
    assert_eq!(state.usage, Default::default());
}

#[test]
fn usage_events_carry_a_summary_and_a_name() {
    let kind = usage("dev", "T-1", None);
    assert_eq!(kind.name(), "usage_recorded");
    assert_eq!(kind.summary(), "dev used 10 in / 2 out");
}

mod derivations {
    use chrono::{NaiveDate, TimeZone, Utc};

    use crate::usage::{
        ContextFill, EmployeeUsage, UsageHealth, UsageInput, UsageStats, UsageTotals, view,
    };

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").expect("date")
    }

    fn input<'a>(ticket: Option<&'a str>, tokens: u64, cost: Option<u64>) -> UsageInput<'a> {
        UsageInput {
            employee: "dev",
            ticket,
            session: "s",
            model: Some("m"),
            input_tokens: tokens,
            output_tokens: 0,
            cached_tokens: 0,
            cache_write_tokens: 0,
            cost_micros: cost,
            currency: "USD",
            estimated: false,
            context_used: None,
            context_size: None,
        }
    }

    fn rec(s: &mut UsageStats, d: &str, u: &UsageInput<'_>) {
        let at = Utc.from_utc_datetime(&day(d).and_hms_opt(12, 0, 0).expect("time"));
        s.record(1, at, u);
    }

    #[test]
    fn daily_series_zero_fills_across_a_month_boundary() {
        let mut s = UsageStats::default();
        rec(&mut s, "2026-09-30", &input(None, 7, None));
        let series = s.daily_series(3, day("2026-10-01"));
        let dates: Vec<_> = series.iter().map(|d| d.date.as_str()).collect();
        assert_eq!(dates, ["2026-09-29", "2026-09-30", "2026-10-01"]);
        assert_eq!(series[0].tokens_total(), 0);
        assert_eq!(series[1].input_tokens, 7);
        assert_eq!(
            UsageStats::default()
                .daily_series(14, day("2026-10-01"))
                .len(),
            14
        );
    }

    #[test]
    fn window_excludes_the_eighth_day() {
        let mut s = UsageStats::default();
        rec(&mut s, "2026-10-25", &input(None, 1, None));
        rec(&mut s, "2026-10-19", &input(None, 10, None));
        rec(&mut s, "2026-10-18", &input(None, 100, None));
        assert_eq!(s.window(7, day("2026-10-25")).input_tokens, 11);
        assert_eq!(s.window(1, day("2026-10-25")).input_tokens, 1);
        assert_eq!(s.window(30, day("2026-10-25")).input_tokens, 111);
    }

    #[test]
    fn top_tickets_orders_by_cost_then_tokens_then_id() {
        let mut s = UsageStats::default();
        rec(&mut s, "2026-10-25", &input(Some("T-3"), 5, Some(10)));
        rec(&mut s, "2026-10-25", &input(Some("T-2"), 5, Some(10)));
        rec(&mut s, "2026-10-25", &input(Some("T-1"), 50, Some(1)));
        rec(&mut s, "2026-10-25", &input(Some("T-9"), 500, Some(10)));
        let top: Vec<_> = s
            .top_tickets(3)
            .iter()
            .map(|(id, _)| id.to_string())
            .collect();
        assert_eq!(top, ["T-9", "T-2", "T-3"]);
    }

    #[test]
    fn health_states() {
        let mut e = EmployeeUsage::default();
        assert_eq!(e.health(), UsageHealth::Idle);
        e.turns_total = 3;
        assert_eq!(e.health(), UsageHealth::ReportsNothing);
        e.sampled_turns = 1;
        e.totals.input_tokens = 10;
        e.totals.unpriced_samples = 1;
        assert_eq!(e.health(), UsageHealth::NoCost);
        e.totals.estimated_cost_micros = 5;
        assert_eq!(e.health(), UsageHealth::Ok);
    }

    #[test]
    fn context_percent_is_clamped_and_safe() {
        let fill = |used, size| ContextFill {
            used,
            size,
            session: "s".into(),
            ticket: None,
            at: Utc::now(),
        };
        assert_eq!(fill(5, 0).percent(), 0);
        assert_eq!(fill(122, 200).percent(), 61);
        assert_eq!(fill(900, 200).percent(), 100);
    }

    #[test]
    fn mixed_reported_and_estimated_is_flagged_and_delta_saturates() {
        let mut s = UsageStats::default();
        rec(&mut s, "2026-10-25", &input(None, 10, Some(100)));
        let mut est = input(None, 10, Some(50));
        est.estimated = true;
        rec(&mut s, "2026-10-25", &est);
        assert!(s.total.is_estimated());
        assert!(s.total.has_cost());
        assert_eq!(s.total.total_cost_micros(), 150);
        let base = s.total.clone();
        rec(&mut s, "2026-10-25", &input(None, 4, Some(7)));
        let d = s.total.delta_since(&base);
        assert_eq!((d.input_tokens, d.cost_micros, d.turns), (4, 7, 1));
        let back = base.delta_since(&s.total);
        assert_eq!(back, UsageTotals::default());
    }

    #[test]
    fn view_joins_names_titles_and_pads_days() {
        let (mut e, id) = crate::dev_loop_tests::ready_ticket();
        let mut u = input(Some(&id), 10, Some(5));
        u.employee = "dev";
        rec_into(&mut e.state.usage, &u);
        let now = Utc
            .with_ymd_and_hms(2026, 10, 25, 9, 0, 0)
            .single()
            .expect("t");
        let v = view(&e.state, now, 5, Some(10));
        assert_eq!(v.daily.len(), 5);
        assert_eq!(v.tickets.len(), 1);
        assert!(v.tickets[0].title.is_some());
        assert_eq!(v.cost.total_micros, 5);
        assert_eq!(v.recent.len(), 1);
    }

    fn rec_into(s: &mut UsageStats, u: &UsageInput<'_>) {
        s.record(
            1,
            Utc.with_ymd_and_hms(2026, 10, 25, 1, 0, 0)
                .single()
                .expect("t"),
            u,
        );
    }
}
