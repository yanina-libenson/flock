use crate::error::{AppError, AppResult};
use base64::Engine;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use serde::Serialize;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
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
    #[allow(clippy::too_many_arguments)]
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
        append_system_prompt: Option<&str>,
        model: Option<&str>,
        effort: Option<&str>,
        agent: &str,
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
        // Env-profile vars (e.g. the per-folder `GH_CONFIG_DIR` that selects the
        // right `gh`/GitHub account) are baked into a tmux session only at
        // creation — `new-session -A` ignores `-e` when it attaches to an
        // existing session. So a session created before its profile was set up,
        // or before the profile changed, stays frozen with the stale/missing
        // value: most visibly, the wrong GitHub account. Detect that drift and
        // kill the session here so it's recreated below with the current env.
        // Claude resumes from its own transcript (the resume logic just after),
        // so the conversation isn't lost.
        if tmux_list_sessions().contains(&worktree_id)
            && session_env_drifted(worktree_id, env_vars)
        {
            eprintln!(
                "flock: env drift on {} — recreating session to apply the current profile",
                tmux_session_name(worktree_id)
            );
            tmux_kill_session(worktree_id);
        }

        let resume_id = if initial_prompt.is_none() && !tmux_list_sessions().contains(&worktree_id) {
            latest_session_id_for(agent, cwd, env_vars)
        } else {
            None
        };
        let mcp_entry = crate::mcp::installed_entry();
        let agent_cmd = agent_invocation(
            agent,
            permission_mode,
            initial_prompt,
            resume_id.as_deref(),
            append_system_prompt,
            model,
            effort,
            &cwd_str,
            mcp_entry.as_deref(),
        );
        let env_flags = build_env_flags(&with_worktree_id(env_vars, worktree_id));
        let session_cmd = session_command(&agent_cmd, &shell);
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
    append_system_prompt: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
) -> String {
    let mut cmd = if permission_mode == "default" || permission_mode.is_empty() {
        "claude".to_string()
    } else {
        format!("claude --permission-mode {}", shell_escape(permission_mode))
    };
    if let Some(m) = model {
        if !m.is_empty() {
            cmd = format!("{cmd} --model {}", shell_escape(m));
        }
    }
    if let Some(e) = effort {
        if !e.is_empty() {
            cmd = format!("{cmd} --effort {}", shell_escape(e));
        }
    }
    if let Some(sp) = append_system_prompt {
        if !sp.is_empty() {
            cmd = format!("{cmd} --append-system-prompt {}", shell_escape(sp));
        }
    }
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

/// The agent command for a worktree's session: `claude …` or `codex …`
/// depending on the worktree's `agent` column. `cwd` and `mcp_entry` (the
/// installed Flock MCP server, see `mcp::installed_entry`) are only used by
/// Codex; Claude gets its MCP servers from its own config.
///
/// A row keeps its model/effort across agent switches, so each agent only
/// receives values it understands: Claude never gets a Codex model id (or
/// `"default"`), Codex never gets a Claude alias. Anything else is dropped and
/// the agent falls back to its own default. `append_system_prompt` (an
/// orchestrator's instructions) maps to Codex's `developer_instructions`.
#[allow(clippy::too_many_arguments)]
fn agent_invocation(
    agent: &str,
    permission_mode: &str,
    initial_prompt: Option<&str>,
    resume_id: Option<&str>,
    append_system_prompt: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
    cwd: &str,
    mcp_entry: Option<&str>,
) -> String {
    let effort = effort.filter(|e| *e != "default");
    if agent == crate::db::AGENT_CODEX {
        let model = model.filter(|m| is_codex_model(m));
        codex_invocation(
            permission_mode,
            initial_prompt,
            resume_id,
            model,
            effort,
            append_system_prompt,
            cwd,
            mcp_entry,
        )
    } else {
        let model = model.filter(|m| !is_codex_model(m) && *m != "default");
        claude_invocation(
            permission_mode,
            initial_prompt,
            resume_id,
            append_system_prompt,
            model,
            effort,
        )
    }
}

/// A concrete Codex model id (`"default"` means "don't pass `-m`").
fn is_codex_model(m: &str) -> bool {
    m != "default" && crate::commands::CODEX_MODELS.contains(&m)
}

/// TOML basic-string literal for a `codex -c key=<value>` override. Escapes
/// quotes, backslashes and control characters — an orchestrator's
/// instructions span many lines, and a raw newline isn't valid TOML (Codex
/// would then take the whole value, quotes included, as a literal string).
fn toml_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Build the `codex` invocation, mapping the worktree's Claude-shaped settings
/// onto Codex's CLI (verified against codex-cli 0.160):
///
/// - **Permission mode** → sandbox + approval policy. `bypassPermissions` →
///   `--dangerously-bypass-approvals-and-sandbox`; `auto` → `--approve-for-me`
///   (automatic approval review, Codex's closest analogue); `plan` → read-only
///   sandbox; `dontAsk` → workspace-write, never ask (a disallowed action
///   fails back to the model); `default` / `acceptEdits` → workspace-write,
///   ask on request (Codex's standard preset).
/// - **Model** → `-m` when it's a Codex model id (`commands::CODEX_MODELS`);
///   omitted for `"default"` or a leftover Claude alias, so Codex runs its own
///   configured default.
/// - **Effort** → `model_reasoning_effort`. Codex accepts the same
///   low/medium/high/xhigh/max scale Flock validates against.
/// - **Developer instructions** → `developer_instructions`, Codex's
///   counterpart of `--append-system-prompt`: it adds a developer message on
///   top of Codex's own system prompt (`base_instructions` would replace it).
///   Only needed on a fresh session; a resumed one replays it from history.
///
/// Every session also gets, per invocation (never written to
/// `~/.codex/config.toml`):
/// - `--no-daemon`, so the agent runs inside the tmux pane — killing the
///   session (switch back, memory reaping) really stops it, rather than a turn
///   carrying on in Codex's shared background app-server.
/// - the worktree trusted, so a fresh worktree doesn't open on Codex's blocking
///   "Trust this folder?" prompt.
/// - the Flock MCP server (`kb_*`/`task_*` tools), forwarding
///   `FLOCK_WORKTREE_ID` so the server can identify its worktree (that's how
///   `task_create` links children and runs the cross-account check), plus the
///   server's optional `FLOCK_API_URL` / `FLOCK_TOKEN` overrides. Codex only
///   hands an MCP server the env vars it's told to; Claude hands it all.
///
/// Resume is `codex resume <id>`; a prompt goes after `--` so text starting
/// with `-` can't be read as a flag.
#[allow(clippy::too_many_arguments)]
fn codex_invocation(
    permission_mode: &str,
    initial_prompt: Option<&str>,
    resume_id: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
    developer_instructions: Option<&str>,
    cwd: &str,
    mcp_entry: Option<&str>,
) -> String {
    let resume_id = resume_id.filter(|id| !id.is_empty());
    let mut cmd = if resume_id.is_some() {
        "codex resume --no-daemon".to_string()
    } else {
        "codex --no-daemon".to_string()
    };
    let perm = match permission_mode {
        "bypassPermissions" => "--dangerously-bypass-approvals-and-sandbox",
        "auto" => "--approve-for-me",
        "plan" => "-s read-only -a on-request",
        "dontAsk" => "-s workspace-write -a never",
        _ => "-s workspace-write -a on-request",
    };
    cmd = format!("{cmd} {perm}");
    if let Some(m) = model.filter(|m| !m.is_empty()) {
        cmd = format!("{cmd} -m {}", shell_escape(m));
    }
    if let Some(e) = effort.filter(|e| !e.is_empty()) {
        let kv = format!("model_reasoning_effort={}", toml_str(e));
        cmd = format!("{cmd} -c {}", shell_escape(&kv));
    }
    if let Some(di) = developer_instructions.filter(|d| !d.is_empty()) {
        let kv = format!("developer_instructions={}", toml_str(di));
        cmd = format!("{cmd} -c {}", shell_escape(&kv));
    }
    let trust = format!("projects={{{}={{trust_level=\"trusted\"}}}}", toml_str(cwd));
    cmd = format!("{cmd} -c {}", shell_escape(&trust));
    if let Some(entry) = mcp_entry.filter(|e| !e.is_empty()) {
        for kv in [
            "mcp_servers.flock.command=\"node\"".to_string(),
            format!("mcp_servers.flock.args=[{}]", toml_str(entry)),
            "mcp_servers.flock.env_vars=[\"FLOCK_WORKTREE_ID\",\"FLOCK_API_URL\",\"FLOCK_TOKEN\"]"
                .to_string(),
        ] {
            cmd = format!("{cmd} -c {}", shell_escape(&kv));
        }
    }
    if let Some(id) = resume_id {
        cmd = format!("{cmd} {}", shell_escape(id));
    }
    if let Some(p) = initial_prompt.filter(|p| !p.is_empty()) {
        cmd = format!("{cmd} -- {}", shell_escape(p));
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
/// resume-on-reattach path agree on which session is "current". None doubles as
/// the "no resumable session" signal for the REST resume-on-input path.
/// `config_dir` is the session's `CLAUDE_CONFIG_DIR` (the transcript lives under
/// it, not always `~/.claude`).
pub fn latest_session_id(cwd: &Path, config_dir: Option<&str>) -> Option<String> {
    let file = crate::transcript::session_file_for(&cwd.to_string_lossy(), config_dir)?;
    file.file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
}

/// The most recent resumable session id for a worktree's cwd under the given
/// agent: Claude's transcript (under the session's `CLAUDE_CONFIG_DIR`) or
/// Codex's newest interactive rollout for that cwd (under `CODEX_HOME`). None
/// means "no prior session — start fresh".
pub fn latest_session_id_for(
    agent: &str,
    cwd: &Path,
    env_vars: &[(String, String)],
) -> Option<String> {
    if agent == crate::db::AGENT_CODEX {
        crate::transcript::codex_session_for(
            &cwd.to_string_lossy(),
            crate::transcript::codex_home_from_env(env_vars),
        )
        .map(|(_, id)| id)
    } else {
        latest_session_id(cwd, crate::transcript::config_dir_from_env(env_vars))
    }
}

/// Every Flock session gets `FLOCK_WORKTREE_ID` injected so the process (and
/// any MCP server it spawns) can self-identify — e.g. the Flock MCP reads it to
/// tag tasks it spawns with their parent worktree. Appended to the profile env
/// vars without disturbing them (drift detection still keys off the profile set).
fn with_worktree_id(env_vars: &[(String, String)], worktree_id: i64) -> Vec<(String, String)> {
    let mut all = env_vars.to_vec();
    all.push(("FLOCK_WORKTREE_ID".to_string(), worktree_id.to_string()));
    all
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
/// seeding an initial prompt or resuming a prior on-disk session. Used by the
/// orchestration path so a task can be spawned headlessly (cron, MCP, REST); a
/// viewer reattaches later via `attach`, which reconnects to this live tmux
/// session rather than restarting claude.
///
/// `resume_id` bakes in `--resume <id>` so a session whose tmux died (monitor
/// hibernation, memory reaping, reboot) can be brought back from Claude Code's
/// transcript without losing the conversation — the headless counterpart to
/// `attach`'s resume-on-reattach. `initial_prompt` and `resume_id` are mutually
/// exclusive in practice (seed a new task vs. continue an existing one).
#[allow(clippy::too_many_arguments)]
pub fn start_detached(
    worktree_id: i64,
    cwd: &Path,
    permission_mode: &str,
    env_vars: &[(String, String)],
    initial_prompt: Option<&str>,
    append_system_prompt: Option<&str>,
    resume_id: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
    agent: &str,
) -> AppResult<()> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
    let conf_path = tmux_config_path()?;
    let mcp_entry = crate::mcp::installed_entry();
    let tmux_cmd = detached_tmux_cmd(
        worktree_id,
        &cwd.to_string_lossy(),
        permission_mode,
        env_vars,
        initial_prompt,
        append_system_prompt,
        resume_id,
        &shell,
        &conf_path.to_string_lossy(),
        model,
        effort,
        agent,
        mcp_entry.as_deref(),
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

/// Build the detached `tmux new-session -d …` command string. Pure (no env / IO)
/// so the resume + prompt wiring can be asserted in tests.
#[allow(clippy::too_many_arguments)]
fn detached_tmux_cmd(
    worktree_id: i64,
    cwd: &str,
    permission_mode: &str,
    env_vars: &[(String, String)],
    initial_prompt: Option<&str>,
    append_system_prompt: Option<&str>,
    resume_id: Option<&str>,
    shell: &str,
    conf_path: &str,
    model: Option<&str>,
    effort: Option<&str>,
    agent: &str,
    mcp_entry: Option<&str>,
) -> String {
    let session_name = tmux_session_name(worktree_id);
    let agent_cmd = agent_invocation(
        agent,
        permission_mode,
        initial_prompt,
        resume_id,
        append_system_prompt,
        model,
        effort,
        cwd,
        mcp_entry,
    );
    let env_flags = build_env_flags(&with_worktree_id(env_vars, worktree_id));
    let session_cmd = session_command(&agent_cmd, shell);
    format!(
        "tmux -L {socket} -f {conf} new-session -d{env_flags} -s {name} -c {cwd} {session_cmd}",
        socket = shell_escape(TMUX_SOCKET),
        conf = shell_escape(conf_path),
        name = shell_escape(&session_name),
        cwd = shell_escape(cwd),
    )
}

/// U+00A0 — the NBSP Claude renders after `❯` on its idle input line. Same
/// discriminator the monitor keys off (see `monitor::PROMPT_NBSP`); duplicated
/// here as a one-liner to keep pty ↔ monitor decoupled.
const READY_PROMPT_NBSP: &str = "❯\u{00a0}";

/// Codex's input line: `›` + a space at the start of a screen line (the
/// composer, e.g. `› Ask Codex to do anything`). Same glyph the monitor anchors
/// on (see `monitor::CODEX_PROMPT`).
const CODEX_READY_PROMPT: &str = "› ";

/// Poll a freshly-resumed session until the agent has drawn its input UI, so
/// headless input isn't typed into a still-booting TUI and silently dropped.
/// Looks for Claude's input prompt (`❯` + NBSP) or its input-box border (`╭`),
/// or Codex's `› ` composer line.
/// Returns true once ready, false on timeout — on timeout the caller sends
/// anyway (the session *is* live, so it's a best-effort late send, never a
/// 502). Polls ~4×/sec.
pub fn wait_until_ready(worktree_id: i64, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(screen) = tmux_capture_pane(worktree_id) {
            if screen.contains(READY_PROMPT_NBSP)
                || screen.contains('╭')
                || screen.lines().any(|l| l.starts_with(CODEX_READY_PROMPT))
            {
                return true;
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
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

/// True when a live tmux session for this worktree exists but its environment
/// no longer matches the env-profile vars we'd inject now (e.g. a per-folder
/// `GH_CONFIG_DIR` added or changed after the session was first created).
/// `new-session -A` bakes `-e` vars in only at creation, so without recreating
/// the session the stale value sticks. Empty `env_vars` → nothing to compare.
fn session_env_drifted(worktree_id: i64, env_vars: &[(String, String)]) -> bool {
    if env_vars.is_empty() {
        return false;
    }
    let Some(bin) = tmux_bin() else {
        return false;
    };
    let name = tmux_session_name(worktree_id);
    env_vars.iter().any(|(k, v)| {
        let out = std::process::Command::new(bin)
            .args(["-L", TMUX_SOCKET, "show-environment", "-t", &name, k])
            .output();
        match out {
            // tmux prints `KEY=value` (set) or `-KEY` (explicitly unset); a var
            // the session never received exits non-zero. Any non-match is drift.
            Ok(o) if o.status.success() => {
                !tmux_env_line_matches(k, v, &String::from_utf8_lossy(&o.stdout))
            }
            _ => true,
        }
    })
}

/// Whether tmux's `show-environment KEY` output sets `key` to exactly `value`.
fn tmux_env_line_matches(key: &str, value: &str, output: &str) -> bool {
    output.trim() == format!("{key}={value}")
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

/// Max bytes of literal text per `send-keys -l` call. DO NOT raise this or
/// collapse the chunks back into a single call: Claude Code's TUI treats any
/// single input read above ~512–900 bytes as a *paste*. One big `send-keys`
/// reaches it as ~1022-byte pty reads, so a long message either loses every
/// chunk but the last (the child sees only the tail) or becomes a
/// "[Pasted text #N]" that is submitted wrapped in `<pasted_content>` tags —
/// which the child treats as untrusted data and may refuse to act on.
/// Bracketed paste (`paste-buffer -p`) has the same wrapping problem. 256
/// bytes with a short gap was verified byte-exact on a ~2KB message; 900 was not.
const SEND_CHUNK_BYTES: usize = 256;
/// Gap between chunks so each lands as its own small read (see above).
const SEND_CHUNK_DELAY: std::time::Duration = std::time::Duration::from_millis(20);

/// Split `s` into consecutive pieces of at most `max` bytes, never cutting a
/// UTF-8 codepoint. `max` must be ≥ 4 (the longest codepoint).
fn utf8_chunks(s: &str, max: usize) -> Vec<&str> {
    assert!(max >= 4, "chunk size must fit any UTF-8 codepoint");
    let mut out = Vec::new();
    let mut rest = s;
    while !rest.is_empty() {
        let mut end = rest.len().min(max);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        let (head, tail) = rest.split_at(end);
        out.push(head);
        rest = tail;
    }
    out
}

/// Send input to a worktree's tmux session. Literal text goes through
/// `send-keys -l` (typed verbatim), in `SEND_CHUNK_BYTES` pieces when long so
/// Claude Code doesn't treat it as a paste; otherwise `payload` is a tmux key
/// name (`Enter`, `Escape`, `C-c`, …). Goes straight to tmux (args, no shell)
/// so the text is never interpreted as a command. Returns false if tmux or the
/// session is unavailable, or any chunk fails.
pub fn tmux_send(worktree_id: i64, literal: bool, payload: &str) -> bool {
    let Some(bin) = tmux_bin() else {
        return false;
    };
    let name = tmux_session_name(worktree_id);
    let send = |literal: bool, text: &str| {
        let mut args: Vec<&str> = vec!["-L", TMUX_SOCKET, "send-keys", "-t", name.as_str()];
        if literal {
            args.push("-l");
            args.push("--");
        }
        args.push(text);
        std::process::Command::new(bin)
            .args(&args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    };
    if !literal || payload.len() <= SEND_CHUNK_BYTES {
        return send(literal, payload);
    }
    for (i, chunk) in utf8_chunks(payload, SEND_CHUNK_BYTES).into_iter().enumerate() {
        if i > 0 {
            std::thread::sleep(SEND_CHUNK_DELAY);
        }
        if !send(true, chunk) {
            return false;
        }
    }
    true
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
    use super::{claude_invocation, codex_invocation, utf8_chunks, SEND_CHUNK_BYTES};

    #[test]
    fn utf8_chunks_empty_and_short() {
        assert!(utf8_chunks("", SEND_CHUNK_BYTES).is_empty());
        assert_eq!(utf8_chunks("hi — ▾", SEND_CHUNK_BYTES), vec!["hi — ▾"]);
        let exact = "a".repeat(SEND_CHUNK_BYTES);
        assert_eq!(utf8_chunks(&exact, SEND_CHUNK_BYTES), vec![exact.as_str()]);
    }

    #[test]
    fn utf8_chunks_respect_char_boundaries() {
        // Multi-byte chars (3-byte —/▾/ⓘ, 4-byte emoji) at every offset
        // relative to the chunk edge, plus newlines.
        let unit = "ab — ▾ ⓘ 🦆\nline two\n";
        for pad in 0..8 {
            let s = format!("{}{}", "x".repeat(pad), unit.repeat(60));
            for max in [4, 5, 7, 256] {
                let chunks = utf8_chunks(&s, max);
                assert_eq!(chunks.concat(), s, "pad={pad} max={max}");
                assert!(chunks.iter().all(|c| !c.is_empty() && c.len() <= max));
                if max == SEND_CHUNK_BYTES {
                    assert!(chunks.len() > 1);
                }
            }
        }
    }

    #[test]
    fn plain_attach_has_no_resume_or_prompt() {
        assert_eq!(
            claude_invocation("bypassPermissions", None, None, None, None, None),
            "claude --permission-mode 'bypassPermissions'"
        );
        // default mode → bare `claude`, no --permission-mode flag.
        assert_eq!(claude_invocation("default", None, None, None, None, None), "claude");
    }

    #[test]
    fn resume_id_is_baked_in_after_permission_mode() {
        assert_eq!(
            claude_invocation("bypassPermissions", None, Some("abc-123"), None, None, None),
            "claude --permission-mode 'bypassPermissions' --resume 'abc-123'"
        );
    }

    #[test]
    fn initial_prompt_is_a_trailing_positional() {
        assert_eq!(
            claude_invocation("default", Some("fix the bug"), None, None, None, None),
            "claude 'fix the bug'"
        );
    }

    #[test]
    fn empty_resume_id_is_ignored() {
        assert_eq!(
            claude_invocation("default", None, Some(""), None, None, None),
            "claude"
        );
    }

    #[test]
    fn append_system_prompt_after_permission_mode() {
        assert_eq!(
            claude_invocation(
                "bypassPermissions",
                Some("go"),
                None,
                Some("you orchestrate"),
                None,
                None
            ),
            "claude --permission-mode 'bypassPermissions' --append-system-prompt 'you orchestrate' 'go'"
        );
        // Empty system prompt is ignored.
        assert_eq!(
            claude_invocation("default", None, None, Some(""), None, None),
            "claude"
        );
    }

    #[test]
    fn model_and_effort_land_after_permission_mode() {
        assert_eq!(
            claude_invocation(
                "bypassPermissions",
                None,
                None,
                None,
                Some("opus"),
                Some("high")
            ),
            "claude --permission-mode 'bypassPermissions' --model 'opus' --effort 'high'"
        );
        // Omitted → no flags at all, identical to today's default behavior.
        assert_eq!(
            claude_invocation("default", None, None, None, None, None),
            "claude"
        );
        // Empty strings are ignored, same as the other optional flags.
        assert_eq!(
            claude_invocation("default", None, None, None, Some(""), Some("")),
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

    #[test]
    fn detached_cmd_bakes_resume_when_resuming() {
        use super::detached_tmux_cmd;
        // Resume path (REST resume-on-input): `--resume <id>`, no initial prompt.
        let cmd = detached_tmux_cmd(
            7,
            "/work/dir",
            "bypassPermissions",
            &[],
            None,
            None,
            Some("sess-abc"),
            "/bin/zsh",
            "/conf",
            None,
            None,
            "claude",
            None,
        );
        assert!(cmd.contains("new-session -d"));
        assert!(cmd.contains("-s 'flock-7'"));
        assert!(cmd.contains("-c '/work/dir'"));
        // The claude command is shell-escaped into one token, so `--resume` and
        // the id survive but the inner quoting differs (the exact `--resume
        // '<id>'` form is asserted in `resume_id_is_baked_in_after_permission_mode`).
        assert!(cmd.contains("--resume"));
        assert!(cmd.contains("sess-abc"));
        // Normal task-spawn path: an initial prompt, never a resume flag.
        let plain = detached_tmux_cmd(
            7,
            "/work/dir",
            "bypassPermissions",
            &[],
            Some("do it"),
            None,
            None,
            "/bin/zsh",
            "/conf",
            None,
            None,
            "claude",
            None,
        );
        assert!(!plain.contains("--resume"));
        assert!(plain.contains("'do it'"));
    }

    #[test]
    fn detached_cmd_bakes_model_and_effort() {
        use super::detached_tmux_cmd;
        let cmd = detached_tmux_cmd(
            7,
            "/work/dir",
            "bypassPermissions",
            &[],
            Some("do it"),
            None,
            None,
            "/bin/zsh",
            "/conf",
            Some("haiku"),
            Some("low"),
            "claude",
            None,
        );
        assert!(cmd.contains("--model"));
        assert!(cmd.contains("haiku"));
        assert!(cmd.contains("--effort"));
        assert!(cmd.contains("low"));
    }

    /// Shell-unescape one `'…'` token the way `sh` would, so assertions can
    /// compare the TOML that Codex actually receives.
    fn unquote(tok: &str) -> String {
        tok.trim_matches('\'').replace("'\\''", "'")
    }

    #[test]
    fn codex_fresh_start_maps_bypass_and_seeds_prompt_after_dashdash() {
        let cmd = codex_invocation(
            "bypassPermissions",
            Some("-fix the bug"),
            None,
            None,
            None,
            None,
            "/work/wt",
            None,
        );
        assert_eq!(
            cmd,
            "codex --no-daemon --dangerously-bypass-approvals-and-sandbox \
             -c 'projects={\"/work/wt\"={trust_level=\"trusted\"}}' -- '-fix the bug'"
        );
    }

    #[test]
    fn codex_resume_uses_resume_subcommand_with_id_then_prompt() {
        let cmd = codex_invocation("default", Some("continue"), Some("01a1-uuid"), None, None, None, "/w", None);
        assert!(cmd.starts_with("codex resume --no-daemon -s workspace-write -a on-request "));
        assert!(cmd.ends_with(" '01a1-uuid' -- 'continue'"));
        // Plain resume (reattach after the session died): id, no prompt.
        let plain = codex_invocation("default", None, Some("01a1-uuid"), None, None, None, "/w", None);
        assert!(plain.starts_with("codex resume "));
        assert!(plain.ends_with(" '01a1-uuid'"));
        // An empty id is no resume at all.
        assert!(codex_invocation("default", None, Some(""), None, None, None, "/w", None).starts_with("codex --no-daemon "));
    }

    #[test]
    fn codex_permission_modes_map_to_sandbox_and_approvals() {
        let flags = |mode: &str| {
            let c = codex_invocation(mode, None, None, None, None, None, "/w", None);
            c.trim_start_matches("codex --no-daemon ")
                .split(" -c ")
                .next()
                .unwrap()
                .to_string()
        };
        assert_eq!(flags("bypassPermissions"), "--dangerously-bypass-approvals-and-sandbox");
        assert_eq!(flags("auto"), "--approve-for-me");
        assert_eq!(flags("plan"), "-s read-only -a on-request");
        assert_eq!(flags("dontAsk"), "-s workspace-write -a never");
        assert_eq!(flags("default"), "-s workspace-write -a on-request");
        assert_eq!(flags("acceptEdits"), "-s workspace-write -a on-request");
    }

    #[test]
    fn codex_effort_becomes_reasoning_effort_and_mcp_is_wired_per_invocation() {
        let cmd = codex_invocation(
            "bypassPermissions",
            None,
            None,
            None,
            Some("high"),
            None,
            "/Users/y/Application Support/wt",
            Some("/Users/y/Library/Application Support/Flock/mcp/flock-mcp.mjs"),
        );
        let overrides: Vec<String> = cmd.split(" -c ").skip(1).map(unquote).collect();
        assert_eq!(
            overrides,
            vec![
                "model_reasoning_effort=\"high\"".to_string(),
                "projects={\"/Users/y/Application Support/wt\"={trust_level=\"trusted\"}}".to_string(),
                "mcp_servers.flock.command=\"node\"".to_string(),
                "mcp_servers.flock.args=[\"/Users/y/Library/Application Support/Flock/mcp/flock-mcp.mjs\"]".to_string(),
                "mcp_servers.flock.env_vars=[\"FLOCK_WORKTREE_ID\",\"FLOCK_API_URL\",\"FLOCK_TOKEN\"]"
                .to_string(),
            ]
        );
        // No model given → no `-m`.
        assert!(!cmd.contains(" -m "));
    }

    #[test]
    fn codex_toml_strings_escape_quotes_and_backslashes() {
        assert_eq!(super::toml_str(r#"/a "b" \c"#), r#""/a \"b\" \\c""#);
    }

    #[test]
    fn codex_toml_strings_escape_newlines_and_control_chars() {
        // Raw newlines aren't valid in a TOML basic string; an orchestrator
        // prompt is full of them.
        assert_eq!(super::toml_str("a\nb\tc\r\u{1}"), r#""a\nb\tc\r\u0001""#);
    }

    #[test]
    fn codex_model_and_developer_instructions_are_forwarded() {
        let sys = "You are an ORCHESTRATOR.\nUse \"task_create\".";
        let cmd = codex_invocation(
            "bypassPermissions",
            Some("mission"),
            None,
            Some("gpt-6-sol"),
            Some("high"),
            Some(sys),
            "/orch",
            None,
        );
        assert!(cmd.contains(" -m 'gpt-6-sol' "));
        let overrides: Vec<String> = cmd
            .split(" -- ")
            .next()
            .unwrap()
            .split(" -c ")
            .skip(1)
            .map(unquote)
            .collect();
        assert!(overrides.contains(
            &r#"developer_instructions="You are an ORCHESTRATOR.\nUse \"task_create\".""#.to_string()
        ));
        // The mission is still the first turn.
        assert!(cmd.ends_with(" -- 'mission'"));
    }

    #[test]
    fn agent_invocation_gives_each_agent_only_values_it_understands() {
        use super::agent_invocation;
        // A Codex row keeps its Codex model; switched to Claude it's dropped,
        // as is the Codex-only "default" effort.
        let claude = agent_invocation("claude", "default", None, None, None, Some("gpt-6-sol"), Some("default"), "/w", None);
        assert_eq!(claude, "claude");
        let codex = agent_invocation("codex", "default", None, None, None, Some("gpt-6-sol"), Some("default"), "/w", None);
        assert!(codex.contains(" -m 'gpt-6-sol'"));
        assert!(!codex.contains("model_reasoning_effort"));
        // A Claude alias or "default" never reaches Codex as -m.
        for m in ["opus", "default"] {
            let c = agent_invocation("codex", "default", None, None, None, Some(m), None, "/w", None);
            assert!(!c.contains(" -m "), "{m}");
        }
        // An orchestrator's instructions: --append-system-prompt for Claude,
        // developer_instructions for Codex.
        let c = agent_invocation("claude", "default", Some("go"), None, Some("orchestrate"), Some("opus"), Some("high"), "/o", None);
        assert_eq!(c, "claude --model 'opus' --effort 'high' --append-system-prompt 'orchestrate' 'go'");
        let x = agent_invocation("codex", "default", Some("go"), None, Some("orchestrate"), None, None, "/o", None);
        assert!(x.contains("developer_instructions=\"orchestrate\""));
        assert!(!x.contains("append-system-prompt"));
    }

    #[test]
    fn agent_invocation_dispatches_on_agent() {
        use super::agent_invocation;
        let claude = agent_invocation("claude", "default", None, Some("s1"), None, Some("opus"), None, "/w", Some("/m.mjs"));
        assert_eq!(claude, "claude --model 'opus' --resume 's1'");
        let codex = agent_invocation("codex", "default", None, Some("s1"), None, Some("opus"), None, "/w", Some("/m.mjs"));
        assert!(codex.starts_with("codex resume --no-daemon"));
        assert!(codex.contains("mcp_servers.flock"));
    }

    #[test]
    fn detached_cmd_runs_codex_for_codex_worktrees() {
        use super::detached_tmux_cmd;
        let cmd = detached_tmux_cmd(
            7,
            "/work/dir",
            "bypassPermissions",
            &[],
            Some("handoff"),
            None,
            None,
            "/bin/zsh",
            "/conf",
            Some("opus"),
            Some("medium"),
            "codex",
            None,
        );
        assert!(cmd.contains("new-session -d"));
        assert!(cmd.contains("-s 'flock-7'"));
        assert!(cmd.contains("codex --no-daemon"));
        assert!(!cmd.contains("claude"));
        assert!(cmd.contains("handoff"));
    }

    #[test]
    fn tmux_env_line_match_detects_drift() {
        use super::tmux_env_line_matches;
        let k = "GH_CONFIG_DIR";
        let v = "/Users/y/.config/gh-personal";
        // Exact match (tmux may add a trailing newline).
        assert!(tmux_env_line_matches(k, v, "GH_CONFIG_DIR=/Users/y/.config/gh-personal\n"));
        // Wrong value → drift.
        assert!(!tmux_env_line_matches(k, v, "GH_CONFIG_DIR=/Users/y/.config/gh-thanx"));
        // Explicitly-unset form tmux prints with a leading dash → drift.
        assert!(!tmux_env_line_matches(k, v, "-GH_CONFIG_DIR"));
    }
}
