//! Supervised background tasks.
//!
//! A bare `tokio::spawn` that panics dies silently: the `JoinHandle` is dropped, so
//! nothing reaches the log and the stakeholder only sees a stalled company. These
//! helpers name the task and log how it ended, so a crash is traceable in `chillax.log`.

use std::fmt::Display;
use std::future::Future;

use tokio::task::{JoinError, JoinHandle};

/// Spawn `fut` and log if it panics. The returned handle finishes when the task does.
pub fn spawn_logged<F>(name: &'static str, fut: F) -> JoinHandle<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let inner = tokio::spawn(fut);
    tokio::spawn(async move {
        match inner.await {
            Ok(()) => tracing::debug!(task = name, "background task finished"),
            Err(err) => log_join_error(name, &err),
        }
    })
}

/// Like [`spawn_logged`] for tasks that return a `Result`; an `Err` is logged too.
pub fn spawn_logged_result<F, E>(name: &'static str, fut: F) -> JoinHandle<()>
where
    F: Future<Output = Result<(), E>> + Send + 'static,
    E: Display + Send + 'static,
{
    let inner = tokio::spawn(fut);
    tokio::spawn(async move {
        match inner.await {
            Ok(Ok(())) => tracing::debug!(task = name, "background task finished"),
            Ok(Err(err)) => tracing::error!(task = name, %err, "background task failed"),
            Err(err) => log_join_error(name, &err),
        }
    })
}

fn log_join_error(name: &'static str, err: &JoinError) {
    if err.is_panic() {
        // The panic hook has already logged the message, location and backtrace.
        tracing::error!(task = name, "background task panicked");
    } else {
        tracing::warn!(task = name, "background task was cancelled");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_panicking_task_does_not_take_the_supervisor_down() {
        let handle = spawn_logged("boom", async { panic!("expected in test") });
        handle.await.expect("supervisor survives the panic");
    }

    #[tokio::test]
    async fn an_err_result_is_reported_without_panicking() {
        let handle = spawn_logged_result("fails", async { Err::<(), _>("nope") });
        handle.await.expect("supervisor survives the error");
    }
}
