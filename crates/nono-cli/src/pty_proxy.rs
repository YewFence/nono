//! PTY relay for foreground sandboxed sessions.
//!
//! The supervisor interposes a PTY between the real terminal and the sandboxed
//! child process while the PTY runtime remains in use.
//!
//! Architecture:
//! ```text
//!   real terminal <---> supervisor (PTY relay) <---> PTY master/slave <---> child
//! ```

use nix::libc;
use nix::pty::{OpenptyResult, Winsize, openpty};
use nono::{NonoError, Result};
use std::collections::VecDeque;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};
use tracing::{debug, warn};

use crate::timeouts;

const SCROLLBACK_LIMIT_BYTES: usize = 8 * 1024 * 1024;
const VT_SCROLLBACK_ROWS: usize = 10_000;
const MAX_ENHANCED_KEY_SEQUENCE_LEN: usize = 32;
// Composed terminal escape sequences. Each concat! block documents its
// individual CSI sequences inline so the byte-level intent is auditable
// without having to decode raw hex.
const ALT_SCREEN_RESTORE_ESCAPE: &[u8] = concat!(
    "\x1b[0m",       // reset attributes
    "\x1b(B\x1b)B",  // set G0/G1 charset to ASCII
    "\x0f",          // shift-in (select G0)
    "\x1b[r",        // reset scroll region
    "\x1b[?6l",      // disable origin mode
    "\x1b[?1049h",   // enter alternate screen
    "\x1b[?25h",     // show cursor
    "\x1b[2J\x1b[H", // clear screen + cursor home
)
.as_bytes();

const TERMINAL_RESTORE_NORMAL: &[u8] = concat!(
    "\x1b[<u", // restore cursor (kitty private)
    "\x1b[>0n\x1b[>1n\x1b[>2n\x1b[>3n\x1b[>4n\x1b[>6n\x1b[>7n", // disable key reporting
    "\x1b[?1000l\x1b[?1002l\x1b[?1003l", // disable mouse tracking
    "\x1b[?1005l\x1b[?1006l\x1b[?1015l", // disable mouse encodings
    "\x1b[?1004l", // disable focus events
    "\x1b[?2004l", // disable bracketed paste
    "\x1b[?1l", // disable application cursor keys
    "\x1b>",   // normal keypad mode
    "\x1b[?25h", // show cursor
)
.as_bytes();

const TERMINAL_RESTORE_ESCAPE: &[u8] = concat!(
    "\x1b[<u", // restore cursor (kitty private)
    "\x1b[>0n\x1b[>1n\x1b[>2n\x1b[>3n\x1b[>4n\x1b[>6n\x1b[>7n", // disable key reporting
    "\x1b[?1000l\x1b[?1002l\x1b[?1003l", // disable mouse tracking
    "\x1b[?1005l\x1b[?1006l\x1b[?1015l", // disable mouse encodings
    "\x1b[?1004l", // disable focus events
    "\x1b[?2004l", // disable bracketed paste
    "\x1b[?1049l", // exit alternate screen
    "\x1b[?25h", // show cursor
)
.as_bytes();

const TERMINAL_RESTORE_AND_CLEAR_ESCAPE: &[u8] = concat!(
    "\x1b[<u", // restore cursor (kitty private)
    "\x1b[>0n\x1b[>1n\x1b[>2n\x1b[>3n\x1b[>4n\x1b[>6n\x1b[>7n", // disable key reporting
    "\x1b[?1000l\x1b[?1002l\x1b[?1003l", // disable mouse tracking
    "\x1b[?1005l\x1b[?1006l\x1b[?1015l", // disable mouse encodings
    "\x1b[?1004l", // disable focus events
    "\x1b[?2004l", // disable bracketed paste
    "\x1b[?1l", // disable application cursor keys
    "\x1b>",   // normal keypad mode
    "\x1b[?1049l", // exit alternate screen
    "\x1b[?25h", // show cursor
    "\x1b[2J\x1b[H", // clear screen + cursor home
)
.as_bytes();

const CLEAR_PARENT_OUTPUT_AREA: &[u8] = b"\r\x1b[K\x1b[J";

/// PTY pair used by the foreground relay.
pub struct PtyPair {
    /// Master side — held by the supervisor for I/O proxying.
    pub master: OwnedFd,
    /// Slave side — becomes the child's stdin/stdout/stderr.
    pub slave: OwnedFd,
}

