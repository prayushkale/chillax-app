//! Small ACP helpers shared by employee sessions, the persona editor and the agent
//! directory: picking a model through session config options, and turning protocol
//! errors into something the stakeholder can act on.

use agent_client_protocol::schema::v1::{
    SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigSelectOptions, SessionId, SessionModeState, SetSessionConfigOptionRequest,
    SetSessionModeRequest,
};
use agent_client_protocol::{Agent, ConnectionTo, Error, ErrorCode};
use chillax_core::ModelOption;
use chillax_core::agents::{backend_title, match_model};

/// The select option in `options` with `category`, or else one of the `fallback_ids`,
/// as its config id and choices.
fn select_choices(
    options: &[SessionConfigOption],
    category: SessionConfigOptionCategory,
    fallback_ids: &[&str],
) -> Option<(String, Vec<ModelOption>)> {
    let option = options
        .iter()
        .find(|o| o.category.as_ref() == Some(&category))
        .or_else(|| {
            options
                .iter()
                .find(|o| fallback_ids.contains(&o.id.0.as_ref()))
        })?;
    let SessionConfigKind::Select(select) = &option.kind else {
        return None;
    };
    let choices = match &select.options {
        SessionConfigSelectOptions::Ungrouped(options) => options.iter().collect::<Vec<_>>(),
        SessionConfigSelectOptions::Grouped(groups) => {
            groups.iter().flat_map(|g| g.options.iter()).collect()
        }
        _ => Vec::new(),
    };
    let choices = choices
        .into_iter()
        .map(|c| ModelOption {
            id: c.value.0.to_string(),
            name: c.name.clone(),
            description: c.description.clone(),
        })
        .collect();
    Some((option.id.0.to_string(), choices))
}

/// The session's model selector and its choices, if the agent offers one.
pub(crate) fn model_options(options: &[SessionConfigOption]) -> Option<(String, Vec<ModelOption>)> {
    select_choices(options, SessionConfigOptionCategory::Model, &["model"])
}

/// The session's effort (thought level) selector and its choices, if the agent offers one.
pub(crate) fn effort_options(
    options: &[SessionConfigOption],
) -> Option<(String, Vec<ModelOption>)> {
    select_choices(
        options,
        SessionConfigOptionCategory::ThoughtLevel,
        &["effort", "thought_level", "reasoning_effort"],
    )
}

/// What switching a session's model left behind.
pub(crate) struct ModelSelection {
    /// The model id selected, or `None` when the agent kept its default.
    pub model: Option<String>,
    /// The agent's config options after the switch. An agent may rebuild its effort
    /// choices for the new model, so effort is matched against these when present.
    pub options: Option<Vec<SessionConfigOption>>,
}

/// Switch a fresh session to `wanted`. Nothing is selected when nothing was asked for
/// or the agent has no matching model (it keeps its default).
pub(crate) async fn select_model(
    cx: &ConnectionTo<Agent>,
    session: &SessionId,
    options: Option<&[SessionConfigOption]>,
    wanted: Option<&str>,
) -> Result<ModelSelection, Error> {
    let none = ModelSelection {
        model: None,
        options: None,
    };
    let Some(wanted) = wanted.filter(|w| !w.trim().is_empty()) else {
        return Ok(none);
    };
    let Some((config_id, models)) = options.and_then(model_options) else {
        tracing::warn!(
            model = wanted,
            "agent offers no model choice; using its default"
        );
        return Ok(none);
    };
    let Some(model) = match_model(&models, wanted) else {
        tracing::warn!(model = wanted, "agent has no such model; using its default");
        return Ok(none);
    };
    let response = cx
        .send_request(SetSessionConfigOptionRequest::new(
            session.clone(),
            config_id,
            model.id.as_str(),
        ))
        .block_task()
        .await?;
    Ok(ModelSelection {
        model: Some(model.id.clone()),
        options: Some(response.config_options),
    })
}

