//! The verbs that open things in herdr (ADR 0018, 6b): workspaces, tabs, splits and
//! `pane run`. The argument builders are pure; the async wrappers go straight to the
//! client and return errors, because a user-run `chillax herdr open` must say why it
//! failed (the failure policy in `Herdr::call` is for the background reporter).

use std::path::PathBuf;

use serde_json::Value;

use super::{Herdr, HerdrError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Right,
    Down,
}

impl Direction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Right => "right",
            Self::Down => "down",
        }
    }
}

/// Which pane a split divides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SplitTarget {
    /// The pane `chillax` runs in (`--current`: herdr reads `HERDR_PANE_ID`).
    Current,
    #[allow(dead_code)] // reached by splitting a pane other than the caller's
    Pane(String),
}

/// Where and how a new workspace, tab or pane starts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewPane {
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    /// `None` leaves it to herdr's default.
    pub focus: Option<bool>,
}

/// What a create or split returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Created {
    pub workspace: Option<String>,
    pub pane: String,
}

impl NewPane {
    fn push_to(&self, args: &mut Vec<String>) {
        if let Some(cwd) = &self.cwd {
            args.extend(["--cwd".into(), cwd.to_string_lossy().into_owned()]);
        }
        for (k, v) in &self.env {
            args.extend(["--env".into(), format!("{k}={v}")]);
        }
        match self.focus {
            Some(true) => args.push("--focus".into()),
            Some(false) => args.push("--no-focus".into()),
            None => {}
        }
    }
}

impl Herdr {
    pub fn workspace_create_args(label: Option<&str>, spec: &NewPane) -> Vec<String> {
        let mut args = vec!["workspace".to_string(), "create".into()];
        if let Some(cwd) = &spec.cwd {
            args.extend(["--cwd".into(), cwd.to_string_lossy().into_owned()]);
        }
        if let Some(label) = label {
            args.extend(["--label".into(), label.into()]);
        }
        let rest = NewPane {
            cwd: None,
            ..spec.clone()
        };
        rest.push_to(&mut args);
        args
    }

    pub fn tab_create_args(label: Option<&str>, spec: &NewPane) -> Vec<String> {
        let mut args = vec!["tab".to_string(), "create".into()];
        if let Some(cwd) = &spec.cwd {
            args.extend(["--cwd".into(), cwd.to_string_lossy().into_owned()]);
        }
        if let Some(label) = label {
            args.extend(["--label".into(), label.into()]);
        }
        let rest = NewPane {
            cwd: None,
            ..spec.clone()
        };
        rest.push_to(&mut args);
        args
    }

    pub fn pane_split_args(
        target: &SplitTarget,
        direction: Direction,
        ratio: Option<f32>,
        spec: &NewPane,
    ) -> Vec<String> {
        let mut args = vec!["pane".to_string(), "split".into()];
        match target {
            SplitTarget::Current => args.push("--current".into()),
            SplitTarget::Pane(id) => args.extend(["--pane".into(), id.clone()]),
        }
        args.extend(["--direction".into(), direction.as_str().into()]);
        if let Some(ratio) = ratio {
            args.extend(["--ratio".into(), ratio.to_string()]);
        }
        spec.push_to(&mut args);
        args
    }

    /// `pane run` types the command and presses Enter in one step.
    pub fn pane_run_args(pane: &str, command: &str) -> Vec<String> {
        vec!["pane".into(), "run".into(), pane.into(), command.into()]
    }

    pub fn workspace_list_args() -> Vec<String> {
        vec!["workspace".into(), "list".into()]
    }

    pub fn workspace_focus_args(id: &str) -> Vec<String> {
        vec!["workspace".into(), "focus".into(), id.into()]
    }

    /// `.result.workspace.workspace_id` and `.result.root_pane.pane_id` (herdr docs).
    pub fn parse_workspace_created(out: &str) -> Result<Created, HerdrError> {
        let v = json(out)?;
        Ok(Created {
            workspace: text(&v, "/result/workspace/workspace_id"),
            pane: text(&v, "/result/root_pane/pane_id")
                .ok_or_else(|| HerdrError("herdr did not report the new pane".into()))?,
        })
    }