enum ReadFdOutcome {
    Data(usize),
    Eof,
    Retry,
}

enum MasterProxyOutcome {
    Data,
    Closed,
    Retry,
}

/// Tracks how far the teardown drain has matched a cursor-position reply
/// (`ESC [ <params> R`). `step` returns `false` at the terminator or the first
/// byte that cannot belong to a reply, so type-ahead is left for the shell
/// rather than scanned for a stray `R`.
#[derive(Clone, Copy)]
enum CprReplyParse {
    NeedEsc,
    NeedBracket,
    InParams,
}

impl CprReplyParse {
    /// Feed one byte; returns `true` while more reply bytes may follow, `false`
    /// once the reply ends (`R`) or the byte breaks the grammar.
    fn step(&mut self, byte: u8) -> bool {
        match (*self, byte) {
            (Self::NeedEsc, 0x1b) => *self = Self::NeedBracket,
            (Self::NeedBracket, b'[') => *self = Self::InParams,
            // Parameter bytes (`<row>;<col>`) keep the reply going.
            (Self::InParams, b'0'..=b'9' | b';') => {}
            // `R` terminator, or any byte that cannot belong to a reply: stop.
            _ => return false,
        }
        true
    }
}

struct ScreenState {
    parser: vt100::Parser,
}

impl ScreenState {
    fn new(rows: usize, cols: usize) -> Self {
        let rows = rows.max(1).min(u16::MAX as usize) as u16;
        let cols = cols.max(1).min(u16::MAX as usize) as u16;
        Self {
            parser: vt100::Parser::new(rows, cols, VT_SCROLLBACK_ROWS),
        }
    }

    fn apply_bytes(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    fn render(&self) -> Vec<u8> {
        self.parser.screen().state_formatted()
    }

    fn render_plaintext(&self) -> String {
        self.parser.screen().contents()
    }

    fn cursor_position(&self) -> (u16, u16) {
        self.parser.screen().cursor_position()
    }

    fn alternate_screen_active(&self) -> bool {
        self.parser.screen().alternate_screen()
    }
}

/// The running PTY proxy state managed by the supervisor.
pub struct PtyProxy {
    /// PTY master fd
    master: OwnedFd,
    /// Whether stdin should still be polled.
    stdin_active: bool,
    /// Whether the foreground terminal is owned by the relay.
    terminal_active: bool,
    /// Saved terminal settings.
    pub(crate) saved_termios: Option<nix::sys::termios::Termios>,
    /// Recent PTY output retained for diagnostics.
    scrollback: VecDeque<u8>,
    /// Last visible screen state for diagnostics and job-control restoration.
    screen: ScreenState,
    /// Buffered enhanced key report bytes for Ctrl-Z handling.
    pending_key_escape: Vec<u8>,
    /// Ctrl-Z suspension requested from a terminal client.
    suspension_requested: bool,
}

/// Open a PTY pair, inheriting the current terminal's window size.
pub fn open_pty() -> Result<PtyPair> {
    // Get current terminal window size if available
    let winsize = get_terminal_winsize();

    let OpenptyResult { master, slave } = openpty(winsize.as_ref(), None)
        .map_err(|e| NonoError::SandboxInit(format!("openpty() failed: {}", e)))?;

    Ok(PtyPair { master, slave })
}

/// Write a message to stderr and abort the child process.
///
/// Async-signal-safe: only uses raw `write(2)` and `_exit(2)`.
fn child_setup_pty_fatal(message: &[u8]) -> ! {
    // SAFETY: message slice pointer is valid for its length; write(2) and
    // _exit(2) are async-signal-safe and cannot cause memory unsafety.
    unsafe {
        let _ = libc::write(
            libc::STDERR_FILENO,
            message.as_ptr().cast::<libc::c_void>(),
            message.len(),
        );
        libc::_exit(126);
    }
}

/// Set up the slave PTY as the child's controlling terminal.
///
/// Must be called in the child after fork, before exec.
///
/// # Safety
/// Must be called in a freshly-forked child process. `slave_fd` must be a
/// valid open file descriptor for the slave side of a PTY pair.
pub unsafe fn setup_child_pty(slave_fd: RawFd) {
    if nix::unistd::setsid().is_err() {
        child_setup_pty_fatal(b"nono: setsid() failed while configuring child PTY\n");
    }

    // SAFETY: post-fork child; slave_fd is valid per caller contract.
    // ioctl/dup2/close operate on raw fd integers — nix's IO-safe wrappers
    // require AsFd/OwnedFd which aren't available for STDIN_FILENO et al.
    unsafe {
        if libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0) < 0 {
            child_setup_pty_fatal(b"nono: ioctl(TIOCSCTTY) failed while configuring child PTY\n");
        }

        if libc::dup2(slave_fd, libc::STDIN_FILENO) < 0 {
            child_setup_pty_fatal(b"nono: dup2(stdin) failed while configuring child PTY\n");
        }
        if libc::dup2(slave_fd, libc::STDOUT_FILENO) < 0 {
            child_setup_pty_fatal(b"nono: dup2(stdout) failed while configuring child PTY\n");
        }
        if libc::dup2(slave_fd, libc::STDERR_FILENO) < 0 {
            child_setup_pty_fatal(b"nono: dup2(stderr) failed while configuring child PTY\n");
        }

        if slave_fd > 2 {
            libc::close(slave_fd);
        }
    }
}

