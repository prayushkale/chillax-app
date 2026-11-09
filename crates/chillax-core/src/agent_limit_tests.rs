//! The live agent limit.

use crate::command::Command;
use crate::dev_loop_tests::{dev, engine, run, try_run};
use crate::engine::Engine;
use crate::event::{Actor, Event, EventKind};
use crate::model::AgentLimit;

fn set(n: usize) -> Command {
    Command::SetMaxConcurrentAgents { n }
}

fn machine(hardware_max: usize) -> Engine {
    engine().with_hardware_max(hardware_max)
}

#[test]
fn zero_and_above_hardware_are_refused_with_the_range() {
    let e = machine(4);
    for n in [0, 5] {
        let err = e.decide(&Actor::Stakeholder, set(n)).unwrap_err();
        assert!(err.to_string().contains("limit must be 1..=4"), "{err}");
    }
}

#[test]
fn one_and_the_hardware_maximum_are_accepted() {
    let mut e = machine(4);
    for n in [1, 4] {
        assert_eq!(
            run(&mut e, Actor::Stakeholder, set(n)),
            [EventKind::MaxConcurrentAgentsChanged { n, hardware_max: 4 }]
        );
        assert_eq!(
            e.state.agent_limit,
            Some(AgentLimit {
                limit: n,
                hardware_max: 4
            })
        );
    }
}

#[test]
fn setting_the_current_limit_again_is_a_no_op() {
    let mut e = machine(4);
    run(&mut e, Actor::Stakeholder, set(2));
    assert!(run(&mut e, Actor::Stakeholder, set(2)).is_empty());
}

#[test]
fn employees_and_chillax_cannot_change_it() {
    let e = machine(4);
    assert!(e.decide(&dev("dev"), set(2)).is_err());
    assert!(e.decide(&Actor::System, set(2)).is_err());
}

#[test]
fn an_engine_without_hardware_falls_back_to_the_state_and_refuses_when_unknown() {
    let mut e = engine();
    assert!(try_run(&mut e, Actor::Stakeholder, set(2)).is_err());
    let at_boot = Event {
        seq: 1,
        ts: chrono::DateTime::from_timestamp(0, 0).expect("epoch is valid"),
        actor: Actor::System,
        kind: EventKind::MaxConcurrentAgentsChanged {
            n: 3,
            hardware_max: 6,
        },
    };
    e.apply(&at_boot);
    assert!(try_run(&mut e, Actor::Stakeholder, set(7)).is_err());
    assert_eq!(run(&mut e, Actor::Stakeholder, set(6)).len(), 1);
}

#[test]
fn replay_uses_the_hardware_in_the_event_not_the_engine() {
    let event = Event {
        seq: 1,
        ts: chrono::DateTime::from_timestamp(0, 0).expect("epoch is valid"),
        actor: Actor::Stakeholder,
        kind: EventKind::MaxConcurrentAgentsChanged {
            n: 8,
            hardware_max: 16,
        },
    };
    let mut small = machine(2);
    let mut big = machine(32);
    small.apply(&event);
    big.apply(&event);
    assert_eq!(small.state.agent_limit, big.state.agent_limit);
    assert_eq!(
        small.state.agent_limit,
        Some(AgentLimit {
            limit: 8,
            hardware_max: 16
        })
    );
}

#[test]
fn a_changed_hardware_maximum_is_recorded_even_when_the_limit_is_the_same() {
    let mut e = machine(4);
    run(&mut e, Actor::Stakeholder, set(2));
    e.hardware_max = Some(8);
    assert_eq!(run(&mut e, Actor::Stakeholder, set(2)).len(), 1);
}

#[test]
fn the_command_parses_from_json() {
    let cmd: Command = serde_json::from_str(r#"{"type":"set_max_concurrent_agents","n":4}"#)
        .expect("valid command");
    assert_eq!(cmd, set(4));
}
