//! Running apps (ADR 0015): the projection and the decision of when to start, restart
//! and stop them.

use crate::apps::{AppInfo, AppStatus, AppTarget};
use crate::command::Command;
use crate::dev_loop_tests::{engine, run, try_run};
use crate::dispatch::{Action, Context, next_actions};
use crate::engine::Engine;
use crate::event::{Actor, EventKind};

fn configured() -> Engine {
    let mut e = engine();
    let apps = ["dev", "qa", "prod"]
        .iter()
        .enumerate()
        .map(|(i, env)| AppInfo {
            env: env.to_string(),
            port: 3000 + i as u16,
            url: format!("http://localhost:{}", 3000 + i),
        })
        .collect();
    run(&mut e, Actor::System, Command::ConfigureApps { apps });
    e
}

fn system(e: &mut Engine, cmd: Command) {
    run(e, Actor::System, cmd);
}

fn started(e: &mut Engine, env: &str, commit: &str) {
    system(
        e,
        Command::RecordAppStarted {
            env: env.into(),
            port: 3000,
            url: "http://localhost:3000".into(),
            commit: commit.into(),
        },
    );
}

fn ctx(targets: &[(&str, AppTarget)], live: &[(&str, &str)]) -> Context {
    Context {
        max_concurrent: 3,
        app_targets: targets
            .iter()
            .map(|(e, t)| (e.to_string(), t.clone()))
            .collect(),
        app_live: live
            .iter()
            .map(|(e, c)| (e.to_string(), c.to_string()))
            .collect(),
        ..Context::default()
    }
}

fn commit(c: &str) -> AppTarget {
    AppTarget::Commit(c.into())
}

fn app_actions(e: &Engine, ctx: &Context) -> Vec<Action> {
    next_actions(e, ctx)
        .into_iter()
        .filter(|a| matches!(a, Action::RunApp { .. } | Action::StopApp { .. }))
        .collect()
}

fn run_app(env: &str, c: &str, why: &str) -> Action {
    Action::RunApp {
        env: env.into(),
        commit: c.into(),
        why: why.into(),
    }
}

#[test]
fn configuring_apps_builds_the_list_and_is_idempotent() {
    let mut e = configured();
    assert_eq!(e.state.apps.len(), 3);
    assert_eq!(e.state.apps[0].env, "dev");
    assert_eq!(e.state.apps[0].status, AppStatus::Stopped);
    assert!(e.state.apps[0].wanted);
    let again: Vec<AppInfo> = e
        .state
        .apps
        .iter()
        .map(|a| AppInfo {
            env: a.env.clone(),
            port: a.port,
            url: a.url.clone(),
        })
        .collect();
    let kinds = run(
        &mut e,
        Actor::System,
        Command::ConfigureApps { apps: again },
    );
    assert!(kinds.is_empty());
    // Dropping an app from the config removes it; the others keep their status.
    started(&mut e, "dev", "aaaaaaa1");
    system(
        &mut e,
        Command::ConfigureApps {
            apps: vec![AppInfo {
                env: "dev".into(),
                port: 3005,
                url: "http://localhost:3005".into(),
            }],
        },
    );
    assert_eq!(e.state.apps.len(), 1);
    assert_eq!(e.state.apps[0].status, AppStatus::Running);
    assert_eq!(e.state.apps[0].port, 3005);
}

#[test]
fn events_project_into_the_app_status() {
    let mut e = configured();
    started(&mut e, "dev", "abc1234567");
    let app = e.state.app("dev").expect("dev");
    assert_eq!(app.status, AppStatus::Running);
    assert_eq!(app.commit.as_deref(), Some("abc1234567"));
    assert!(app.since.is_some());

    system(
        &mut e,
        Command::RecordAppCrashed {
            env: "dev".into(),
            exit: Some(1),
            tail: "boom".into(),
            will_retry: true,
            commit: "abc1234567".into(),
        },
    );
    let app = e.state.app("dev").expect("dev");
    assert_eq!(app.status, AppStatus::Starting);
    assert_eq!(app.reason.as_deref(), Some("exited with 1"));

    system(
        &mut e,
        Command::RecordAppRestarted {
            env: "dev".into(),
            reason: "crash retry 1".into(),
            attempt: 1,
        },
    );
    assert_eq!(e.state.app("dev").expect("dev").restarts, 1);

    system(
        &mut e,
        Command::RecordAppCrashed {
            env: "dev".into(),
            exit: None,
            tail: String::new(),
            will_retry: false,
            commit: "abc1234567".into(),
        },
    );
    assert_eq!(e.state.app("dev").expect("dev").status, AppStatus::Crashed);

    system(
        &mut e,
        Command::RecordAppStopped {
            env: "prod".into(),
            reason: "no release yet".into(),
        },
    );
    let prod = e.state.app("prod").expect("prod");
    assert_eq!(prod.status, AppStatus::Stopped);
    assert_eq!(prod.reason.as_deref(), Some("no release yet"));
}

