use std::path::Path;
use std::time::Duration;

use std::sync::Arc;

use rand::Rng;
use shlex::try_join;
use tokio::sync::Notify;
use tracing::debug;

use crate::config::Config;

const INITIAL_DELAY_MS: u64 = 200;
const BACKOFF_FACTOR: f64 = 2.0;

pub(crate) fn backoff(attempt: u64) -> Duration {
    let exp = BACKOFF_FACTOR.powi(attempt.saturating_sub(1) as i32);
    let base = (INITIAL_DELAY_MS as f64 * exp) as u64;
    let jitter = rand::rng().random_range(0.9..1.1);
    Duration::from_millis((base as f64 * jitter) as u64)
}

pub fn escape_command(command: &[String]) -> String {
    try_join(command.iter().map(|s| s.as_str())).unwrap_or_else(|_| command.join(" "))
}

pub fn strip_bash_lc_and_escape(command: &[String]) -> String {
    match command {
        [first, second, third]
            if is_shell_like_executable(first)
                && (second == "-lc" || second == "-c") =>
        {
            third.clone()
        }
        _ => escape_command(command),
    }
}

pub(crate) fn is_shell_like_executable(token: &str) -> bool {
    let trimmed = token.trim_matches('"').trim_matches('\'');
    let name = Path::new(trimmed)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(trimmed)
        .to_ascii_lowercase();
    matches!(
        name.as_str(),
        "bash"
            | "bash.exe"
            | "sh"
            | "sh.exe"
            | "zsh"
            | "zsh.exe"
            | "dash"
            | "dash.exe"
            | "ksh"
            | "ksh.exe"
            | "busybox"
    )
}

#[allow(dead_code)]
pub fn notify_on_sigint() -> Arc<Notify> {
    let notify = Arc::new(Notify::new());

    tokio::spawn({
        let notify = Arc::clone(&notify);
        async move {
            loop {
                tokio::signal::ctrl_c().await.ok();
                debug!("Keyboard interrupt");
                notify.notify_waiters();
            }
        }
    });

    notify
}

#[allow(dead_code)]
pub fn is_inside_git_repo(config: &Config) -> bool {
    let mut dir = config.cwd.to_path_buf();

    loop {
        if dir.join(".git").exists() {
            return true;
        }

        if !dir.pop() {
            break;
        }
    }

    false
}
