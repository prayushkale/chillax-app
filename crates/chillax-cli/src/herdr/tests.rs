use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chillax_core::config::{HerdrConfig, HerdrMode};
use chillax_core::engine::Engine;
use chillax_core::role::{Role, RoleCatalog};
use chillax_core::workflow::Workflow;
use chillax_core::{
    Actor, AgentState, CompanyHandle, EventKind, Hub, Level, MemoryLog, Notification,
};

use super::reporter::{HerdrNotifier, Throttle};
use super::*;

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}

fn herdr(client: Arc<RecordingHerdr>) -> Herdr {
    Herdr::new(client, "p_1".into(), "Acme Co").without_backoff()
}

fn note(level: Level, key: &str) -> Notification {
    Notification {
        title: "Chillax needs you: Dana".into(),
        body: "Run rm".into(),
        level,
        key: key.into(),
    }
}

#[test]
fn report_agent_arguments_are_exact() {
    let h = herdr(Arc::default());
    assert_eq!(
        h.report_agent_args(AgentState::Working, 7),
        strings(&[
            "pane",
            "report-agent",
            "p_1",
            "--source",
            "chillax:acme-co",
            "--agent",
            "chillax",
            "--state",
            "working",
            "--seq",
            "7",
            "--agent-session-id",
            "chillax:acme-co",
        ])
    );
    assert!(
        !h.report_agent_args(AgentState::Idle, 1)
            .contains(&"--".to_string())
    );
    let blocked = h.report_agent_args(AgentState::Blocked, 8);
    assert!(blocked.windows(2).any(|w| w == ["--state", "blocked"]));
    assert!(
        blocked
            .windows(2)
            .any(|w| w == ["--message", "Waiting for you in Chillax"])
    );
}

#[test]
fn release_and_metadata_arguments_are_exact() {
    let h = herdr(Arc::default());
    assert_eq!(
        h.release_agent_args(9),
        strings(&[
            "pane",
            "release-agent",
            "p_1",
            "--source",
            "chillax:acme-co",
            "--agent",
            "chillax",
            "--seq",
            "9",
        ])
    );
    let tokens = BTreeMap::from([("inbox", Some("2".to_string())), ("qa", None)]);
    assert_eq!(
        h.report_metadata_args(&tokens, 10),
        strings(&[
            "pane",
            "report-metadata",
            "p_1",
            "--source",
            "chillax:acme-co",
            "--agent",
            "chillax",
            "--token",
            "inbox=2",
            "--clear-token",
            "qa",
            "--seq",
            "10",
        ])
    );
}

#[test]
fn notification_arguments_are_exact_and_safe() {
    assert_eq!(
        Herdr::notification_args(&note(Level::Attention, "k")),
        strings(&[
            "notification",
            "show",
            "Chillax needs you: Dana",
            "--body",
            "Run rm",
            "--sound",
            "request",
        ])
    );
    let mut info = note(Level::Info, "k");
    info.body = String::new();
    info.title = "-rf\nboom".into();
    assert_eq!(
        Herdr::notification_args(&info),
        strings(&["notification", "show", " -rf boom"])
    );
}

#[test]
fn seq_rises_with_every_call() {
    let h = herdr(Arc::default());
    let (a, b) = (h.next_seq(), h.next_seq());
    assert!(b > a);
}

#[test]
fn detection_needs_env_pane_and_an_existing_binary() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bin = dir.path().join("herdr");
    std::fs::write(&bin, "").expect("write");
    let env = |pairs: &[(&str, &str)]| {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect::<BTreeMap<_, _>>()
    };
    let bin_str = bin.to_str().expect("utf8");
    let full = env(&[
        ("HERDR_ENV", "1"),
        ("HERDR_PANE_ID", "p_3"),
        ("HERDR_BIN_PATH", bin_str),
    ]);
    assert_eq!(
        HerdrEnv::detect(&full),
        Some(HerdrEnv {
            bin: bin.clone(),
            pane: "p_3".into()
        })
    );
    assert!(HerdrEnv::detect(&env(&[])).is_none());
    assert!(
        HerdrEnv::detect(&env(&[
            ("HERDR_ENV", "0"),
            ("HERDR_PANE_ID", "p_3"),
            ("HERDR_BIN_PATH", bin_str)
        ]))
        .is_none()
    );
    assert!(HerdrEnv::detect(&env(&[("HERDR_ENV", "1"), ("HERDR_BIN_PATH", bin_str)])).is_none());
    assert!(
        HerdrEnv::detect(&env(&[
            ("HERDR_ENV", "1"),
            ("HERDR_PANE_ID", "p_3"),
            ("HERDR_BIN_PATH", "/no/such/herdr")
        ]))
        .is_none()
    );
    assert!(activate(HerdrMode::Off, &full).is_none());
    assert!(activate(HerdrMode::Auto, &full).is_some());
    assert!(activate(HerdrMode::On, &full).is_some());
    assert!(activate(HerdrMode::Auto, &env(&[])).is_none());
    assert!(activate(HerdrMode::On, &env(&[])).is_none());
}

