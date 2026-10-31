//! Auto-shutdown countdown (F8) and the platform power-off command.

use std::sync::Arc;
use std::time::Duration;

use newskite_types::Settings;

use crate::{lock, Inner, LogLevel, ServiceEvent};

/// The platform power-off command as `(program, arguments)`, or `None` where unsupported.
///
/// Linux: `systemctl poweroff`; Windows: `shutdown /s /t 0`; macOS:
/// `osascript -e 'tell app "System Events" to shut down'`.
pub fn shutdown_command() -> Option<(&'static str, &'static [&'static str])> {
    #[cfg(target_os = "linux")]
    {
        Some(("systemctl", &["poweroff"]))
    }
    #[cfg(target_os = "windows")]
    {
        Some(("shutdown", &["/s", "/t", "0"]))
    }
    #[cfg(target_os = "macos")]
    {
        Some(("osascript", &["-e", "tell app \"System Events\" to shut down"]))
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        None
    }
}

/// Runs the platform power-off command. Compiled out of this crate's unit tests, which
/// therefore can never power off the machine.
#[cfg(not(test))]
fn platform_shutdown() -> Result<(), String> {
    let (program, args) = shutdown_command().ok_or("shutting down is not supported on this platform")?;
    let status = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("could not run {program}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} exited with {status}"))
    }
}

#[cfg(test)]
fn platform_shutdown() -> Result<(), String> {
    Err("system shutdown is disabled in unit tests".into())
}

/// Starts the countdown after a run drained the queue, if configured and allowed.
pub(crate) fn maybe_start_countdown(inner: &Arc<Inner>, settings: &Settings) {
    if !settings.auto_shutdown || inner.shutdown.is_cancelled() {
        return;
    }
    if !inner.opts.allow_shutdown {
        inner.log(LogLevel::Info, "auto-shutdown is not available in this mode; skipped");
        return;
    }
    let secs = settings.auto_shutdown_countdown_secs;
    let token = inner.shutdown.child_token();
    {
        let mut st = inner.lock_state();
        if st.running {
            // A new run started meanwhile; the queue is not finished after all.
            return;
        }
        if let Some(old) = st.countdown.replace(token.clone()) {
            old.cancel();
        }
    }
    inner.log(LogLevel::Warn, format!("queue finished: shutting down in {secs} s unless canceled"));

    let inner = inner.clone();
    inner.rt.clone().spawn(async move {
        for left in (1..=secs).rev() {
            inner.emit(ServiceEvent::ShutdownCountdown { secs_left: left });
            tokio::select! {
                _ = token.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        }
        // Claim the countdown under the lock so a concurrent cancel either wins or is a no-op.
        {
            let mut st = inner.lock_state();
            if token.is_cancelled() {
                return;
            }
            st.countdown = None;
        }
        inner.emit(ServiceEvent::ShutdownCountdown { secs_left: 0 });
        inner.log(LogLevel::Warn, "shutting down the computer now");
        let hook = lock(&inner.shutdown_hook).clone();
        let result = tokio::task::spawn_blocking(move || match hook {
            Some(h) => h(),
            None => platform_shutdown(),
        })
        .await
        .unwrap_or_else(|e| Err(format!("shutdown task failed: {e}")));
        if let Err(e) = result {
            inner.log(LogLevel::Error, format!("shutdown failed: {e}"));
        }
    });
}

/// Cancels a running countdown. See [`crate::Service::cancel_shutdown`].
pub(crate) fn cancel(inner: &Inner) {
    // Cancel while holding the lock so the countdown task cannot claim it concurrently.
    let canceled = {
        let mut st = inner.lock_state();
        st.countdown.take().map(|t| t.cancel()).is_some()
    };
    if canceled {
        inner.emit(ServiceEvent::ShutdownCanceled);
        inner.log(LogLevel::Info, "auto-shutdown canceled");
    }
}
