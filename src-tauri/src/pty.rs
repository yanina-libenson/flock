use crate::error::{AppError, AppResult};
use base64::Engine;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use serde::Serialize;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tauri::{AppHandle, Emitter};

/// Dedicated tmux socket name — isolates Flock's sessions from any tmux the
/// user runs in Terminal.app. All tmux invocations share this socket + config.
const TMUX_SOCKET: &str = "flock";

/// tmux config Flock ships. Rewritten on every launch so edits by the user
/// don't accumulate drift. Mouse on is the big one — without it scroll wheel
/// events get swallowed by claude. The 50k history limit is so you can scroll
/// back through a long conversation; tmux defaults to a stingy 2000.
const TMUX_CONF: &str = "\
# Managed by Flock. Do not edit — regenerated on each launch.

set -g mouse on
set -g history-limit 50000
set -g default-terminal \"xterm-256color\"
set -ag terminal-overrides \",xterm-256color:RGB\"
set -g escape-time 10
set -g status off
# Forward window focus events to the program (Claude Code uses these for
# cursor blink behavior and pause-on-blur).
set -g focus-events on
# Emit OSC 52 on copy-mode yank so the xterm.js OSC 52 handler can forward
# the selection to the system clipboard.
set -g set-clipboard on
";

fn data_dir() -> AppResult<PathBuf> {
    let dir = dirs::data_local_dir()
        .ok_or_else(|| AppError::msg("no data local dir"))?
        .join("Flock");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn tmux_config_path() -> AppResult<PathBuf> {
    Ok(data_dir()?.join("tmux.conf"))
}

/// Write the tmux config to disk. Idempotent; called on app startup. Also
/// best-effort sources the file into the live tmux server if one is already
/// running — `-f` is only honored at server start, so without this a config
/// edit would only take effect after `tmux kill-server`.
pub fn ensure_tmux_config() -> AppResult<PathBuf> {
    let path = tmux_config_path()?;
    std::fs::write(&path, TMUX_CONF)?;

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
    let cmd = format!(
        "tmux -L {socket} source-file {conf} 2>/dev/null || true",
        socket = shell_escape(TMUX_SOCKET),
        conf = shell_escape(&path.to_string_lossy()),
    );
    let _ = std::process::Command::new(shell)
        .args(["-i", "-l", "-c", &cmd])
        .output();

    Ok(path)
}

/// A single attached PTY. There's at most one per worktree; it's a *client*
/// attached to a tmux session named `flock-<worktree_id>`. The tmux server
/// owns the real terminal state and outlives Flock.
struct Attach {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    /// Set true when this attach is being replaced by a new one for the same
    /// worktree — tells the reader thread to skip the `pty:exit` emit on its
    /// way out so a freshly-mounted pane doesn't swallow the old attach's
    /// exit and flip to "exited" while its own PTY is live.
    suppress_exit: Arc<AtomicBool>,
}

pub struct PtyManager {
    /// Keyed by worktree_id. Simpler than a separate session id since tmux
    /// already gives us persistence — one tmux session per worktree.
    attaches: Arc<Mutex<HashMap<i64, Attach>>>,
}

#[derive(Serialize, Clone)]
pub struct PtyOutput {
    pub worktree_id: i64,
    pub b64: String,
}

#[derive(Serialize, Clone)]
pub struct PtyExit {
    pub worktree_id: i64,
}

pub fn tmux_session_name(worktree_id: i64) -> String {
    format!("flock-{worktree_id}")
}

impl PtyManager {
    pub fn new() -> Self {
        Self {
            attaches: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Spawn a PTY whose child attaches xterm to the worktree's tmux session.
    /// `new-session -A` = attach if exists, else create + run `claude`. `-D`
    /// kicks any stale client from a prior Flock run. Using `-L flock` pins
    /// us to our dedicated tmux server and `-f <conf>` seeds it with our
    /// mouse / history / RGB config on first launch.
    pub fn attach(
        &self,
        app: &AppHandle,
        worktree_id: i64,
        cwd: &Path,
        cols: u16,
        rows: u16,
        permission_mode: &str,
    ) -> AppResult<()> {
        // Evict any prior attach for this worktree. `kill()` marks the old
        // attach silent so its reader thread's tail `pty:exit` emit is
        // skipped — otherwise the newly-mounted pane, filtering by
        // worktree_id, would receive the old exit and flip to "exited"
        // while its own PTY is live.
        self.kill(worktree_id).ok();

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| AppError::Pty(format!("openpty: {e}")))?;

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
        let session_name = tmux_session_name(worktree_id);
        let cwd_str = cwd.to_string_lossy();
        let conf_path = tmux_config_path()?;

        // Run through the user's interactive login shell so ~/.zshrc runs
        // and PATH picks up brew-installed tmux plus the user's `claude`.
        //
        // `--permission-mode` is appended only when not "default" — passing
        // the literal string `default` is equivalent but noisier in `ps`.
        // tmux `new-session -A` is sticky: the flag we choose now is the
        // flag claude lives with until the session is killed. The frontend
        // toggle handles that by calling kill() before re-attaching.
        let claude_cmd = if permission_mode == "default" || permission_mode.is_empty() {
            "claude".to_string()
        } else {
            format!(
                "claude --permission-mode {}",
                shell_escape(permission_mode)
            )
        };
        let tmux_cmd = format!(
            "exec tmux -L {socket} -f {conf} new-session -A -D -s {name} -c {cwd} {claude}",
            socket = shell_escape(TMUX_SOCKET),
            conf = shell_escape(&conf_path.to_string_lossy()),
            name = shell_escape(&session_name),
            cwd = shell_escape(&cwd_str),
            claude = claude_cmd,
        );

        let mut cmd = CommandBuilder::new(&shell);
        cmd.arg("-i");
        cmd.arg("-l");
        cmd.arg("-c");
        cmd.arg(&tmux_cmd);
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| AppError::Pty(format!("spawn: {e}")))?;

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| AppError::Pty(format!("clone reader: {e}")))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| AppError::Pty(format!("take writer: {e}")))?;

        let suppress_exit = Arc::new(AtomicBool::new(false));
        {
            let mut map = self.attaches.lock().unwrap();
            map.insert(
                worktree_id,
                Attach {
                    master: pair.master,
                    writer,
                    child,
                    suppress_exit: suppress_exit.clone(),
                },
            );
        }

        let app_r = app.clone();
        let attaches_r = self.attaches.clone();
        let suppress_exit_r = suppress_exit;
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            let engine = base64::engine::general_purpose::STANDARD;
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let b64 = engine.encode(&buf[..n]);
                        let _ = app_r.emit(
                            "pty:output",
                            PtyOutput {
                                worktree_id,
                                b64,
                            },
                        );
                    }
                    Err(_) => break,
                }
            }
            // Reader drained → tmux client exited. Cleanup does NOT kill the
            // tmux *server*: the session stays alive for the next attach.
            {
                let mut map = attaches_r.lock().unwrap();
                if let Some(mut a) = map.remove(&worktree_id) {
                    let _ = a.child.kill();
                }
            }
            if !suppress_exit_r.load(Ordering::Relaxed) {
                let _ = app_r.emit("pty:exit", PtyExit { worktree_id });
            }
        });

        Ok(())
    }

    pub fn write(&self, worktree_id: i64, bytes: &[u8]) -> AppResult<()> {
        let mut map = self.attaches.lock().unwrap();
        let a = map
            .get_mut(&worktree_id)
            .ok_or_else(|| AppError::Pty(format!("no attach for worktree {worktree_id}")))?;
        a.writer
            .write_all(bytes)
            .map_err(|e| AppError::Pty(format!("write: {e}")))?;
        a.writer
            .flush()
            .map_err(|e| AppError::Pty(format!("flush: {e}")))?;
        Ok(())
    }

    pub fn resize(&self, worktree_id: i64, cols: u16, rows: u16) -> AppResult<()> {
        let map = self.attaches.lock().unwrap();
        let a = map
            .get(&worktree_id)
            .ok_or_else(|| AppError::Pty(format!("no attach for worktree {worktree_id}")))?;
        a.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| AppError::Pty(format!("resize: {e}")))?;
        Ok(())
    }

    pub fn kill(&self, worktree_id: i64) -> AppResult<()> {
        let mut map = self.attaches.lock().unwrap();
        if let Some(mut a) = map.remove(&worktree_id) {
            // Intentional shutdown (worktree removal, pane close) — suppress
            // the reader thread's tail `pty:exit` emit. Only *natural* child
            // exits (claude crashed, tmux detach) should surface as pty:exit,
            // because that's the signal a live pane actually wants to react
            // to by flipping its status to "exited".
            a.suppress_exit.store(true, Ordering::Relaxed);
            let _ = a.child.kill();
        }
        Ok(())
    }
}