#[tokio::test]
async fn doctor_is_silent_without_herdr() {
    assert!(doctor(HerdrMode::Auto, &BTreeMap::new()).await.is_empty());
    assert!(doctor(HerdrMode::Off, &BTreeMap::new()).await.is_empty());
    let on = doctor(HerdrMode::On, &BTreeMap::new()).await;
    assert_eq!(on.len(), 1);
    assert!(on[0].starts_with('!'));
}

#[tokio::test]
async fn disables_itself_after_five_consecutive_failures() {
    let client = Arc::new(RecordingHerdr::default());
    client.set_failing(true);
    let h = herdr(client.clone());
    for _ in 0..5 {
        assert!(!h.report_agent(AgentState::Idle).await);
    }
    assert!(h.is_disabled());
    assert!(!h.report_agent(AgentState::Idle).await);
    assert_eq!(client.calls().len(), 5, "no calls after giving up");
}

#[tokio::test]
async fn release_runs_even_in_backoff_or_when_disabled() {
    let client = Arc::new(RecordingHerdr::default());
    client.set_failing(true);
    let h = Herdr::new(client.clone(), "p_1".into(), "Acme Co");
    assert!(!h.report_agent(AgentState::Idle).await);
    assert!(
        !h.report_agent(AgentState::Idle).await,
        "skipped in backoff"
    );
    assert_eq!(client.calls().len(), 1);
    client.set_failing(false);
    assert!(h.release().await);
    assert_eq!(client.calls().len(), 2);
    assert_eq!(client.calls()[1][1], "release-agent");
}

#[tokio::test]
async fn a_success_resets_the_failure_count() {
    let client = Arc::new(RecordingHerdr::default());
    let h = herdr(client.clone());
    client.set_failing(true);
    for _ in 0..4 {
        h.report_agent(AgentState::Idle).await;
    }
    client.set_failing(false);
    assert!(h.report_agent(AgentState::Idle).await);
    client.set_failing(true);
    for _ in 0..4 {
        h.report_agent(AgentState::Idle).await;
    }
    assert!(!h.is_disabled());
}

#[test]
fn throttle_dedupes_a_key_for_a_minute_and_caps_bursts() {
    let t0 = Instant::now();
    let mut t = Throttle::default();
    assert!(t.allow("inbox:1", t0));
    assert!(!t.allow("inbox:1", t0 + Duration::from_secs(59)));
    assert!(t.allow("inbox:1", t0 + Duration::from_secs(61)));

    let mut t = Throttle::default();
    for i in 0..5 {
        assert!(t.allow(&format!("k{i}"), t0));
    }
    assert!(!t.allow("k5", t0 + Duration::from_secs(9)));
    assert!(t.allow("k5", t0 + Duration::from_secs(10)));
}

#[tokio::test]
async fn the_notifier_drops_repeats() {
    let client = Arc::new(RecordingHerdr::default());
    let notifier = HerdrNotifier::new(Arc::new(herdr(client.clone())));
    use chillax_core::Notifier;
    notifier.notify(note(Level::Attention, "inbox:1")).await;
    notifier.notify(note(Level::Attention, "inbox:1")).await;
    assert_eq!(client.calls().len(), 1);
}

async fn hub() -> Arc<Hub> {
    let pm = Role::parse("---\nid: pm\ntitle: PM\n---\nPM\n").expect("valid role");
    let engine = Engine::new("Acme Co", RoleCatalog::new([pm]), Workflow::default());
    let hub = Hub::open(engine, Arc::new(MemoryLog::default()))
        .await
        .expect("opens");
    hub.send(
        Actor::Stakeholder,
        chillax_core::Command::HireEmployee {
            role: "pm".into(),
            name: Some("Pat".into()),
            backend: None,
            model: None,
            effort: None,
            keep_defaults: false,
        },
    )
    .await
    .expect("hires");
    Arc::new(hub)
}

