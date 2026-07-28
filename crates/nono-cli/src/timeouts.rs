//! Named timeout and polling-interval constants used across the CLI.
//!
//! Internal poll intervals use fixed defaults.

use std::time::Duration;

// session_commands

/// Poll interval while waiting for a session to exit after SIGTERM.
pub const STOP_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Poll interval when tailing a log file and the reader reaches EOF.
pub const LOG_TAIL_POLL_INTERVAL: Duration = Duration::from_millis(250);