impl Default for PtyManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Minimal POSIX shell quoting — wrap in single quotes and escape any embedded
/// single quotes. Good enough for our tmux invocations (session names,
/// absolute paths, socket names).
fn shell_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Kill a Flock tmux session by worktree id. Called when a worktree is
/// removed from the UI. Goes through the login shell (PATH) and targets our
/// dedicated socket.
pub fn tmux_kill_session(worktree_id: i64) {
    let name = tmux_session_name(worktree_id);
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
    let cmd = format!(
        "tmux -L {socket} kill-session -t {name}",
        socket = shell_escape(TMUX_SOCKET),
        name = shell_escape(&name),
    );
    let _ = std::process::Command::new(shell)
        .args(["-i", "-l", "-c", &cmd])
        .output();
}

/// Absolute path to the `tmux` binary, resolved once via the login shell
/// (macOS GUI apps launch with a minimal PATH that misses `/opt/homebrew/bin`;
/// the user's shell rc fixes that). Cached so the status monitor — which polls
/// every couple seconds — can invoke tmux directly without paying the
/// interactive-shell startup cost on every call. tmux itself needs no special
/// env to list sessions or capture panes; only spawning `claude` does.
fn tmux_bin() -> Option<&'static Path> {
    static BIN: OnceLock<Option<PathBuf>> = OnceLock::new();
    BIN.get_or_init(|| {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
        let out = std::process::Command::new(shell)
            .args(["-i", "-l", "-c", "command -v tmux"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if p.is_empty() {
            None
        } else {
            Some(PathBuf::from(p))
        }
    })
    .as_deref()
}

/// Worktree ids of every live Flock-owned tmux session on our dedicated socket.
/// Returns empty when tmux is missing or no server is running.
pub fn tmux_list_sessions() -> Vec<i64> {
    let Some(bin) = tmux_bin() else {
        return Vec::new();
    };
    let out = std::process::Command::new(bin)
        .args(["-L", TMUX_SOCKET, "list-sessions", "-F", "#{session_name}"])
        .output();
    let Ok(out) = out else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix("flock-"))
        .filter_map(|s| s.parse::<i64>().ok())
        .collect()
}

/// Capture the rendered screen of a worktree's tmux session, or None if the
/// session is gone. `-p` prints the visible pane as plain text — the actual
/// rendered cells with no escape sequences — which is exactly what the
/// needs-input detector parses.
pub fn tmux_capture_pane(worktree_id: i64) -> Option<String> {
    let bin = tmux_bin()?;
    let name = tmux_session_name(worktree_id);
    let out = std::process::Command::new(bin)
        .args(["-L", TMUX_SOCKET, "capture-pane", "-t", &name, "-p"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Does `tmux` exist on the user's PATH? We invoke via the login shell
/// because macOS launches GUI apps with a minimal PATH that doesn't include
/// `/opt/homebrew/bin`; the user's shell rc fixes that.
pub fn tmux_available() -> bool {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
    std::process::Command::new(shell)
        .args(["-i", "-l", "-c", "command -v tmux"])
        .output()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false)
}