fn spawn_for(hub: &Arc<Hub>, client: Arc<RecordingHerdr>, notifications: bool) -> HerdrHandle {
    let config = HerdrConfig {
        enabled: HerdrMode::On,
        notifications,
        metadata: true,
    };
    let company: Arc<dyn CompanyHandle> = hub.clone();
    let mut handle = spawn(client, "p_1".into(), "Acme Co", &config, company);
    // Tests do not wait 300 ms per burst.
    handle.task.abort();
    let herdr = handle.herdr.clone();
    let options = reporter::Options {
        metadata: true,
        notifications,
        debounce: Duration::from_millis(20),
        hires_interval: HIRES_INTERVAL,
        hires_refresh: Duration::from_secs(40),
    };
    let company: Arc<dyn CompanyHandle> = hub.clone();
    let events = company.subscribe();
    handle.task = tokio::spawn(reporter::run(company, herdr, events, options));
    handle
}

const HIRES_INTERVAL: Duration = Duration::from_millis(400);

async fn settle() {
    tokio::time::sleep(Duration::from_millis(200)).await;
}

fn verbs(client: &RecordingHerdr) -> Vec<String> {
    client
        .calls()
        .iter()
        .map(|c| format!("{} {}", c[0], c[1]))
        .collect()
}

fn ask(id: &str) -> EventKind {
    EventKind::QuestionAsked {
        id: id.into(),
        employee: "pat".into(),
        question: "Which db?".into(),
    }
}

#[tokio::test]
async fn reports_once_then_only_on_change_and_debounces_bursts() {
    let hub = hub().await;
    let client = Arc::new(RecordingHerdr::default());
    let handle = spawn_for(&hub, client.clone(), true);
    settle().await;
    assert_eq!(
        verbs(&client),
        [
            "pane report-agent",
            "pane report-metadata",
            "pane report-metadata"
        ],
        "initial report: state, tokens, then the hires"
    );
    let first = client.calls()[0].clone();
    assert!(first.windows(2).any(|w| w == ["--state", "idle"]));

    // A new hire shows up in the hires metadata only: no state or token report.
    hub.send(
        Actor::Stakeholder,
        chillax_core::Command::HireEmployee {
            role: "pm".into(),
            name: Some("Quinn".into()),
            backend: None,
            model: None,
            effort: None,
            keep_defaults: false,
        },
    )
    .await
    .expect("hires");
    tokio::time::sleep(HIRES_INTERVAL * 2).await;
    let alerts = client.calls().len();
    assert_eq!(alerts, 4, "{:?}", client.calls());

    // A burst of two questions becomes one blocked report and one metadata report.
    hub.record(Actor::System, ask("i1")).await.expect("records");
    hub.record(Actor::System, ask("i2")).await.expect("records");
    settle().await;
    let calls = client.calls();
    let after = &calls[alerts..];
    let reports: Vec<_> = after.iter().filter(|c| c[1] == "report-agent").collect();
    assert_eq!(reports.len(), 1, "{after:?}");
    assert!(reports[0].windows(2).any(|w| w == ["--state", "blocked"]));
    let meta: Vec<_> = after
        .iter()
        .filter(|c| c[1] == "report-metadata" && c.contains(&"--token".to_string()))
        .filter(|c| c.iter().any(|a| a.starts_with("inbox=")))
        .collect();
    assert_eq!(meta.len(), 1);
    assert!(meta[0].contains(&"inbox=2".to_string()));
    assert_eq!(
        after.iter().filter(|c| c[0] == "notification").count(),
        2,
        "one per question"
    );
    handle.shutdown().await;
}

#[tokio::test]
async fn replayed_history_never_notifies_and_notifications_can_be_off() {
    let hub = hub().await;
    hub.record(Actor::System, ask("old"))
        .await
        .expect("records");
    let client = Arc::new(RecordingHerdr::default());
    let handle = spawn_for(&hub, client.clone(), false);
    settle().await;
    hub.record(Actor::System, ask("new"))
        .await
        .expect("records");
    settle().await;
    assert!(client.calls().iter().all(|c| c[0] != "notification"));
    handle.shutdown().await;
}

