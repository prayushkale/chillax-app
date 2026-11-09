//! The dev-loop tools: request_review, submit_review, submit_qa, handoff and blockers.

use std::sync::Arc;

use chillax_core::{
    Actor, Command, CompanyHandle, Engine, Hub, MemoryLog, Role, RoleCatalog, Workflow,
};
use chillax_mcp::{ToolContext, ToolError};
use chillax_tracker::LocalTracker;
use serde_json::{Value, json};

const PM: &str = "---\nid: pm\ntitle: PM\ntags: [manager]\nmcp_tools: [create_story, create_sprint, assign, update_ticket, my_assignments]\n---\nPM\n";
const DEV: &str = "---\nid: dev\ntitle: Developer\ntags: [developer]\nowns_states: [in_progress]\nmcp_tools: [my_assignments, request_review, handoff]\n---\nDev\n";
const QA: &str = "---\nid: qa-engineer\ntitle: QA Engineer\ntags: [qa]\nowns_states: [in_qa]\nmcp_tools: [my_assignments, get_ticket, submit_qa]\n---\nQA\n";
const LEAD: &str = "---\nid: tech-lead\ntitle: Tech Lead\ntags: [reviewer]\nowns_states: [in_review]\nmcp_tools: [my_assignments, submit_review]\n---\nLead\n";

struct Setup {
    hub: Arc<Hub>,
    ctx: ToolContext,
    pm: Role,
    dev: Role,
    lead: Role,
    qa: Role,
}

async fn setup() -> Setup {
    let roles = [PM, DEV, LEAD, QA].map(|r| Role::parse(r).expect("role"));
    let hub = Arc::new(
        Hub::open(
            Engine::new("Acme", RoleCatalog::new(roles.clone()), Workflow::default()),
            Arc::new(MemoryLog::default()),
        )
        .await
        .expect("hub"),
    );
    for (role, name) in [
        ("pm", "Pat"),
        ("dev", "Dana"),
        ("tech-lead", "Lee"),
        ("qa-engineer", "Quinn"),
    ] {
        hub.send(
            Actor::Stakeholder,
            Command::HireEmployee {
                role: role.into(),
                name: Some(name.into()),
                backend: None,
                model: None,
                effort: None,
                keep_defaults: false,
            },
        )
        .await
        .expect("hired");
    }
    let company: Arc<dyn CompanyHandle> = hub.clone();
    let [pm, dev, lead, qa] = roles;
    Setup {
        ctx: ToolContext {
            tracker: Arc::new(LocalTracker::new(company.clone())),
            company,
            attachments: None,
        },
        hub,
        pm,
        dev,
        lead,
        qa,
    }
}

impl Setup {
    async fn story(&self, title: &str, extra: Value) -> Value {
        let mut args = json!({"title": title, "acceptance_criteria": ["works"]});
        args.as_object_mut()
            .expect("object")
            .extend(extra.as_object().cloned().unwrap_or_default());
        self.ctx
            .call("pat", &self.pm, "create_story", args)
            .await
            .expect("story")
    }

    /// T-1 planned, approved and assigned to Dana.
    async fn ready_ticket(&self) {
        self.story("Login", Value::Null).await;
        self.ctx
            .call(
                "pat",
                &self.pm,
                "create_sprint",
                json!({"name": "S1", "tickets": ["T-1"]}),
            )
            .await
            .expect("sprint");
        self.hub
            .send(
                Actor::Stakeholder,
                Command::ApproveSprint { id: "SP-1".into() },
            )
            .await
            .expect("approved");
        self.ctx
            .call(
                "pat",
                &self.pm,
                "assign",
                json!({"ticket": "T-1", "employee": "dana"}),
            )
            .await
            .expect("assigned");
    }

    /// T-1 built, reviewed and merged, then a QA round with the environment up.
    async fn testing_ticket(&self) {
        self.ready_ticket().await;
        self.system(Command::StartWork {
            id: "T-1".into(),
            branch: "feat/T-1".into(),
            worktree: "w/T-1".into(),
        })
        .await;
        self.system(Command::RecordTests {
            id: "T-1".into(),
            passed: true,
            skipped: false,
            output_tail: String::new(),
        })
        .await;
        self.system(Command::StartReview {
            id: "T-1".into(),
            reviewer: "lee".into(),
        })
        .await;
        self.hub
            .send(
                Actor::Employee("lee".into()),
                Command::SubmitReview {
                    id: "T-1".into(),
                    verdict: chillax_core::ReviewVerdict::Approve,
                    body: String::new(),
                },
            )
            .await
            .expect("reviewed");
        self.system(Command::RecordMerge {
            id: "T-1".into(),
            commit: "abc1234".into(),
        })
        .await;
        self.system(Command::RecordPromotion {
            commit: "def5678".into(),
        })
        .await;
        for step in [chillax_core::EnvStep::Up, chillax_core::EnvStep::Test] {
            self.system(Command::RecordEnvRun {
                step,
                ok: true,
                skipped: false,
                output_tail: String::new(),
            })
            .await;
        }
    }