/// Get the current terminal window size, if available.
fn get_terminal_winsize() -> Option<Winsize> {
    let mut ws: Winsize = unsafe { std::mem::zeroed() };
    // SAFETY: ioctl with TIOCGWINSZ reads window size into ws
    let ret = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) };
    if ret == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
        Some(ws)
    } else {
        None
    }
}

impl PtyProxy {
    /// Create a foreground PTY relay.
    pub fn new(master: OwnedFd) -> Result<Self> {
        let winsize = current_winsize(master.as_raw_fd()).unwrap_or(Winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        });

        Ok(Self {
            master,
            stdin_active: true,
            terminal_active: true,
            saved_termios: set_terminal_raw(),
            scrollback: VecDeque::with_capacity(SCROLLBACK_LIMIT_BYTES.min(64 * 1024)),
            screen: ScreenState::new(winsize.ws_row as usize, winsize.ws_col as usize),
            pending_key_escape: Vec::new(),
            suspension_requested: false,
        })
    }

    /// Release the local terminal for a final supervisor-owned prompt.
    ///
    /// This leaves the child screen, restores cooked terminal mode, and
    /// releases the terminal so later teardown does not redraw or clear it.
    pub fn release_terminal_for_prompt(&mut self) -> bool {
        if !self.terminal_active {
            return false;
        }

        let in_alt_screen = self.screen.alternate_screen_active();
        leave_child_screen(in_alt_screen);
        // Swallow a late cursor-position reply (`ESC[<row>;<col>R`) that an
        // exiting TUI's `ESC[6n` query leaves in the input queue; otherwise it
        // is handed to the shell and pasted into the next prompt (e.g. `3;1R`).
        // Done here, still in raw mode, on the final teardown path only —
        // `restore_terminal()` stays non-draining so the suspend/resume and
        // temporary-prompt paths preserve type-ahead.
        discard_late_terminal_input(libc::STDIN_FILENO, timeouts::TERMINAL_QUERY_REPLY_TIMEOUT);
        self.restore_terminal();
        // If the child's last output had no trailing newline, `\r\x1b[K` inside
        // `prepare_parent_output_area` would erase it.  Emit a newline first so
        // the child's output is preserved.  Skip in alt-screen: the terminal
        // restores the normal-screen cursor on exit, making the column moot.
        if !in_alt_screen {
            let (_row, col) = self.screen.cursor_position();
            if col > 0 {
                let _ = write_all_fd(libc::STDOUT_FILENO, b"\n");
            }
        }
        prepare_parent_output_area();
        self.terminal_active = false;
        self.stdin_active = false;
        self.pending_key_escape.clear();
        true
    }

    /// Before suspending, if the child is in the alternate screen buffer, exit
    /// it so the shell's "[1]+ Stopped" prompt shows on the normal screen. Use
    /// the clearing restore so the normal screen starts clean:
    /// without the clear, the cursor lands at a stale position in the normal
    /// buffer and the shell prompt mixes with leftover lines, drift that
    /// accumulates across repeated Ctrl-Z/fg cycles.
    pub(crate) fn leave_screen_for_suspension(&self) {
        if self.screen.alternate_screen_active() {
            let _ = write_all_fd(libc::STDOUT_FILENO, terminal_restore_escape(true));
            drain_terminal_output(libc::STDOUT_FILENO);
        }
    }

    /// On resume, re-enter the alternate screen and repaint it from nono's
    /// captured screen state. Emitting only the alt-screen-enter sequence leaves
    /// a blank buffer that TUIs which ignore SIGWINCH (e.g. opencode/opentui)
    /// never repaint. Instead we reconstruct the screen the same way a
    /// terminal restoration does: alt-screen enter + a full vt100 repaint of the
    /// current contents (`ScreenState::render` -> `state_formatted`).
    pub(crate) fn reenter_screen_for_resume(&self) {
        if self.screen.alternate_screen_active() {
            let _ = write_all_fd(libc::STDOUT_FILENO, ALT_SCREEN_RESTORE_ESCAPE);
            let _ = write_all_fd(libc::STDOUT_FILENO, &self.scrollback_snapshot());
            drain_terminal_output(libc::STDOUT_FILENO);
        }
    }

    /// Get poll fds for the supervisor loop.
    ///
    /// Returns `(master_fd, stdin_fd)`; stdin is `-1` after EOF.
    pub fn poll_fds(&self) -> (RawFd, RawFd) {
        (
            self.master.as_raw_fd(),
            if self.stdin_active {
                libc::STDIN_FILENO
            } else {
                -1
            },
        )
    }

    /// Borrow the PTY master fd for ioctl operations (e.g. tcgetpgrp).
    pub(crate) fn master_fd(&self) -> &OwnedFd {
        &self.master
    }

    /// Proxy data from the PTY master to the foreground terminal (child -> user).
    ///
    /// Returns false if the PTY master became unavailable.
    #[must_use = "false indicates the PTY master is no longer usable"]
    pub fn proxy_master_to_client(&mut self) -> bool {
        !matches!(
            self.proxy_master_to_client_once(),
            MasterProxyOutcome::Closed
        )
    }

    fn proxy_master_to_client_once(&mut self) -> MasterProxyOutcome {
        let mut buf = [0u8; 4096];
        let n = match read_fd_once(self.master.as_raw_fd(), &mut buf) {
            Ok(ReadFdOutcome::Data(n)) => n,
            Ok(ReadFdOutcome::Eof) => return MasterProxyOutcome::Closed,
            Ok(ReadFdOutcome::Retry) => return MasterProxyOutcome::Retry,
            Err(err) => {
                debug!("PTY proxy: failed reading PTY master: {}", err);
                return MasterProxyOutcome::Closed;
            }
        };

        self.record_output(&buf[..n]);

        if self.terminal_active
            && let Err(err) = write_all_fd(libc::STDOUT_FILENO, &buf[..n])
        {
            warn!("PTY relay terminal output write failed: {err}");
            self.terminal_active = false;
        }

        MasterProxyOutcome::Data
    }

    /// Drain child output still queued on the PTY master after the child exits.
    ///
    /// `waitpid` can report the child exit before the supervisor has relayed the
    /// final terminal bytes. Draining here keeps parent-owned diagnostics and
    /// prompts ordered after the application's own stderr/stdout.
    pub fn drain_master_output(&mut self, quiet_timeout: Duration) {
        let mut quiet_deadline = Instant::now() + quiet_timeout;

        loop {
            let now = Instant::now();
            if now >= quiet_deadline {
                break;
            }
            let remaining = quiet_deadline.saturating_duration_since(now);
            let timeout_ms = remaining.as_millis().min(i32::MAX as u128) as i32;
            let mut pfd = libc::pollfd {
                fd: self.master.as_raw_fd(),
                events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                revents: 0,
            };

            let ret = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
            if ret > 0 {
                if pfd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                    match self.proxy_master_to_client_once() {
                        MasterProxyOutcome::Data => {
                            quiet_deadline = Instant::now() + quiet_timeout;
                            continue;
                        }
                        MasterProxyOutcome::Retry => {
                            if pfd.revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                                break;
                            }
                            continue;
                        }
                        MasterProxyOutcome::Closed => break,
                    }
                }
                if pfd.revents & libc::POLLNVAL != 0 {
                    break;
                }
            } else if ret == 0 {
                break;
            } else {
                let err = std::io::Error::last_os_error();
                if err.kind() != std::io::ErrorKind::Interrupted {
                    debug!("PTY proxy: post-exit drain poll failed: {}", err);
                    break;
                }
            }
        }
    }

    /// Proxy data from the foreground terminal to the PTY master (user → child).
    ///
    /// Returns false if the PTY master became unavailable.
    #[must_use = "false indicates the PTY master is no longer usable"]
    pub fn proxy_client_to_master(&mut self) -> bool {
        if !self.stdin_active {
            return true;
        }

        let mut buf = [0u8; 4096];
        let n = match read_fd_once(libc::STDIN_FILENO, &mut buf) {
            Ok(ReadFdOutcome::Data(n)) => n,
            Ok(ReadFdOutcome::Eof) => {
                self.stdin_active = false;
                return true;
            }
            Ok(ReadFdOutcome::Retry) => return true,
            Err(err) => {
                warn!("PTY relay terminal input read failed: {err}");
                self.stdin_active = false;
                return true;
            }
        };

        let forwarded = self.filter_client_input(&buf[..n]);
        if !forwarded.is_empty()
            && let Err(err) = write_all_fd(self.master.as_raw_fd(), &forwarded)
        {
            warn!("PTY relay failed forwarding terminal input to PTY master: {err}");
            return false;
        }

        true
    }

    pub fn take_suspension_request(&mut self) -> bool {
        std::mem::take(&mut self.suspension_requested)
    }

    /// Temporarily restore the local terminal so the parent can prompt.
    ///
    /// Returns true when the foreground terminal was paused and must later
    /// be resumed with [`Self::resume_terminal_after_prompt`].
    pub fn pause_terminal_for_prompt(&mut self) -> bool {
        if self.terminal_active {
            leave_child_screen(self.screen.alternate_screen_active());
            self.restore_terminal();
            true
        } else {
            false
        }
    }

    /// Re-enter relay terminal mode after a supervisor-owned prompt.
    ///
    /// Returns true when the foreground terminal was resumed.
    pub fn resume_terminal_after_prompt(&mut self) -> bool {
        if !self.terminal_active {
            return false;
        }

        if self.saved_termios.is_none() {
            self.saved_termios = set_terminal_raw();
        }
        self.reenter_screen_for_resume();
        true
    }

    /// Restore terminal settings.
    pub(crate) fn restore_terminal(&mut self) {
        if let Some(ref termios) = self.saved_termios {
            let _ = nix::sys::termios::tcsetattr(
                std::io::stdin(),
                nix::sys::termios::SetArg::TCSANOW,
                termios,
            );
            self.saved_termios = None;
        }
    }

    fn record_output(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }

        let first_output = self.scrollback.is_empty();
        self.screen.apply_bytes(bytes);

        if bytes.len() >= SCROLLBACK_LIMIT_BYTES {
            self.scrollback.clear();
            self.scrollback.extend(
                bytes[bytes.len() - SCROLLBACK_LIMIT_BYTES..]
                    .iter()
                    .copied(),
            );
            return;
        }

        let overflow = self
            .scrollback
            .len()
            .saturating_add(bytes.len())
            .saturating_sub(SCROLLBACK_LIMIT_BYTES);
        if overflow > 0 {
            drop(self.scrollback.drain(..overflow));
        }
        self.scrollback.extend(bytes.iter().copied());
        let _ = first_output;
    }

    fn scrollback_snapshot(&self) -> Vec<u8> {
        self.screen.render()
    }

    /// Return captured terminal output as plain text for diagnostic analysis.
    ///
    /// Called after the child exits so the supervisor can search for
    /// sandbox-related error messages in the terminal output.
    pub fn screen_plaintext(&self) -> String {
        let mut captured = Vec::with_capacity(self.scrollback.len());
        captured.extend(self.scrollback.iter().copied());
        let scrollback = String::from_utf8_lossy(&captured).into_owned();
        let screen = self.screen.render_plaintext();

        if scrollback.trim().is_empty() {
            return screen;
        }

        if screen.trim().is_empty() || scrollback.contains(screen.trim_end()) {
            return scrollback;
        }

        format!("{scrollback}\n{screen}")
    }

    /// Returns true once the child has rendered visible terminal content.
    pub fn has_visible_output(&self) -> bool {
        self.screen
            .render_plaintext()
            .chars()
            .any(|ch| !ch.is_whitespace())
    }

    /// Returns true once the child appears interactive: either it has entered
    /// alt-screen (TUI) or it has written visible non-whitespace output (REPL,
    /// shell, readline prompt). Both signals mean the process is running and
    /// the startup-timeout should not fire.
    pub fn is_interactive(&self) -> bool {
        self.screen.alternate_screen_active() || self.has_visible_output()
    }

    fn filter_client_input(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut forwarded = Vec::with_capacity(bytes.len());
        for (index, &byte) in bytes.iter().enumerate() {
            if byte == 0x1A {
                self.suspension_requested = true;
                continue;
            }
            if self.maybe_consume_enhanced_key_byte(byte, &mut forwarded) {
                continue;
            }
            if byte == b'\x1b' && bytes.get(index + 1).copied() == Some(b'[') {
                self.pending_key_escape.push(byte);
                continue;
            }
            forwarded.push(byte);
        }
        forwarded
    }

    fn maybe_consume_enhanced_key_byte(&mut self, byte: u8, forwarded: &mut Vec<u8>) -> bool {
        if self.pending_key_escape.is_empty() {
            return false;
        }

        self.pending_key_escape.push(byte);
        match match_enhanced_key_sequence(&self.pending_key_escape, 0x1a) {
            EnhancedKeyMatch::Pending => {
                if self.pending_key_escape.len() > MAX_ENHANCED_KEY_SEQUENCE_LEN {
                    forwarded.append(&mut self.pending_key_escape);
                }
            }
            EnhancedKeyMatch::Matched => {
                self.pending_key_escape.clear();
                self.suspension_requested = true;
            }
            EnhancedKeyMatch::Invalid => forwarded.append(&mut self.pending_key_escape),
        }

        true
    }
}