#[tokio::test]
async fn shutdown_stops_reporting_and_releases() {
    let hub = hub().await;
    let client = Arc::new(RecordingHerdr::default());
    let handle = spawn_for(&hub, client.clone(), true);
    settle().await;
    handle.shutdown().await;
    let before = client.calls().len();
    assert_eq!(client.calls().last().expect("calls")[1], "release-agent");
    hub.record(Actor::System, ask("late"))
        .await
        .expect("records");
    settle().await;
    assert_eq!(client.calls().len(), before, "aborted task stays quiet");
}

fn hires_calls(client: &RecordingHerdr) -> Vec<Vec<String>> {
    client
        .calls()
        .into_iter()
        .filter(|c| {
            c[1] == "report-metadata"
                && c.iter()
                    .any(|a| a == "--display-agent" || a == "--clear-display-agent")
        })
        .collect()
}

fn has(call: &[String], flag: &str, value: &str) -> bool {
    call.windows(2).any(|w| w[0] == flag && w[1] == value)
}

#[test]
fn hires_arguments_are_exact_and_clearing_is_complete() {
    let h = herdr(Arc::default());
    let meta = HiresMeta {
        hires: "Dana working T-4 · Sam idle".into(),
        active: 1,
        labels: vec![("working", "Dana T-4".into()), ("idle", "-Sam idle".into())],
        display_agent: "chillax (1 working)".into(),
    };
    assert_eq!(
        h.report_hires_args(&meta, 3),
        strings(&[
            "pane",
            "report-metadata",
            "p_1",
            "--source",
            "chillax:acme-co",
            "--agent",
            "chillax",
            "--token",
            "hires=Dana working T-4 · Sam idle",
            "--token",
            "active=1",
            "--state-label",
            "working=Dana T-4",
            "--state-label",
            "idle= -Sam idle",
            "--display-agent",
            "chillax (1 working)",
            "--ttl-ms",
            "60000",
            "--seq",
            "3",
        ])
    );
    let clear = h.clear_hires_args(false, 4);
    for flag in ["--clear-state-labels", "--clear-display-agent"] {
        assert!(clear.contains(&flag.to_string()), "{flag}");
    }
    assert!(has(&clear, "--clear-token", "hires"));
    assert!(has(&clear, "--clear-token", "active"));
}

#[tokio::test]
async fn hires_metadata_is_sent_once_and_follows_changes() {
    let hub = hub().await;
    let client = Arc::new(RecordingHerdr::default());
    let handle = spawn_for(&hub, client.clone(), false);
    settle().await;
    let first = hires_calls(&client);
    assert_eq!(first.len(), 1);
    assert!(
        has(&first[0], "--token", "hires=Pat offline")
            || has(&first[0], "--token", "hires=Pat idle")
    );
    assert!(has(&first[0], "--token", "active=0"));
    assert!(has(&first[0], "--display-agent", "chillax"));
    assert!(
        first[0]
            .iter()
            .any(|a| a.starts_with("idle=") || a.starts_with("--state-label"))
    );
    // An unrelated event changes nothing visible: nothing more is sent.
    let total = client.calls().len();
    hub.record(Actor::System, EventKind::DispatchPaused)
        .await
        .expect("records");
    settle().await;
    assert_eq!(client.calls().len(), total);
    handle.shutdown().await;
}

#[tokio::test]
async fn a_burst_of_events_sends_the_hires_at_most_twice() {
    let hub = hub().await;
    let client = Arc::new(RecordingHerdr::default());
    let handle = spawn_for(&hub, client.clone(), false);
    settle().await;
    let before = hires_calls(&client).len();
    for n in 0..50 {
        hub.record(Actor::System, ask(&format!("q{n}")))
            .await
            .expect("records");
        hub.send(
            Actor::Stakeholder,
            chillax_core::Command::HireEmployee {
                role: "pm".into(),
                name: Some(format!("Hire{n}")),
                backend: None,
                model: None,
                effort: None,
                keep_defaults: false,
            },
        )
        .await
        .ok();
    }
    tokio::time::sleep(HIRES_INTERVAL * 3).await;
    let sent = hires_calls(&client).len() - before;
    assert!((1..=2).contains(&sent), "{sent} hires calls");
    // The deferred last value did arrive.
    let last = hires_calls(&client).pop().expect("a call");
    assert!(last.iter().any(|a| a.starts_with("hires=")));
    handle.shutdown().await;
}

