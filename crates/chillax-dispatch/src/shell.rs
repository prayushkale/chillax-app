//! Run the project's own commands.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use chillax_agents::procgroup;
use tokio::process::Command;

/// How long a timed-out command's group gets after SIGTERM before SIGKILL.
const KILL_GRACE: Duration = Duration::from_secs(2);

/// How much of a command's output is kept: the end, where failures are reported.
const TAIL_BYTES: usize = 4000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellResult {
    pub ok: bool,
    /// The last of stdout and stderr, or why the command did not run.
    pub tail: String,
}

/// Run `command` with `sh -c` in `dir`. A command that does not finish within `limit`
/// is killed and counts as failed.
pub async fn run_shell(command: &str, dir: &Path, limit: Duration) -> ShellResult {
    run_shell_with(command, dir, limit, &BTreeMap::new()).await
}

/// [`run_shell`] with extra env vars on top of the inherited ones.
pub async fn run_shell_with(
    command: &str,
    dir: &Path,
    limit: Duration,
    env: &BTreeMap<String, String>,
) -> ShellResult {
    let mut cmd = Command::new("sh");
    cmd.args(["-c", command])
        .current_dir(dir)
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Its own process group, so a timeout or Chillax exiting reaches the servers the
    // command started, not only `sh` (ADR 0015).
    #[cfg(unix)]
    cmd.process_group(0);
    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            return ShellResult {
                ok: false,
                tail: format!("could not run `{command}`: {err}"),
            };
        }
    };
    let pgid = child.id();
    if let Some(pgid) = pgid {
        procgroup::register(pgid);
    }
    match tokio::time::timeout(limit, child.wait_with_output()).await {
        Err(_) => {
            if let Some(pgid) = pgid {
                procgroup::terminate(pgid, KILL_GRACE).await;
            }
            ShellResult {
                ok: false,
                tail: format!("timed out after {}s", limit.as_secs()),
            }
        }
        Ok(Err(err)) => {
            if let Some(pgid) = pgid {
                procgroup::terminate(pgid, KILL_GRACE).await;
            }
            ShellResult {
                ok: false,
                tail: format!("could not run `{command}`: {err}"),
            }
        }
        Ok(Ok(out)) => {
            if let Some(pgid) = pgid {
                // A server the step started on purpose (QA `up`) stays tracked, and is
                // stopped when Chillax exits.
                procgroup::settle(pgid).await;
            }
            let mut text = String::from_utf8_lossy(&out.stdout).to_string();
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            ShellResult {
                ok: out.status.success(),
                tail: tail(text.trim()),
            }
        }
    }
}

fn tail(text: &str) -> String {
    if text.len() <= TAIL_BYTES {
        return text.to_string();
    }
    let mut start = text.len() - TAIL_BYTES;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &text[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn success_and_failure_are_reported_with_output() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ok = run_shell("echo fine", dir.path(), Duration::from_secs(10)).await;
        assert_eq!(
            ok,
            ShellResult {
                ok: true,
                tail: "fine".into()
            }
        );
        let bad = run_shell("echo oops >&2; exit 3", dir.path(), Duration::from_secs(10)).await;
        assert!(!bad.ok);
        assert_eq!(bad.tail, "oops");
    }

    #[tokio::test]
    async fn commands_run_in_the_given_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("marker"), "").expect("write");
        assert!(
            run_shell("test -f marker", dir.path(), Duration::from_secs(10))
                .await
                .ok
        );
    }

    #[tokio::test]
    async fn slow_commands_are_killed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let slow = run_shell("sleep 30", dir.path(), Duration::from_millis(100)).await;
        assert!(!slow.ok);
        assert!(slow.tail.starts_with("timed out"));
    }

    #[cfg(unix)]
    async fn pgid_of(pid: &str) -> String {
        let out = tokio::process::Command::new("ps")
            .args(["-o", "pgid=", "-p", pid])
            .output()
            .await
            .expect("ps runs");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[cfg(unix)]
    async fn pid_alive(pid: &str) -> bool {
        tokio::process::Command::new("kill")
            .args(["-0", pid])
            .stderr(Stdio::null())
            .status()
            .await
            .is_ok_and(|s| s.success())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_server_a_step_leaves_running_is_tracked_and_ended_with_its_group() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ok = run_shell(
            "sleep 1000 >/dev/null 2>&1 & echo $!",
            dir.path(),
            Duration::from_secs(10),
        )
        .await;
        assert!(ok.ok);
        let pid = ok.tail.clone();
        assert!(pid_alive(&pid).await, "the step's server keeps running");
        let pgid: u32 = pgid_of(&pid).await.parse().expect("pgid");
        assert!(procgroup::registered().contains(&pgid));
        procgroup::terminate(pgid, Duration::from_secs(5)).await;
        for _ in 0..40 {
            if !pid_alive(&pid).await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(!pid_alive(&pid).await, "group kill reaches the server");
        assert!(!procgroup::registered().contains(&pgid));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_timeout_kills_servers_the_step_started() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_file = dir.path().join("pid");
        let command = format!(
            "sleep 1000 >/dev/null 2>&1 & echo $! > {}; sleep 1000",
            pid_file.display()
        );
        let slow = run_shell(&command, dir.path(), Duration::from_millis(600)).await;
        assert!(slow.tail.starts_with("timed out"));
        let pid = std::fs::read_to_string(&pid_file)
            .expect("pid")
            .trim()
            .to_string();
        for _ in 0..40 {
            if !pid_alive(&pid).await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("background sleep {pid} survived the timeout");
    }

    #[tokio::test]
    async fn long_output_keeps_its_end() {
        let dir = tempfile::tempdir().expect("tempdir");
        let result = run_shell(
            "yes line | head -n 5000; echo the-end",
            dir.path(),
            Duration::from_secs(10),
        )
        .await;
        assert!(result.tail.starts_with('…'));
        assert!(result.tail.ends_with("the-end"));
        assert!(result.tail.len() <= TAIL_BYTES + 8);
    }
}