#[test]
fn the_stakeholder_controls_apps_and_only_when_it_makes_sense() {
    let mut e = configured();
    let ask = |e: &mut Engine, cmd| try_run(e, Actor::Stakeholder, cmd);
    // Nothing runs yet: stop and restart are refused, start too (it is simply waiting).
    assert!(ask(&mut e, Command::StopApp { env: "dev".into() }).is_err());
    assert!(ask(&mut e, Command::RestartApp { env: "dev".into() }).is_err());
    assert!(ask(&mut e, Command::StartApp { env: "dev".into() }).is_err());
    assert!(
        ask(
            &mut e,
            Command::StartApp {
                env: "staging".into()
            }
        )
        .is_err()
    );
    started(&mut e, "dev", "a1");
    assert!(ask(&mut e, Command::StartApp { env: "dev".into() }).is_err());
    ask(&mut e, Command::StopApp { env: "dev".into() }).expect("stop");
    let app = e.state.app("dev").expect("dev");
    assert!(!app.wanted);
    assert!(app.can_start());
    ask(&mut e, Command::StartApp { env: "dev".into() }).expect("start");
    let app = e.state.app("dev").expect("dev");
    assert!(app.wanted && app.restart);
    assert_eq!(app.status, AppStatus::Starting);
    // Agents and Chillax cannot use the stakeholder's controls, and the stakeholder
    // cannot record what only Chillax knows.
    assert!(
        try_run(
            &mut e,
            Actor::Employee("x".into()),
            Command::RestartApp { env: "dev".into() }
        )
        .is_err()
    );
    assert!(
        try_run(
            &mut e,
            Actor::System,
            Command::StopApp { env: "dev".into() }
        )
        .is_err()
    );
    assert!(
        try_run(
            &mut e,
            Actor::Stakeholder,
            Command::RecordAppStarted {
                env: "dev".into(),
                port: 1,
                url: String::new(),
                commit: String::new()
            }
        )
        .is_err()
    );
}

#[test]
fn an_app_with_code_to_run_is_started_then_left_alone() {
    let mut e = configured();
    let targets = [("dev", commit("a1"))];
    assert_eq!(
        app_actions(&e, &ctx(&targets, &[])),
        vec![run_app("dev", "a1", "dev at a1")]
    );
    started(&mut e, "dev", "a1");
    assert!(app_actions(&e, &ctx(&targets, &[("dev", "a1")])).is_empty());
}

#[test]
fn the_app_restarts_when_the_code_moves() {
    let mut e = configured();
    started(&mut e, "dev", "1111111aaa");
    let actions = app_actions(
        &e,
        &ctx(&[("dev", commit("2222222bbb"))], &[("dev", "1111111aaa")]),
    );
    assert_eq!(
        actions,
        vec![run_app(
            "dev",
            "2222222bbb",
            "dev moved from 1111111 to 2222222"
        )]
    );
}

#[test]
fn an_app_the_log_says_runs_but_has_no_process_is_started_again() {
    let mut e = configured();
    started(&mut e, "qa", "q1");
    // A restart of Chillax: the state was replayed, the process is gone.
    assert_eq!(
        app_actions(&e, &ctx(&[("qa", commit("q1"))], &[])),
        vec![run_app("qa", "q1", "qa at q1")]
    );
}

