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
# Pass OSC 8 hyperlinks through to xterm.js — without this tmux strips them and
# links whose visible text differs from the URL render as plain (unclickable)
# text. xterm.js supports OSC 8 (we set a linkHandler), so advertise it.
set -as terminal-features \",xterm-256color:hyperlinks\"
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
        env_vars: &[(String, String)],
        initial_prompt: Option<&str>,
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
        // `--permission-mode`, env vars, and any initial prompt are all baked
        // into the claude invocation at session creation. tmux `new-session -A`
        // is sticky: these are the flags claude lives with until the session is
        // killed (the frontend toggle handles that by killing before
        // re-attaching). On *re-attach* to an existing session the prompt arg
        // is moot — claude is already running.
        //
        // Crash resilience: when the tmux session is gone (server killed under
        // memory pressure, machine reboot, OOM), `new-session -A` creates a
        // *fresh* session — which would start claude empty and silently drop
        // the conversation. Flock keeps no on-disk copy of the live session, so
        // instead we resume Claude Code's own transcript: if a prior session
        // file exists for this worktree's cwd and we're not seeding a brand-new
        // task prompt, bake in `--resume <id>` so the reopened pane continues
        // where it left off. When the session is still live this is moot (the
        // claude arg is ignored on attach).
        let resume_id = if initial_prompt.is_none() && !tmux_list_sessions().contains(&worktree_id) {
            latest_session_id(cwd)
        } else {
            None
        };
        let claude = claude_invocation(permission_mode, initial_prompt, resume_id.as_deref());
        let env_flags = build_env_flags(env_vars);
        let session_cmd = session_command(&claude, &shell);
        let tmux_cmd = format!(
            "exec tmux -L {socket} -f {conf} new-session -A -D{env_flags} -s {name} -c {cwd} {session_cmd}",
            socket = shell_escape(TMUX_SOCKET),
            conf = shell_escape(&conf_path.to_string_lossy()),
            name = shell_escape(&session_name),
            cwd = shell_escape(&cwd_str),
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

/// Build the `claude` invocation: base command + optional `--permission-mode`
/// + optional `--resume <id>` (continue a prior on-disk conversation) + optional
/// initial prompt (passed as a positional argument, which Claude Code runs as
/// the session's first turn). All user-supplied parts are shell-escaped because
/// the result is embedded in a `sh -c` string. `resume_id` and `initial_prompt`
/// are mutually exclusive in practice — resume continues an existing session
/// (no new first turn), seeding a prompt starts a fresh task.
fn claude_invocation(
    permission_mode: &str,
    initial_prompt: Option<&str>,
    resume_id: Option<&str>,
) -> String {
    let mut cmd = if permission_mode == "default" || permission_mode.is_empty() {
        "claude".to_string()
    } else {
        format!("claude --permission-mode {}", shell_escape(permission_mode))
    };
    if let Some(id) = resume_id {
        if !id.is_empty() {
            cmd = format!("{cmd} --resume {}", shell_escape(id));
        }
    }
    if let Some(p) = initial_prompt {
        if !p.is_empty() {
            cmd = format!("{cmd} {}", shell_escape(p));
        }
    }
    cmd
}

/// Wrap the `claude` command as the tmux session's shell-command so that when
/// claude exits, the pane **falls back to an interactive login shell** in the
/// same worktree dir instead of the session dying. Without this, `claude` is
/// the session's root process — exiting it ends the session and the pane goes
/// dead, leaving nowhere to run shell commands (or `claude --resume`). With it,
/// `/exit` drops you to a normal prompt, exactly like running claude inside a
/// terminal.
///
/// The result is shell-escaped to a single token: tmux receives it verbatim as
/// the shell-command and runs `sh -c "<claude>; exec <shell> -i -l"`.
fn session_command(claude: &str, shell: &str) -> String {
    let inner = format!("{claude}; exec {} -i -l", shell_escape(shell));
    shell_escape(&inner)
}

/// The most recent Claude Code session id for a worktree's cwd, or None if it
/// has never run claude there. The transcript filename stem *is* the session id
/// (`<id>.jsonl`), so this is what `claude --resume <id>` expects. Reuses the
/// transcript module's cwd→slug + newest-file logic so the Reader view and the
/// resume-on-reattach path agree on which session is "current".
fn latest_session_id(cwd: &Path) -> Option<String> {
    let file = crate::transcript::session_file_for(&cwd.to_string_lossy())?;
    file.file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
}

/// `-e KEY=VAL` flags for per-environment vars (see env_profiles). Each pair is
/// shell-escaped as a unit.
fn build_env_flags(env_vars: &[(String, String)]) -> String {
    env_vars
        .iter()
        .map(|(k, v)| format!(" -e {}", shell_escape(&format!("{k}={v}"))))
        .collect()
}

/// Start a worktree's claude session **detached** (no PTY client), optionally
/// seeding an initial prompt. Used by the orchestration path so a task can be
/// spawned headlessly (cron, MCP, REST); a viewer reattaches later via
/// `attach`, which reconnects to this live tmux session rather than restarting
/// claude.
pub fn start_detached(
    worktree_id: i64,
    cwd: &Path,
    permission_mode: &str,
    env_vars: &[(String, String)],
    initial_prompt: Option<&str>,
) -> AppResult<()> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
    let session_name = tmux_session_name(worktree_id);
    let conf_path = tmux_config_path()?;
    let claude = claude_invocation(permission_mode, initial_prompt, None);
    let env_flags = build_env_flags(env_vars);
    let session_cmd = session_command(&claude, &shell);
    let tmux_cmd = format!(
        "tmux -L {socket} -f {conf} new-session -d{env_flags} -s {name} -c {cwd} {session_cmd}",
        socket = shell_escape(TMUX_SOCKET),
        conf = shell_escape(&conf_path.to_string_lossy()),
        name = shell_escape(&session_name),
        cwd = shell_escape(&cwd.to_string_lossy()),
    );
    let out = std::process::Command::new(shell)
        .args(["-i", "-l", "-c", &tmux_cmd])
        .output()
        .map_err(|e| AppError::Pty(format!("spawn detached: {e}")))?;
    if !out.status.success() {
        return Err(AppError::Pty(format!(
            "tmux new-session failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
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

/// Total resident memory (KB) of each Flock session's process subtree, keyed by
/// worktree id. The subtree root is the tmux pane's pid (the `sh -c "claude;
/// exec shell"` wrapper); we sum it plus every descendant — the `claude` process
/// and its MCP/node children, which is where the memory actually lives. Returns
/// empty when tmux is missing or no server is running. Two shells per call
/// (`tmux list-panes` + a full `ps`), so the caller (the monitor's memory-budget
/// reaper) runs it on a slow cadence, not the 2s status poll.
pub fn session_rss_kb() -> HashMap<i64, u64> {
    let mut out = HashMap::new();
    let Some(bin) = tmux_bin() else {
        return out;
    };
    // worktree id -> pane root pid
    let panes = std::process::Command::new(bin)
        .args([
            "-L",
            TMUX_SOCKET,
            "list-panes",
            "-a",
            "-F",
            "#{session_name} #{pane_pid}",
        ])
        .output();
    let Ok(panes) = panes else {
        return out;
    };
    if !panes.status.success() {
        return out;
    }
    let roots: Vec<(i64, i32)> = String::from_utf8_lossy(&panes.stdout)
        .lines()
        .filter_map(|l| {
            let (name, pid) = l.split_once(' ')?;
            let id = name.strip_prefix("flock-")?.parse::<i64>().ok()?;
            let pid = pid.trim().parse::<i32>().ok()?;
            Some((id, pid))
        })
        .collect();
    if roots.is_empty() {
        return out;
    }

    // Whole process table: pid, ppid, rss(KB). Build per-pid RSS + a ppid->kids
    // adjacency so each root's subtree can be summed.
    let ps = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,rss="])
        .output();
    let Ok(ps) = ps else {
        return out;
    };
    let mut rss: HashMap<i32, u64> = HashMap::new();
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    for line in String::from_utf8_lossy(&ps.stdout).lines() {
        let mut it = line.split_whitespace();
        let (Some(pid), Some(ppid), Some(r)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        let (Ok(pid), Ok(ppid), Ok(r)) =
            (pid.parse::<i32>(), ppid.parse::<i32>(), r.parse::<u64>())
        else {
            continue;
        };
        rss.insert(pid, r);
        children.entry(ppid).or_default().push(pid);
    }

    // Iterative DFS per root (process trees are shallow, but a visited guard
    // keeps a pathological table from looping). Reparented descendants whose
    // ppid no longer points into the subtree are missed — acceptable; the heavy
    // memory is `claude` itself, a direct child of the pane.
    for (id, root) in roots {
        let mut total: u64 = 0;
        let mut seen: std::collections::HashSet<i32> = std::collections::HashSet::new();
        let mut stack = vec![root];
        while let Some(pid) = stack.pop() {
            if !seen.insert(pid) {
                continue;
            }
            total += rss.get(&pid).copied().unwrap_or(0);
            if let Some(kids) = children.get(&pid) {
                stack.extend(kids.iter().copied());
            }
        }
        out.insert(id, total);
    }
    out
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

/// Reflow a worktree's tmux window to an explicit size. Sets `window-size
/// manual` so the resize sticks even while a differently-sized client is
/// attached (without it, tmux snaps the window back to the attached client).
/// This is the "active viewer claims the size" primitive: the phone calls it
/// with its narrow size on open, the desktop re-claims its full size when its
/// pane becomes active. Last caller wins.
pub fn tmux_resize_window(worktree_id: i64, cols: u16, rows: u16) -> bool {
    let Some(bin) = tmux_bin() else {
        return false;
    };
    let name = tmux_session_name(worktree_id);
    let cols = cols.max(1).to_string();
    let rows = rows.max(1).to_string();
    std::process::Command::new(bin)
        .args([
            "-L",
            TMUX_SOCKET,
            "set-option",
            "-t",
            name.as_str(),
            "window-size",
            "manual",
            ";",
            "resize-window",
            "-t",
            name.as_str(),
            "-x",
            &cols,
            "-y",
            &rows,
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Like `tmux_capture_pane` but keeps escape sequences (`-e`) so colors and
/// attributes survive — used to paint the live terminal in the PWA via
/// xterm.js. The monitor uses the plain (escape-stripped) variant instead,
/// since its text matching would choke on raw ANSI.
pub fn tmux_capture_pane_ansi(worktree_id: i64) -> Option<String> {
    let bin = tmux_bin()?;
    let name = tmux_session_name(worktree_id);
    // `-S -50`: include ~50 lines of scrollback above the visible screen so the
    // PWA can scroll back through recent context. Kept modest on purpose:
    // older lines were printed at the desktop's wider width and can't re-wrap
    // (terminal scrollback is fixed-width), so deep history reads jagged on a
    // narrow phone. Recent lines are more likely at the current narrow width.
    let out = std::process::Command::new(bin)
        .args([
            "-L", TMUX_SOCKET, "capture-pane", "-t", &name, "-e", "-p", "-S", "-50",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Send input to a worktree's tmux session. Literal text goes through
/// `send-keys -l` (typed verbatim); otherwise `payload` is a tmux key name
/// (`Enter`, `Escape`, `C-c`, …). Goes straight to tmux (args, no shell) so
/// the text is never interpreted as a command. Returns false if tmux or the
/// session is unavailable.
pub fn tmux_send(worktree_id: i64, literal: bool, payload: &str) -> bool {
    let Some(bin) = tmux_bin() else {
        return false;
    };
    let name = tmux_session_name(worktree_id);
    let mut args: Vec<&str> = vec!["-L", TMUX_SOCKET, "send-keys", "-t", name.as_str()];
    if literal {
        args.push("-l");
        args.push("--");
    }
    args.push(payload);
    std::process::Command::new(bin)
        .args(&args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
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

#[cfg(test)]
mod tests {
    use super::claude_invocation;

    #[test]
    fn plain_attach_has_no_resume_or_prompt() {
        assert_eq!(
            claude_invocation("bypassPermissions", None, None),
            "claude --permission-mode 'bypassPermissions'"
        );
        // default mode → bare `claude`, no --permission-mode flag.
        assert_eq!(claude_invocation("default", None, None), "claude");
    }

    #[test]
    fn resume_id_is_baked_in_after_permission_mode() {
        assert_eq!(
            claude_invocation("bypassPermissions", None, Some("abc-123")),
            "claude --permission-mode 'bypassPermissions' --resume 'abc-123'"
        );
    }

    #[test]
    fn initial_prompt_is_a_trailing_positional() {
        assert_eq!(
            claude_invocation("default", Some("fix the bug"), None),
            "claude 'fix the bug'"
        );
    }

    #[test]
    fn empty_resume_id_is_ignored() {
        assert_eq!(
            claude_invocation("default", None, Some("")),
            "claude"
        );
    }

    #[test]
    fn session_command_wraps_claude_with_shell_fallback() {
        use super::session_command;
        // One escaped token (tmux gets it verbatim), and unwrapping it yields
        // `<claude>; exec <shell> -i -l` so the pane survives claude exiting.
        let tok = session_command("claude --permission-mode 'bypassPermissions'", "/bin/zsh");
        assert!(tok.starts_with('\'') && tok.ends_with('\''));
        let inner = &tok[1..tok.len() - 1].replace("'\\''", "'");
        assert_eq!(
            inner,
            "claude --permission-mode 'bypassPermissions'; exec '/bin/zsh' -i -l"
        );
    }
}