#[tokio::test]
async fn a_let_go_hire_drops_out_and_nobody_left_clears_everything() {
    let hub = hub().await;
    let client = Arc::new(RecordingHerdr::default());
    let handle = spawn_for(&hub, client.clone(), false);
    settle().await;
    hub.send(
        Actor::Stakeholder,
        chillax_core::Command::LetGo {
            employee: "pat".into(),
        },
    )
    .await
    .expect("lets go");
    tokio::time::sleep(HIRES_INTERVAL * 2).await;
    let last = hires_calls(&client).pop().expect("a call");
    assert!(
        last.contains(&"--clear-display-agent".to_string()),
        "{last:?}"
    );
    assert!(has(&last, "--clear-token", "hires"));
    handle.shutdown().await;
}

#[tokio::test]
async fn shutdown_clears_the_hires_before_releasing() {
    let hub = hub().await;
    let client = Arc::new(RecordingHerdr::default());
    let handle = spawn_for(&hub, client.clone(), false);
    settle().await;
    handle.shutdown().await;
    let calls = client.calls();
    let n = calls.len();
    assert_eq!(calls[n - 1][1], "release-agent");
    assert!(calls[n - 2].contains(&"--clear-display-agent".to_string()));
}

#[tokio::test]
async fn failing_hires_calls_back_off_like_the_rest() {
    let client = Arc::new(RecordingHerdr::default());
    client.set_failing(true);
    let h = Herdr::new(client.clone(), "p_1".into(), "Acme Co");
    let meta = HiresMeta {
        hires: "Pat idle".into(),
        active: 0,
        labels: vec![],
        display_agent: "chillax".into(),
    };
    assert!(!h.report_hires(&meta).await);
    assert!(!h.report_hires(&meta).await, "skipped in backoff");
    assert_eq!(client.calls().len(), 1);
    assert!(!h.release_hires().await, "release bypasses backoff");
    assert_eq!(client.calls().len(), 2);
}

mod open_verbs {
    use std::path::{Path, PathBuf};

    use chillax_core::config::AppConfig;
    use chillax_core::engine::{EmployeeView, State};
    use chillax_core::model::{Employee, EmployeeStatus, Ticket};

    use super::super::open::{self, Mode, Place, Plan};
    use super::*;

    fn spec(cwd: &str) -> NewPane {
        NewPane {
            cwd: Some(PathBuf::from(cwd)),
            env: vec![("A".into(), "b".into())],
            focus: Some(false),
        }
    }

    #[test]
    fn workspace_create_arguments_are_exact() {
        assert_eq!(
            Herdr::workspace_create_args(Some("Acme"), &spec("/r")),
            strings(&[
                "workspace",
                "create",
                "--cwd",
                "/r",
                "--label",
                "Acme",
                "--env",
                "A=b",
                "--no-focus"
            ])
        );
        assert_eq!(
            Herdr::workspace_create_args(None, &NewPane::default()),
            strings(&["workspace", "create"])
        );
    }

    #[test]
    fn pane_split_arguments_are_exact() {
        let mut s = spec("/r");
        s.focus = Some(true);
        assert_eq!(
            Herdr::pane_split_args(&SplitTarget::Current, Direction::Right, Some(0.4), &s),
            strings(&[
                "pane",
                "split",
                "--current",
                "--direction",
                "right",
                "--ratio",
                "0.4",
                "--cwd",
                "/r",
                "--env",
                "A=b",
                "--focus"
            ])
        );
        assert_eq!(
            Herdr::pane_split_args(
                &SplitTarget::Pane("p_9".into()),
                Direction::Down,
                None,
                &NewPane::default()
            ),
            strings(&["pane", "split", "--pane", "p_9", "--direction", "down"])
        );
    }

    #[test]
    fn pane_run_and_tab_arguments_are_exact() {
        assert_eq!(
            Herdr::pane_run_args("p_2", "chillax transcript pat --follow"),
            strings(&["pane", "run", "p_2", "chillax transcript pat --follow"])
        );
        assert_eq!(
            Herdr::tab_create_args(Some("Pat"), &NewPane::default()),
            strings(&["tab", "create", "--label", "Pat"])
        );
    }