/// Set a fresh session's effort to `wanted`. Returns the effort id selected, or `None`
/// when nothing was asked for or the agent has no matching level. An unknown effort
/// never fails the session: the agent keeps its default.
pub(crate) async fn select_effort(
    cx: &ConnectionTo<Agent>,
    session: &SessionId,
    options: Option<&[SessionConfigOption]>,
    wanted: Option<&str>,
) -> Result<Option<String>, Error> {
    let Some(wanted) = wanted.filter(|w| !w.trim().is_empty()) else {
        return Ok(None);
    };
    let Some((config_id, efforts)) = options.and_then(effort_options) else {
        tracing::warn!(
            effort = wanted,
            "agent offers no effort choice; using its default"
        );
        return Ok(None);
    };
    let Some(effort) = match_model(&efforts, wanted) else {
        tracing::warn!(
            effort = wanted,
            "agent has no such effort; using its default"
        );
        return Ok(None);
    };
    match cx
        .send_request(SetSessionConfigOptionRequest::new(
            session.clone(),
            config_id,
            effort.id.as_str(),
        ))
        .block_task()
        .await
    {
        Ok(_) => Ok(Some(effort.id.clone())),
        Err(err) => {
            tracing::warn!(effort = wanted, %err, "agent refused the effort; using its default");
            Ok(None)
        }
    }
}

/// Mode ids that mean "do not ask", most permissive first. Claude calls it
/// `bypassPermissions`, Gemini `yolo`, Codex `full-access`; `auto` is the fallback some
/// agents use for their unattended mode.
const BYPASS_MODES: &[&str] = &[
    "bypasspermissions",
    "yolo",
    "full-access",
    "fullaccess",
    "full_access",
    "danger-full-access",
    "bypass",
    "auto",
];

/// The id of the mode that stops the agent asking for permission, if it offers one.
pub(crate) fn pick_bypass_mode(modes: &SessionModeState) -> Option<String> {
    let normalized = |id: &str| id.to_lowercase();
    BYPASS_MODES.iter().find_map(|wanted| {
        modes
            .available_modes
            .iter()
            .find(|mode| normalized(&mode.id.0) == *wanted)
            .map(|mode| mode.id.0.to_string())
    })
}

/// Switch a fresh session to the agent's bypass mode (permissions = "auto"). A refusal
/// is logged and ignored: the permission handler still approves every request.
pub(crate) async fn enter_bypass_mode(
    cx: &ConnectionTo<Agent>,
    session: &SessionId,
    modes: Option<&SessionModeState>,
) -> Option<String> {
    let modes = modes?;
    let wanted = pick_bypass_mode(modes)?;
    if modes.current_mode_id.0.as_ref() == wanted {
        return Some(wanted);
    }
    match cx
        .send_request(SetSessionModeRequest::new(session.clone(), wanted.clone()))
        .block_task()
        .await
    {
        Ok(_) => Some(wanted),
        Err(err) => {
            tracing::warn!(mode = %wanted, %err, "agent refused its bypass mode");
            None
        }
    }
}

