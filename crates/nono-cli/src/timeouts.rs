//! Named timeout and polling-interval constants used across the CLI.
//!
//! User-facing timeouts can be overridden via environment variables.
//! Internal poll intervals use fixed defaults.

use std::time::Duration;
use tracing::warn;

// exec_strategy

/// Quiet period to drain final PTY output after child exit before parent
/// diagnostics/prompts take over the terminal.
pub const POST_EXIT_PTY_DRAIN_TIMEOUT: Duration = Duration::from_millis(100);

// pty_proxy

/// Maximum time to wait on final teardown for a terminal's reply to a
/// cursor-position query (`ESC[6n`) the exiting child emitted, so the reply is
/// consumed instead of being pasted into the shell prompt. Bounds the
/// byte-at-a-time read that waits out the (possibly fragmented) reply; kept
/// short so it is imperceptible on exit.
pub const TERMINAL_QUERY_REPLY_TIMEOUT: Duration = Duration::from_millis(30);

// session_commands

/// Poll interval while waiting for a session to exit after SIGTERM.
pub const STOP_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Poll interval when tailing a log file and the reader reaches EOF.
pub const LOG_TAIL_POLL_INTERVAL: Duration = Duration::from_millis(250);

// Configurable user-facing timeouts

/// Read `NONO_PTY_DRAIN_TIMEOUT` (milliseconds). Returns the default when
/// the variable is absent or unparseable.
pub fn pty_drain_timeout() -> Duration {
    env_duration_millis("NONO_PTY_DRAIN_TIMEOUT", POST_EXIT_PTY_DRAIN_TIMEOUT)
}

/// Upper bound for any user-supplied timeout. Prevents `Instant + Duration`
/// overflow from user-controlled values (u64::MAX seconds would panic).
const MAX_TIMEOUT: Duration = Duration::from_secs(3600);

fn env_duration_millis(var: &str, default: Duration) -> Duration {
    match std::env::var(var) {
        Ok(val) => match val.parse::<u64>() {
            Ok(ms) => {
                let d = Duration::from_millis(ms);
                if d > MAX_TIMEOUT {
                    warn!(
                        "{var}={val} exceeds maximum ({} s), clamping",
                        MAX_TIMEOUT.as_secs()
                    );
                    MAX_TIMEOUT
                } else {
                    d
                }
            }
            Err(_) => {
                warn!("{var}={val:?} is not a valid number of milliseconds, using default");
                default
            }
        },
        Err(_) => default,
    }
}