enum EnhancedKeyMatch {
    Pending,
    Matched,
    Invalid,
}

fn match_enhanced_key_sequence(bytes: &[u8], expected_key: u8) -> EnhancedKeyMatch {
    if bytes.is_empty() {
        return EnhancedKeyMatch::Pending;
    }
    if bytes[0] != b'\x1b' {
        return EnhancedKeyMatch::Invalid;
    }
    if bytes.len() == 1 {
        return EnhancedKeyMatch::Pending;
    }
    if bytes[1] != b'[' {
        return EnhancedKeyMatch::Invalid;
    }
    if bytes.len() == 2 {
        return EnhancedKeyMatch::Pending;
    }

    let payload = &bytes[2..];
    let Some((&last, body)) = payload.split_last() else {
        return EnhancedKeyMatch::Pending;
    };

    if last == b'u' {
        if body.is_empty()
            || !body
                .iter()
                .all(|b| b.is_ascii_digit() || matches!(b, b';' | b':'))
        {
            return EnhancedKeyMatch::Invalid;
        }
        let mut fields = body.split(|b| matches!(b, b';' | b':'));
        let Some(first_field) = fields.next() else {
            return EnhancedKeyMatch::Invalid;
        };
        if first_field.is_empty() {
            return EnhancedKeyMatch::Invalid;
        }
        let Some(codepoint) = parse_ascii_u32(first_field) else {
            return EnhancedKeyMatch::Invalid;
        };
        let modifiers = fields.find_map(parse_ascii_u32).unwrap_or(1);
        return if enhanced_key_matches(expected_key, codepoint, modifiers) {
            EnhancedKeyMatch::Matched
        } else {
            EnhancedKeyMatch::Invalid
        };
    }

    if last == b'~' {
        let fields: Vec<&[u8]> = body.split(|b| *b == b';').collect();
        if fields.len() == 3
            && fields[0] == b"27"
            && fields[1].iter().all(|b| b.is_ascii_digit())
            && fields[2].iter().all(|b| b.is_ascii_digit())
        {
            let Some(modifiers) = parse_ascii_u32(fields[1]) else {
                return EnhancedKeyMatch::Invalid;
            };
            let Some(codepoint) = parse_ascii_u32(fields[2]) else {
                return EnhancedKeyMatch::Invalid;
            };
            return if enhanced_key_matches(expected_key, codepoint, modifiers) {
                EnhancedKeyMatch::Matched
            } else {
                EnhancedKeyMatch::Invalid
            };
        }
    }

    if (last.is_ascii_digit() || matches!(last, b';' | b':'))
        && body
            .iter()
            .all(|b| b.is_ascii_digit() || matches!(b, b';' | b':' | b'~'))
    {
        return EnhancedKeyMatch::Pending;
    }

    EnhancedKeyMatch::Invalid
}