    async fn system(&self, cmd: Command) {
        self.hub.send(Actor::System, cmd).await.expect("accepted");
    }

    async fn ticket(&self, id: &str) -> chillax_core::Ticket {
        self.hub
            .snapshot()
            .await
            .ticket(id)
            .expect("ticket")
            .clone()
    }
}

#[tokio::test]
async fn review_tools_carry_a_ticket_through_review() {
    let s = setup().await;
    s.ready_ticket().await;
    assert_eq!(
        s.ctx
            .call("dana", &s.dev, "my_assignments", Value::Null)
            .await,
        Ok(json!([])),
        "a planned ticket is Chillax's to start, not the developer's"
    );
    s.system(Command::StartWork {
        id: "T-1".into(),
        branch: "feat/T-1".into(),
        worktree: "w/T-1".into(),
    })
    .await;
    let mine = s
        .ctx
        .call("dana", &s.dev, "my_assignments", Value::Null)
        .await
        .expect("assignments");
    assert_eq!(mine[0]["id"], "T-1");
    assert_eq!(mine[0]["branch"], "feat/T-1");

    s.ctx
        .call(
            "dana",
            &s.dev,
            "request_review",
            json!({"id": "T-1", "summary": "login works; added tests"}),
        )
        .await
        .expect("requested");
    assert!(s.ticket("T-1").await.review_requested);

    s.system(Command::RecordTests {
        id: "T-1".into(),
        passed: true,
        skipped: false,
        output_tail: String::new(),
    })
    .await;
    s.system(Command::StartReview {
        id: "T-1".into(),
        reviewer: "lee".into(),
    })
    .await;
    let reviewing = s
        .ctx
        .call("lee", &s.lead, "my_assignments", Value::Null)
        .await
        .expect("assignments");
    assert_eq!(
        reviewing[0]["id"], "T-1",
        "a reviewer sees what they review"
    );
    assert_eq!(
        s.ctx
            .call("dana", &s.dev, "my_assignments", Value::Null)
            .await,
        Ok(json!([])),
        "and the developer waits while it is in review"
    );

    s.ctx
        .call(
            "lee",
            &s.lead,
            "submit_review",
            json!({"id": "T-1", "verdict": "approve", "body": "lgtm"}),
        )
        .await
        .expect("reviewed");
    assert!(s.ticket("T-1").await.checks.contains("review_approved"));
}

#[tokio::test]
async fn review_tools_are_limited_to_the_right_people() {
    let s = setup().await;
    s.ready_ticket().await;
    s.system(Command::StartWork {
        id: "T-1".into(),
        branch: "feat/T-1".into(),
        worktree: "w/T-1".into(),
    })
    .await;
    assert_eq!(
        s.ctx
            .call(
                "dana",
                &s.dev,
                "submit_review",
                json!({"id": "T-1", "verdict": "approve"})
            )
            .await,
        Err(ToolError::Forbidden("submit_review".into()))
    );
    // Lee is not the developer on T-1.
    let wrong = s
        .ctx
        .call(
            "lee",
            &Role::parse("---\nid: x\ntitle: X\nmcp_tools: [request_review]\n---\nX\n")
                .expect("role"),
            "request_review",
            json!({"id": "T-1", "summary": "mine"}),
        )
        .await;
    assert!(matches!(wrong, Err(ToolError::Rejected(m)) if m.contains("not assigned to you")));
    // Requested changes need feedback.
    let empty = s
        .ctx
        .call(
            "lee",
            &s.lead,
            "submit_review",
            json!({"id": "T-1", "verdict": "request_changes"}),
        )
        .await;
    assert!(matches!(empty, Err(ToolError::Rejected(_))));
}