/// A protocol error as one actionable sentence. Agents answer `auth_required` when
/// their CLI is signed out (e.g. an expired login), often with no detail.
pub fn describe_error(backend: &str, err: &Error) -> String {
    if err.code == ErrorCode::AuthRequired {
        let detail = err
            .message
            .strip_prefix("Authentication required")
            .map(|rest| rest.trim_start_matches(':').trim())
            .filter(|rest| !rest.is_empty())
            .map(|rest| format!(" ({rest})"))
            .unwrap_or_default();
        let hint = match backend {
            "claude" => {
                "Run `claude` in a terminal and sign in with /login, or export \
                 ANTHROPIC_API_KEY before starting Chillax"
            }
            "codex" => "Run `codex login`, or export OPENAI_API_KEY before starting Chillax",
            "gemini" => {
                "Run `gemini` and sign in, or export GEMINI_API_KEY before starting Chillax"
            }
            _ => "Sign in with the agent's own CLI",
        };
        let who = if backend.is_empty() {
            "the agent".to_string()
        } else {
            format!("the {} agent", backend_title(backend))
        };
        return format!("{who} isn't signed in{detail}. {hint}, then try again.");
    }
    let text = match &err.data {
        Some(data) if !data.is_null() => format!("{} ({data})", err.message),
        _ => err.message.clone(),
    };
    // Keep the agent's own words: they carry the reset time the dispatcher parses.
    if chillax_core::limits::is_rate_limit(&text) {
        let who = if backend.is_empty() {
            "the agent".to_string()
        } else {
            format!("the {} agent", backend_title(backend))
        };
        return format!("{who} hit its usage limit: {text}");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_errors_say_how_to_sign_in() {
        let bare = describe_error("claude", &Error::auth_required());
        assert!(
            bare.starts_with("the Claude Code agent isn't signed in."),
            "{bare}"
        );
        assert!(bare.contains("/login"));
        let detailed = describe_error(
            "codex",
            &Error::new(-32000, "Authentication required: token expired"),
        );
        assert!(
            detailed.contains("isn't signed in (token expired)"),
            "{detailed}"
        );
        assert!(detailed.contains("codex login"));
        let other = describe_error("x", &Error::invalid_params().data(serde_json::json!("cwd")));
        assert_eq!(other, "Invalid params (\"cwd\")");
    }

    #[test]
    fn usage_limits_keep_the_agents_words_and_are_recognised() {
        let err = Error::new(-32603, "You've hit your session limit · resets 3pm")
            .data(serde_json::json!({"errorKind": "rate_limit"}));
        let text = describe_error("claude", &err);
        assert!(
            text.starts_with("the Claude Code agent hit its usage limit:"),
            "{text}"
        );
        assert!(text.contains("resets 3pm"), "{text}");
        assert!(chillax_core::limits::is_rate_limit(&text));
    }

    fn select_option(
        id: &str,
        category: Option<SessionConfigOptionCategory>,
        values: &[&str],
    ) -> SessionConfigOption {
        use agent_client_protocol::schema::v1::SessionConfigSelectOption;
        let option = SessionConfigOption::select(
            id.to_string(),
            id.to_string(),
            values[0].to_string(),
            values
                .iter()
                .map(|v| SessionConfigSelectOption::new(v.to_string(), *v))
                .collect::<Vec<_>>(),
        );
        match category {
            Some(category) => option.category(category),
            None => option,
        }
    }

    #[test]
    fn effort_is_found_by_category_then_by_well_known_ids() {
        let ids = |options: &[SessionConfigOption]| {
            effort_options(options)
                .map(|(id, choices)| (id, choices.into_iter().map(|c| c.id).collect::<Vec<_>>()))
        };
        let model = select_option("model", Some(SessionConfigOptionCategory::Model), &["a"]);
        let by_category = select_option(
            "think",
            Some(SessionConfigOptionCategory::ThoughtLevel),
            &["low", "high"],
        );
        assert_eq!(
            ids(&[model.clone(), by_category]),
            Some(("think".into(), vec!["low".into(), "high".into()]))
        );
        let by_id = select_option("reasoning_effort", None, &["low", "xhigh"]);
        assert_eq!(
            ids(&[model.clone(), by_id]),
            Some((
                "reasoning_effort".into(),
                vec!["low".into(), "xhigh".into()]
            ))
        );
        // Without an effort option, and a model option is never mistaken for one.
        assert_eq!(ids(std::slice::from_ref(&model)), None);
        assert_eq!(ids(&[]), None);
        assert_eq!(
            model_options(&[model]).map(|(id, _)| id),
            Some("model".into())
        );
    }

    fn modes(current: &str, ids: &[&str]) -> SessionModeState {
        SessionModeState::new(
            current.to_string(),
            ids.iter()
                .map(|id| agent_client_protocol::schema::v1::SessionMode::new(id.to_string(), *id))
                .collect(),
        )
    }

    #[test]
    fn the_most_permissive_offered_mode_is_the_bypass_mode() {
        let pick = |ids: &[&str]| pick_bypass_mode(&modes("default", ids));
        assert_eq!(
            pick(&["default", "acceptEdits", "plan", "bypassPermissions"]).as_deref(),
            Some("bypassPermissions")
        );
        assert_eq!(
            pick(&["default", "autoEdit", "yolo"]).as_deref(),
            Some("yolo")
        );
        assert_eq!(
            pick(&["read-only", "auto", "full-access"]).as_deref(),
            Some("full-access")
        );
        assert_eq!(pick(&["default", "plan"]), None);
        assert_eq!(pick(&[]), None);
    }
}