fn parse_ascii_u32(bytes: &[u8]) -> Option<u32> {
    std::str::from_utf8(bytes).ok()?.parse::<u32>().ok()
}

fn enhanced_key_matches(expected_key: u8, codepoint: u32, modifiers: u32) -> bool {
    if modifiers == 1 {
        return codepoint == u32::from(expected_key)
            && expected_key.is_ascii_graphic().then_some(()).is_some()
            || (expected_key == b' ' && codepoint == u32::from(expected_key));
    }

    if modifiers == 5 {
        return control_key_candidates(expected_key).is_some_and(|candidates| {
            candidates
                .into_iter()
                .any(|candidate| codepoint == candidate)
        });
    }

    false
}

fn control_key_candidates(expected_key: u8) -> Option<[u32; 2]> {
    match expected_key {
        0x01..=0x1a => Some([
            u32::from(expected_key + 0x40),
            u32::from(expected_key + 0x60),
        ]),
        0x1b..=0x1f => Some([
            u32::from(expected_key + 0x40),
            u32::from(expected_key + 0x40),
        ]),
        _ => None,
    }
}

fn current_winsize(fd: RawFd) -> Option<Winsize> {
    let mut ws: Winsize = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) };
    if ret == 0 && ws.ws_row > 0 && ws.ws_col > 0 {
        Some(ws)
    } else {
        None
    }
}