    /// `.result.root_pane.pane_id` for a new tab.
    pub fn parse_tab_created(out: &str) -> Result<Created, HerdrError> {
        Self::parse_workspace_created(out)
    }

    /// `.result.pane.pane_id` of a split.
    pub fn parse_split(out: &str) -> Result<Created, HerdrError> {
        let v = json(out)?;
        Ok(Created {
            workspace: None,
            pane: text(&v, "/result/pane/pane_id")
                .ok_or_else(|| HerdrError("herdr did not report the new pane".into()))?,
        })
    }

    pub async fn workspace_create(
        &self,
        label: Option<&str>,
        spec: &NewPane,
    ) -> Result<Created, HerdrError> {
        let out = self
            .client
            .run(&Self::workspace_create_args(label, spec))
            .await?;
        Self::parse_workspace_created(&out)
    }

    pub async fn tab_create(
        &self,
        label: Option<&str>,
        spec: &NewPane,
    ) -> Result<Created, HerdrError> {
        let out = self.client.run(&Self::tab_create_args(label, spec)).await?;
        Self::parse_tab_created(&out)
    }

    pub async fn pane_split(
        &self,
        target: &SplitTarget,
        direction: Direction,
        ratio: Option<f32>,
        spec: &NewPane,
    ) -> Result<Created, HerdrError> {
        let out = self
            .client
            .run(&Self::pane_split_args(target, direction, ratio, spec))
            .await?;
        Self::parse_split(&out)
    }

    pub async fn pane_run(&self, pane: &str, command: &str) -> Result<(), HerdrError> {
        self.client
            .run(&Self::pane_run_args(pane, command))
            .await
            .map(|_| ())
    }

    /// The id of an existing workspace labelled `label`, if `workspace list` shows one.
    /// The list's shape is not documented, so this searches the JSON for an object that
    /// has both a matching `label` and a `workspace_id`.
    pub async fn find_workspace(&self, label: &str) -> Option<String> {
        let out = self.client.run(&Self::workspace_list_args()).await.ok()?;
        find_labelled(&json(&out).ok()?, label)
    }

    pub async fn workspace_focus(&self, id: &str) -> Result<(), HerdrError> {
        self.client
            .run(&Self::workspace_focus_args(id))
            .await
            .map(|_| ())
    }

    /// The working directory of `pane` per `pane get` (`foreground_cwd`, documented as
    /// a field of pane info; searched for rather than assuming where it nests).
    pub async fn pane_cwd(&self, pane: &str) -> Option<PathBuf> {
        let out = self
            .client
            .run(&["pane".into(), "get".into(), pane.into()])
            .await
            .ok()?;
        find_key(&json(&out).ok()?, "foreground_cwd").map(PathBuf::from)
    }

    pub fn pane(&self) -> &str {
        &self.pane
    }
}

fn json(out: &str) -> Result<Value, HerdrError> {
    serde_json::from_str(out).map_err(|e| HerdrError(format!("unexpected herdr output: {e}")))
}

fn text(v: &Value, pointer: &str) -> Option<String> {
    v.pointer(pointer)?.as_str().map(str::to_string)
}

fn find_labelled(v: &Value, label: &str) -> Option<String> {
    match v {
        Value::Object(map) => {
            if map.get("label").and_then(Value::as_str) == Some(label)
                && let Some(id) = map.get("workspace_id").and_then(Value::as_str)
            {
                return Some(id.to_string());
            }
            map.values().find_map(|v| find_labelled(v, label))
        }
        Value::Array(items) => items.iter().find_map(|v| find_labelled(v, label)),
        _ => None,
    }
}

fn find_key(v: &Value, key: &str) -> Option<String> {
    match v {
        Value::Object(map) => map
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| map.values().find_map(|v| find_key(v, key))),
        Value::Array(items) => items.iter().find_map(|v| find_key(v, key)),
        _ => None,
    }
}

/// POSIX single-quote `text` for a command line typed into a pane. Plain words stay bare.
pub fn shell_quote(text: &str) -> String {
    let plain = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c));
    if plain {
        text.to_string()
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}