#[test]
fn a_restart_request_restarts_even_on_the_same_code() {
    let mut e = configured();
    started(&mut e, "dev", "a1");
    run(
        &mut e,
        Actor::Stakeholder,
        Command::RestartApp { env: "dev".into() },
    );
    assert_eq!(
        app_actions(&e, &ctx(&[("dev", commit("a1"))], &[("dev", "a1")])),
        vec![run_app("dev", "a1", "restart requested")]
    );
    // Once it started, the request is spent.
    started(&mut e, "dev", "a1");
    assert!(app_actions(&e, &ctx(&[("dev", commit("a1"))], &[("dev", "a1")])).is_empty());
}

#[test]
fn a_stopped_app_stays_stopped_until_started() {
    let mut e = configured();
    started(&mut e, "dev", "a1");
    run(
        &mut e,
        Actor::Stakeholder,
        Command::StopApp { env: "dev".into() },
    );
    let targets = [("dev", commit("a2"))];
    let stop = Action::StopApp {
        env: "dev".into(),
        reason: "stopped by you".into(),
    };
    assert_eq!(
        app_actions(&e, &ctx(&targets, &[("dev", "a1")])),
        vec![stop]
    );
    system(
        &mut e,
        Command::RecordAppStopped {
            env: "dev".into(),
            reason: "stopped by you".into(),
        },
    );
    // New code does not wake it up.
    assert!(app_actions(&e, &ctx(&targets, &[])).is_empty());
    run(
        &mut e,
        Actor::Stakeholder,
        Command::StartApp { env: "dev".into() },
    );
    assert_eq!(
        app_actions(&e, &ctx(&targets, &[])),
        vec![run_app("dev", "a2", "dev at a2")]
    );
}

#[test]
fn prod_with_no_release_is_stopped_with_a_reason_once() {
    let mut e = configured();
    let targets = [("prod", AppTarget::Idle("no release yet".into()))];
    let stop = Action::StopApp {
        env: "prod".into(),
        reason: "no release yet".into(),
    };
    assert_eq!(app_actions(&e, &ctx(&targets, &[])), vec![stop]);
    system(
        &mut e,
        Command::RecordAppStopped {
            env: "prod".into(),
            reason: "no release yet".into(),
        },
    );
    assert!(app_actions(&e, &ctx(&targets, &[])).is_empty());
    // The first release starts it.
    assert_eq!(
        app_actions(&e, &ctx(&[("prod", commit("t1"))], &[])),
        vec![run_app("prod", "t1", "prod at t1")]
    );
}

#[test]
fn a_crashed_app_is_not_retried_on_the_same_code_but_is_on_new_code() {
    let mut e = configured();
    started(&mut e, "dev", "a1");
    system(
        &mut e,
        Command::RecordAppCrashed {
            env: "dev".into(),
            exit: Some(1),
            tail: "x".into(),
            will_retry: false,
            commit: "a1".into(),
        },
    );
    assert!(app_actions(&e, &ctx(&[("dev", commit("a1"))], &[])).is_empty());
    assert_eq!(
        app_actions(&e, &ctx(&[("dev", commit("a2"))], &[])).len(),
        1
    );
    // While Chillax is still retrying (the process is live) nothing else is done.
    assert!(app_actions(&e, &ctx(&[("dev", commit("a1"))], &[("dev", "a1")])).is_empty());
    // The stakeholder's restart tries again.
    run(
        &mut e,
        Actor::Stakeholder,
        Command::RestartApp { env: "dev".into() },
    );
    assert_eq!(
        app_actions(&e, &ctx(&[("dev", commit("a1"))], &[])).len(),
        1
    );
}

#[test]
fn apps_follow_the_code_while_dispatch_is_paused_and_wait_for_their_own_action() {
    let mut e = configured();
    run(&mut e, Actor::Stakeholder, Command::PauseDispatch);
    let mut c = ctx(&[("dev", commit("a1")), ("qa", commit("q1"))], &[]);
    assert_eq!(app_actions(&e, &c).len(), 2);
    c.busy.insert("@app-dev".into());
    assert_eq!(app_actions(&e, &c), vec![run_app("qa", "q1", "qa at q1")]);
}

#[test]
fn nothing_happens_for_apps_without_a_known_target() {
    let e = configured();
    assert!(app_actions(&e, &ctx(&[], &[])).is_empty());
    let events = EventKind::AppsConfigured { apps: vec![] };
    assert_eq!(events.name(), "apps_configured");
}