impl Drop for PtyProxy {
    fn drop(&mut self) {
        if self.terminal_active {
            let _ = write_all_fd(
                libc::STDOUT_FILENO,
                terminal_restore_escape(self.screen.alternate_screen_active()),
            );
        }
        self.restore_terminal();
    }
}

/// Put the terminal into raw mode, returning the saved settings.
fn set_terminal_raw() -> Option<nix::sys::termios::Termios> {
    use nix::sys::termios;

    let stdin_fd = std::io::stdin();

    let original = match termios::tcgetattr(&stdin_fd) {
        Ok(t) => t,
        Err(_) => return None, // Not a terminal
    };

    let mut raw = original.clone();
    termios::cfmakeraw(&mut raw);

    if let Err(e) = termios::tcsetattr(&stdin_fd, termios::SetArg::TCSANOW, &raw) {
        warn!("Failed to set raw terminal mode: {}", e);
        return None;
    }

    Some(original)
}

fn leave_child_screen(in_alt_screen: bool) {
    let esc = if in_alt_screen {
        terminal_restore_escape(false)
    } else {
        TERMINAL_RESTORE_NORMAL
    };
    let _ = write_all_fd(libc::STDOUT_FILENO, esc);
    drain_terminal_output(libc::STDOUT_FILENO);
}

