//! The `Environment` adapter that runs the project's own shell commands:
//! `[env.dev] test`, `[env.qa] up/test/down` and `[env.prod] deploy`.

use std::path::Path;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chillax_agents::EnvFiles;
use chillax_core::EnvStep;
use chillax_core::config::{ProdEnv, QaEnv};
use chillax_core::env::{EnvError, EnvRun, Environment};

use crate::shell::run_shell_with;

/// The commands a [`ShellEnvironment`] runs; swapped as a whole on a config reload.
#[derive(Clone)]
struct Commands {
    dev_test: Option<String>,
    dev_timeout: Duration,
    qa: QaEnv,
    prod: ProdEnv,
}

pub struct ShellEnvironment {
    commands: RwLock<Commands>,
    env_files: EnvFiles,
}

impl ShellEnvironment {
    pub fn new(dev_test: Option<String>, dev_timeout: Duration, qa: QaEnv, prod: ProdEnv) -> Self {
        Self {
            commands: RwLock::new(Commands {
                dev_test,
                dev_timeout,
                qa,
                prod,
            }),
            env_files: EnvFiles::default(),
        }
    }

    /// Give each step the project's env files for its environment.
    pub fn with_env_files(mut self, env_files: EnvFiles) -> Self {
        self.env_files = env_files;
        self
    }

    /// Use new commands from the next step on (live config reload); running steps finish
    /// with the ones they started with.
    pub fn set_commands(
        &self,
        dev_test: Option<String>,
        dev_timeout: Duration,
        qa: QaEnv,
        prod: ProdEnv,
    ) {
        let mut commands = self.commands.write().expect("commands lock poisoned");
        *commands = Commands {
            dev_test,
            dev_timeout,
            qa,
            prod,
        };
    }

    /// The command for a step and how long it may run. `None` when none is configured.
    fn command(&self, env: &str, step: EnvStep) -> Option<(String, Duration)> {
        let c = self.commands.read().expect("commands lock poisoned");
        let qa = Duration::from_secs(c.qa.timeout_secs);
        let found = match (env, step) {
            ("dev", EnvStep::Test) => c.dev_test.as_deref().map(|x| (x, c.dev_timeout)),
            ("qa", EnvStep::Up) => c.qa.up.as_deref().map(|x| (x, qa)),
            ("qa", EnvStep::Test) => c.qa.test.as_deref().map(|x| (x, qa)),
            ("qa", EnvStep::Down) => c.qa.down.as_deref().map(|x| (x, qa)),
            ("prod", EnvStep::Deploy) => c
                .prod
                .deploy
                .as_deref()
                .map(|x| (x, Duration::from_secs(c.prod.timeout_secs))),
            _ => None,
        };
        found.map(|(x, d)| (x.to_string(), d))
    }
}

#[async_trait]
impl Environment for ShellEnvironment {
    async fn run(&self, env: &str, step: EnvStep, cwd: &Path) -> Result<EnvRun, EnvError> {
        let Some((command, limit)) = self.command(env, step) else {
            return Ok(EnvRun::skipped());
        };
        let started = Instant::now();
        let vars = self.env_files.vars(env).await;
        let result = run_shell_with(&command, cwd, limit, &vars).await;
        Ok(EnvRun {
            ok: result.ok,
            skipped: false,
            exit: None,
            output_tail: result.tail,
            duration: started.elapsed(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn environment(qa: QaEnv, prod: ProdEnv) -> ShellEnvironment {
        ShellEnvironment::new(
            Some("echo dev-test".into()),
            Duration::from_secs(10),
            qa,
            prod,
        )
    }

    #[tokio::test]
    async fn a_step_without_a_command_is_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = environment(QaEnv::default(), ProdEnv::default());
        for (name, step) in [
            ("qa", EnvStep::Up),
            ("qa", EnvStep::Test),
            ("qa", EnvStep::Down),
            ("prod", EnvStep::Deploy),
            ("staging", EnvStep::Up),
        ] {
            let run = env.run(name, step, dir.path()).await.expect("runs");
            assert!(run.skipped && run.ok, "{name} {step:?}");
        }
    }

    #[tokio::test]
    async fn configured_commands_run_in_the_directory_and_report_their_output() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("marker"), "").expect("write");
        let env = environment(
            QaEnv {
                up: Some("test -f marker && echo up".into()),
                test: Some("echo broken >&2; exit 2".into()),
                ..QaEnv::default()
            },
            ProdEnv {
                deploy: Some("echo shipped".into()),
                ..ProdEnv::default()
            },
        );
        let up = env.run("qa", EnvStep::Up, dir.path()).await.expect("runs");
        assert!(up.ok && !up.skipped);
        assert_eq!(up.output_tail, "up");
        let test = env
            .run("qa", EnvStep::Test, dir.path())
            .await
            .expect("runs");
        assert!(!test.ok && !test.skipped);
        assert_eq!(test.output_tail, "broken");
        let deploy = env
            .run("prod", EnvStep::Deploy, dir.path())
            .await
            .expect("runs");
        assert_eq!(deploy.output_tail, "shipped");
        let dev = env
            .run("dev", EnvStep::Test, dir.path())
            .await
            .expect("runs");
        assert_eq!(dev.output_tail, "dev-test");
    }

    #[tokio::test]
    async fn new_commands_apply_from_the_next_step() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = environment(QaEnv::default(), ProdEnv::default());
        let before = env.run("qa", EnvStep::Up, dir.path()).await.expect("runs");
        assert!(before.skipped);
        env.set_commands(
            None,
            Duration::from_secs(10),
            QaEnv {
                up: Some("echo migrated".into()),
                ..QaEnv::default()
            },
            ProdEnv::default(),
        );
        let after = env.run("qa", EnvStep::Up, dir.path()).await.expect("runs");
        assert!(!after.skipped);
        assert_eq!(after.output_tail, "migrated");
    }

    #[tokio::test]
    async fn each_environment_has_its_own_time_limit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = environment(
            QaEnv {
                up: Some("sleep 30".into()),
                timeout_secs: 1,
                ..QaEnv::default()
            },
            ProdEnv::default(),
        );
        let up = env.run("qa", EnvStep::Up, dir.path()).await.expect("runs");
        assert!(!up.ok);
        assert!(
            up.output_tail.starts_with("timed out"),
            "{}",
            up.output_tail
        );
    }

    #[tokio::test]
    async fn steps_get_the_env_files_of_their_environment() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(root.path().join(".env"), "FOO=shared\nBAR=1\n").expect("write");
        std::fs::write(root.path().join(".env.qa"), "FOO=qa\n").expect("write");
        let tree = tempfile::tempdir().expect("tempdir");
        let env = ShellEnvironment::new(
            Some("test \"$FOO$BAR\" = shared1".into()),
            Duration::from_secs(10),
            QaEnv {
                test: Some("test \"$FOO$BAR\" = qa1".into()),
                ..QaEnv::default()
            },
            ProdEnv::default(),
        )
        .with_env_files(EnvFiles::new(
            root.path(),
            &chillax_core::config::EnvConfig::default(),
        ));
        let qa = env
            .run("qa", EnvStep::Test, tree.path())
            .await
            .expect("runs");
        assert!(qa.ok, "{}", qa.output_tail);
        let dev = env
            .run("dev", EnvStep::Test, tree.path())
            .await
            .expect("runs");
        assert!(dev.ok, "{}", dev.output_tail);
    }
}