    #[test]
    fn created_panes_are_parsed_from_the_documented_paths() {
        let ws = r#"{"result":{"workspace":{"workspace_id":"w_1"},"tab":{"tab_id":"t_1"},"root_pane":{"pane_id":"p_7"}}}"#;
        let created = Herdr::parse_workspace_created(ws).expect("parsed");
        assert_eq!(created.pane, "p_7");
        assert_eq!(created.workspace.as_deref(), Some("w_1"));
        let split = r#"{"result":{"pane":{"pane_id":"p_8"}}}"#;
        assert_eq!(Herdr::parse_split(split).expect("parsed").pane, "p_8");
        assert!(Herdr::parse_split("not json").is_err());
        assert!(Herdr::parse_split("{}").is_err());
    }

    #[test]
    fn quoting_keeps_plain_words_and_protects_the_rest() {
        assert_eq!(shell_quote("pat-2"), "pat-2");
        assert_eq!(shell_quote("Pat Smith"), "'Pat Smith'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("a;rm -rf /"), "'a;rm -rf /'");
        assert_eq!(shell_quote(""), "''");
    }

    fn state_with(worktree: Option<&str>, dropped: bool) -> State {
        let mut s = State::default();
        s.employees.push(EmployeeView {
            employee: Employee {
                id: "pat".into(),
                name: "Pat".into(),
                role: "dev".into(),
                role_title: "Dev".into(),
                backend: "claude".into(),
                model: None,
                effort: None,
            },
            status: EmployeeStatus::Idle,
            last_progress: None,
            turn_open: false,
            stopped: false,
            reply_pending: false,
            pending_agent: None,
        });
        let mut t = serde_json::json!({
            "id": "T-1", "kind": "task", "title": "x", "state": "In Progress",
            "assignee": "pat"
        });
        if let Some(w) = worktree {
            t["worktree"] = w.into();
        }
        if dropped {
            t["dropped"] = "no".into();
        }
        s.tickets
            .push(serde_json::from_value::<Ticket>(t).expect("ticket"));
        s
    }

    #[test]
    fn employees_open_in_their_ticket_worktree_else_the_root() {
        let root = Path::new("/repo");
        let with = state_with(Some(".chillax/worktrees/T-1"), false);
        assert_eq!(
            open::employee_cwd(&with, "pat", root, |_| true),
            Path::new("/repo/.chillax/worktrees/T-1")
        );
        // The worktree is gone from disk, the ticket has none, or it was dropped.
        assert_eq!(open::employee_cwd(&with, "pat", root, |_| false), root);
        let none = state_with(None, false);
        assert_eq!(open::employee_cwd(&none, "pat", root, |_| true), root);
        let dropped = state_with(Some("w"), true);
        assert_eq!(open::employee_cwd(&dropped, "pat", root, |_| true), root);
        assert_eq!(open::employee_cwd(&with, "nobody", root, |_| true), root);
    }

    #[test]
    fn modes_build_their_commands() {
        let cwd = PathBuf::from("/wt");
        let app = AppConfig {
            run: Some("npm run dev -- --port {port}".into()),
            port: Some(3100),
            run_cwd: Some("web".into()),
            ..AppConfig::default()
        };
        let shell = open::plan(Mode::Shell, "pat", cwd.clone(), &app).expect("shell");
        assert_eq!(shell.command, None);
        let t = open::plan(Mode::Transcript, "pat", cwd.clone(), &app).expect("transcript");
        assert_eq!(
            t.command.as_deref(),
            Some("chillax transcript pat --follow")
        );
        let a = open::plan(Mode::App, "pat", cwd.clone(), &app).expect("app");
        assert_eq!(a.command.as_deref(), Some("npm run dev -- --port 3100"));
        assert_eq!(a.cwd, Path::new("/wt/web"));
        assert!(open::plan(Mode::App, "pat", cwd, &AppConfig::default()).is_err());
    }

    #[tokio::test]
    async fn open_splits_the_current_pane_then_runs_the_command() {
        let client = Arc::new(RecordingHerdr::default());
        client.push_reply(r#"{"result":{"pane":{"pane_id":"p_5"}}}"#);
        let h = herdr(client.clone());
        let plan = Plan {
            cwd: PathBuf::from("/wt"),
            command: Some("chillax transcript pat --follow".into()),
        };
        open::open_plan(&h, &plan, "Pat", Place::Split(Direction::Right))
            .await
            .expect("opened");
        assert_eq!(
            client.calls(),
            vec![
                strings(&[
                    "pane",
                    "split",
                    "--current",
                    "--direction",
                    "right",
                    "--cwd",
                    "/wt",
                    "--focus"
                ]),
                strings(&["pane", "run", "p_5", "chillax transcript pat --follow"]),
            ]
        );
    }

    #[tokio::test]
    async fn a_shell_open_runs_nothing_and_a_failing_herdr_is_an_error() {
        let client = Arc::new(RecordingHerdr::default());
        client.push_reply(r#"{"result":{"root_pane":{"pane_id":"p_6"}}}"#);
        let h = herdr(client.clone());
        let plan = Plan {
            cwd: PathBuf::from("/wt"),
            command: None,
        };
        open::open_plan(&h, &plan, "Pat", Place::Tab)
            .await
            .expect("opened");
        assert_eq!(client.calls().len(), 1);
        assert_eq!(client.calls()[0][..2], strings(&["tab", "create"])[..]);

        client.set_failing(true);
        assert!(open::open_plan(&h, &plan, "Pat", Place::Tab).await.is_err());
    }

    #[test]
    fn the_outside_herdr_message_says_what_to_do() {
        assert!(open::NOT_IN_HERDR.starts_with("not inside a herdr pane"));
        assert!(open::NOT_IN_HERDR.contains("chillax herdr workspace"));
    }

    #[tokio::test]
    async fn a_project_workspace_is_focused_when_it_exists_else_created() {
        let client = Arc::new(RecordingHerdr::default());
        client.push_reply(r#"{"result":{"workspaces":[{"workspace_id":"w_3","label":"Acme"}]}}"#);
        let h = herdr(client.clone());
        open::project_workspace(&h, "Acme", Path::new("/r"))
            .await
            .expect("focused");
        assert_eq!(client.calls()[1], strings(&["workspace", "focus", "w_3"]));

        let client = Arc::new(RecordingHerdr::default());
        client.push_reply(r#"{"result":{"workspaces":[]}}"#);
        client.push_reply(
            r#"{"result":{"workspace":{"workspace_id":"w_4"},"root_pane":{"pane_id":"p_1"}}}"#,
        );
        let h = herdr(client.clone());
        open::project_workspace(&h, "Acme", Path::new("/r"))
            .await
            .expect("created");
        let calls = client.calls();
        assert_eq!(calls[1][..2], strings(&["workspace", "create"])[..]);
        assert_eq!(calls[2], strings(&["pane", "run", "p_1", "chillax"]));
    }

    #[test]
    fn the_deepest_matching_project_owns_a_directory() {
        let inst = |name: &str, root: &str| chillax_core::InstanceInfo {
            root: root.into(),
            name: name.into(),
            url: format!("http://x/{name}"),
            pid: 1,
            started_at: String::new(),
        };
        let list = [inst("outer", "/p"), inst("inner", "/p/sub")];
        let found = open::instance_for_cwd(&list, Path::new("/p/sub/src")).expect("found");
        assert_eq!(found.name, "inner");
        assert!(open::instance_for_cwd(&list, Path::new("/q")).is_none());
    }
}

/// The shipped plugin manifest: only keys the herdr docs list, each action calls chillax.
#[test]
fn the_plugin_manifest_parses_and_every_action_calls_chillax() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../integrations/herdr/herdr-plugin.toml"
    );
    let text = std::fs::read_to_string(path).expect("manifest");
    let manifest: toml::Table = toml::from_str(&text).expect("valid toml");
    for key in ["id", "name", "version", "min_herdr_version"] {
        assert!(manifest[key].is_str(), "{key}");
    }
    let actions = manifest["actions"].as_array().expect("actions");
    assert!(actions.len() >= 4);
    for action in actions {
        let a = action.as_table().expect("table");
        assert!(!a["id"].as_str().expect("id").contains('.'));
        assert!(a["title"].is_str());
        let command = a["command"].as_array().expect("argv");
        let joined: Vec<&str> = command.iter().filter_map(|v| v.as_str()).collect();
        assert!(joined.join(" ").contains("chillax"), "{joined:?}");
    }
}