fn prepare_parent_output_area() {
    let _ = write_all_fd(libc::STDOUT_FILENO, CLEAR_PARENT_OUTPUT_AREA);
    drain_terminal_output(libc::STDOUT_FILENO);
}

pub(crate) fn terminal_restore_escape(clear_screen: bool) -> &'static [u8] {
    if clear_screen {
        TERMINAL_RESTORE_AND_CLEAR_ESCAPE
    } else {
        TERMINAL_RESTORE_ESCAPE
    }
}

fn drain_terminal_output(fd: RawFd) {
    // SAFETY: `isatty` only inspects the borrowed fd and does not take ownership.
    if unsafe { libc::isatty(fd) } != 1 {
        return;
    }

    loop {
        // SAFETY: `tcdrain` waits for queued terminal output on the borrowed fd.
        let ret = unsafe { libc::tcdrain(fd) };
        if ret == 0 {
            break;
        }
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::Interrupted {
            debug!("PTY proxy: terminal output drain failed: {}", err);
            break;
        }
    }
}

/// Wait briefly for, then consume, a late cursor-position reply on `fd`.
///
/// On final teardown an exiting TUI child may have just sent the terminal a
/// cursor-position query (`ESC[6n`); the terminal's reply (`ESC[<row>;<col>R`)
/// lands only after the child is gone, so nothing consumes it and it gets
/// pasted into the next shell prompt (e.g. `3;1R`).
///
/// Reads one byte at a time up to `max_wait`, matching the CPR grammar and
/// stopping at the terminator or the first byte that cannot belong to a reply —
/// so queued type-ahead (which fails the grammar) is left for the shell rather
/// than scanned for a stray `R`. Byte-at-a-time polling (not a single
/// `tcflush`) lets a reply that arrives fragmented over a slow link be waited
/// out without leaking its tail.
///
/// MUST run while the terminal is still in raw mode, so the reply — which has no
/// trailing newline — is readable. No-op when `fd` is not a terminal.
fn discard_late_terminal_input(fd: RawFd, max_wait: Duration) {
    // SAFETY: `isatty` only inspects the borrowed fd and does not take ownership.
    if unsafe { libc::isatty(fd) } != 1 {
        return;
    }
    // Make sure the forwarded query has actually been transmitted so the
    // terminal has a chance to reply before we wait for it.
    drain_terminal_output(fd);

    let deadline = Instant::now() + max_wait;
    let mut parse = CprReplyParse::NeedEsc;
    loop {
        let remaining = match deadline.checked_duration_since(Instant::now()) {
            Some(d) if !d.is_zero() => d,
            _ => break,
        };
        let timeout_ms = remaining.as_millis().min(i32::MAX as u128) as i32;
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: single-fd poll over the borrowed terminal fd.
        let ready = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if ready <= 0 || (pfd.revents & libc::POLLIN) == 0 {
            // Timed out, errored, or woke for a non-readable event (e.g. HUP):
            // there is nothing (more) to consume.
            break;
        }

        let mut byte = [0u8; 1];
        // SAFETY: reads a single byte into a stack buffer we own; `fd` is live.
        let n = unsafe { libc::read(fd, byte.as_mut_ptr().cast::<libc::c_void>(), 1) };
        match n {
            // Keep reading while the byte extends a reply; stop once it is fully
            // drained or the byte is type-ahead (leaving the rest for the shell).
            1 if parse.step(byte[0]) => continue,
            1 => break,
            // EOF: nothing left to read.
            0 => break,
            // Negative: retry on EINTR, otherwise give up.
            _ => {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
        }
    }
}

fn write_all_fd(fd: RawFd, mut bytes: &[u8]) -> std::io::Result<()> {
    while !bytes.is_empty() {
        let written =
            unsafe { libc::write(fd, bytes.as_ptr().cast::<libc::c_void>(), bytes.len()) };
        if written > 0 {
            bytes = &bytes[written as usize..];
            continue;
        }

        let err = std::io::Error::last_os_error();
        match err.kind() {
            std::io::ErrorKind::Interrupted => continue,
            std::io::ErrorKind::WouldBlock => wait_for_fd_writable(fd)?,
            _ => return Err(err),
        }
    }

    Ok(())
}

fn read_fd_once(fd: RawFd, buf: &mut [u8]) -> std::io::Result<ReadFdOutcome> {
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast::<libc::c_void>(), buf.len()) };
        if n > 0 {
            return Ok(ReadFdOutcome::Data(n as usize));
        }
        if n == 0 {
            return Ok(ReadFdOutcome::Eof);
        }

        let err = std::io::Error::last_os_error();
        match err.kind() {
            std::io::ErrorKind::Interrupted => continue,
            std::io::ErrorKind::WouldBlock => return Ok(ReadFdOutcome::Retry),
            _ => return Err(err),
        }
    }
}