#[tokio::test]
async fn a_tester_sees_what_is_under_test_and_signs_it_off() {
    let s = setup().await;
    s.testing_ticket().await;
    let mine = s
        .ctx
        .call("quinn", &s.qa, "my_assignments", Value::Null)
        .await
        .expect("assignments");
    assert_eq!(mine[0]["id"], "T-1", "testers see the round's tickets");
    assert_eq!(
        s.ctx
            .call("dana", &s.dev, "my_assignments", Value::Null)
            .await,
        Ok(json!([])),
        "developers do not"
    );

    s.ctx
        .call(
            "quinn",
            &s.qa,
            "submit_qa",
            json!({
                "id": "T-1",
                "verdict": "pass",
                "criteria": [{"criterion": "works", "passed": true, "evidence": "ran it"}],
                "notes": "all good",
            }),
        )
        .await
        .expect("verdict");
    let ticket = s.ticket("T-1").await;
    assert_eq!(ticket.state, "done");
    assert!(ticket.checks.contains("qa_passed"));
    assert_eq!(
        s.ctx
            .call("quinn", &s.qa, "my_assignments", Value::Null)
            .await,
        Ok(json!([])),
        "nothing is left to test"
    );
}

#[tokio::test]
async fn a_failing_verdict_files_the_bug_for_the_tester() {
    let s = setup().await;
    s.testing_ticket().await;
    s.ctx
        .call(
            "quinn",
            &s.qa,
            "submit_qa",
            json!({
                "id": "T-1",
                "verdict": "fail",
                "criteria": [{"criterion": "works", "passed": false, "evidence": "500 on login"}],
                "bug": {"title": "Login 500s", "description": "Steps, expected, actual"},
            }),
        )
        .await
        .expect("verdict");
    let bug = s.ticket("T-2").await;
    assert_eq!(bug.kind, chillax_core::TicketKind::Bug);
    assert_eq!(bug.found_in.as_deref(), Some("T-1"));
    assert_eq!(bug.assignee.as_deref(), Some("dana"));
    assert!(s.ticket("T-1").await.checks.contains("qa_failed"));
}

#[tokio::test]
async fn verdicts_are_for_testers_and_must_be_proven() {
    let s = setup().await;
    s.testing_ticket().await;
    let pass = json!({
        "id": "T-1",
        "verdict": "pass",
        "criteria": [{"criterion": "works", "passed": true, "evidence": "ran it"}],
    });
    assert_eq!(
        s.ctx.call("dana", &s.dev, "submit_qa", pass.clone()).await,
        Err(ToolError::Forbidden("submit_qa".into()))
    );
    // A bare pass is refused by the company, and the reason reaches the agent.
    let bare = s
        .ctx
        .call(
            "quinn",
            &s.qa,
            "submit_qa",
            json!({"id": "T-1", "verdict": "pass"}),
        )
        .await;
    assert!(matches!(bare, Err(ToolError::Rejected(m)) if m.contains("acceptance criteria")));
    // So is a malformed result.
    let broken = s
        .ctx
        .call(
            "quinn",
            &s.qa,
            "submit_qa",
            json!({"id": "T-1", "verdict": "maybe"}),
        )
        .await;
    assert!(matches!(broken, Err(ToolError::BadArguments(_))));
    assert_eq!(s.ticket("T-1").await.state, "in_qa");
    s.ctx
        .call("quinn", &s.qa, "submit_qa", pass)
        .await
        .expect("proven");
}

#[tokio::test]
async fn planners_set_blockers_when_writing_stories() {
    let s = setup().await;
    s.story("Auth", Value::Null).await;
    let second = s.story("Profile", json!({"blocked_by": ["T-1"]})).await;
    assert_eq!(second["id"], "T-2");
    assert_eq!(s.ticket("T-2").await.blocked_by, ["T-1"]);

    // A cycle is refused, and the list can be replaced or cleared.
    let cycle = s
        .ctx
        .call(
            "pat",
            &s.pm,
            "update_ticket",
            json!({"id": "T-1", "blocked_by": ["T-2"]}),
        )
        .await;
    assert!(matches!(cycle, Err(ToolError::Rejected(m)) if m.contains("cycle")));
    s.ctx
        .call(
            "pat",
            &s.pm,
            "update_ticket",
            json!({"id": "T-2", "blocked_by": []}),
        )
        .await
        .expect("cleared");
    assert!(s.ticket("T-2").await.blocked_by.is_empty());
}

#[tokio::test]
async fn handoff_passes_the_ticket_to_another_role() {
    let s = setup().await;
    s.ready_ticket().await;
    s.ctx
        .call(
            "dana",
            &s.dev,
            "handoff",
            json!({"id": "T-1", "role": "tech-lead", "note": "needs an architect"}),
        )
        .await
        .expect("handed off");
    assert_eq!(s.ticket("T-1").await.assignee.as_deref(), Some("lee"));
}