fn wait_for_fd_writable(fd: RawFd) -> std::io::Result<()> {
    loop {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        let ret = unsafe { libc::poll(&mut pfd, 1, -1) };
        if ret > 0 {
            if pfd.revents & libc::POLLOUT != 0 {
                return Ok(());
            }
            if pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                return Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe));
            }
            continue;
        }
        if ret == 0 {
            continue;
        }

        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        return Err(err);
    }
}

#[cfg(test)]
mod tests {
    use super::{PtyProxy, ScreenState};
    use nix::pty::openpty;
    use std::collections::VecDeque;

    fn test_proxy() -> PtyProxy {
        let pair = openpty(None, None).expect("openpty");
        PtyProxy {
            master: pair.master,
            stdin_active: true,
            terminal_active: false,
            saved_termios: None,
            scrollback: VecDeque::new(),
            screen: ScreenState::new(24, 80),
            pending_key_escape: Vec::new(),
            suspension_requested: false,
        }
    }

    #[test]
    fn screen_plaintext_includes_raw_scrollback_for_diagnostics() {
        let mut proxy = test_proxy();
        proxy.record_output(b"permission denied\r\n");
        assert!(proxy.screen_plaintext().contains("permission denied"));
    }

    #[test]
    fn raw_ctrl_z_requests_suspension_without_forwarding() {
        let mut proxy = test_proxy();
        assert!(proxy.filter_client_input(&[0x1a]).is_empty());
        assert!(proxy.take_suspension_request());
    }

    #[test]
    fn enhanced_ctrl_z_requests_suspension_without_forwarding() {
        let mut proxy = test_proxy();
        assert!(proxy.filter_client_input(b"\x1b[122;5u").is_empty());
        assert!(proxy.take_suspension_request());
    }
}
