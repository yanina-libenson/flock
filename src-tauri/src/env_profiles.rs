//! Per-environment env-var injection, resolved by folder.
//!
//! A named **environment** is a bundle of environment variables (e.g. a
//! `RENDER_API_KEY` token). A **binding** maps a directory path to an
//! environment. When a worktree's session starts, Flock resolves the
//! environment by longest-prefix match against the *repo's* registered path
//! (not the worktree checkout) and injects its vars via `tmux -e`.
//!
//! This lets a `~/Code/work` folder share one environment while individual
//! repos under `~/Code/personal` override with their own — new repos under a
//! bound folder inherit automatically.
//!
//! Tokens live here (0600 in the data dir), never in a repo and never
//! committed. The repo's own `.mcp.json` references them as `${VAR}`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Environment {
    pub name: String,
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Binding {
    /// Absolute directory prefix (a containing folder or an exact repo path).
    pub path: String,
    /// Name of the environment this folder uses.
    pub env: String,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct EnvConfig {
    #[serde(default)]
    pub environments: Vec<Environment>,
    #[serde(default)]
    pub bindings: Vec<Binding>,
}

fn config_path() -> std::io::Result<PathBuf> {
    let dir = dirs::data_local_dir()
        .ok_or_else(|| std::io::Error::other("no data local dir"))?
        .join("Flock");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("environments.json"))
}

pub fn load() -> EnvConfig {
    config_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(cfg: &EnvConfig) -> std::io::Result<()> {
    let path = config_path()?;
    let json = serde_json::to_string_pretty(cfg).map_err(std::io::Error::other)?;
    std::fs::write(&path, json)?;
    // Tokens at rest — owner-only.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// True when `prefix` equals `path` or is an ancestor directory of it.
/// Component-aware so `/Code/Personal` does not match `/Code/PersonalX`.
fn path_is_prefix(prefix: &str, path: &str) -> bool {
    let prefix = prefix.trim_end_matches('/');
    let path = path.trim_end_matches('/');
    !prefix.is_empty() && (path == prefix || path.starts_with(&format!("{prefix}/")))
}

/// Resolve the env vars to inject for a repo at `repo_path`. The binding whose
/// path is the longest matching prefix wins (so a repo-level binding beats its
/// containing folder); its environment's vars are returned. Empty when nothing
/// matches.
pub fn resolve_vars(cfg: &EnvConfig, repo_path: &str) -> Vec<(String, String)> {
    let best = cfg
        .bindings
        .iter()
        .filter(|b| path_is_prefix(&b.path, repo_path))
        .max_by_key(|b| b.path.trim_end_matches('/').len());
    if let Some(b) = best {
        if let Some(env) = cfg.environments.iter().find(|e| e.name == b.env) {
            return env
                .vars
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
        }
    }
    Vec::new()
}

/// Resolve the env vars for an explicitly named environment (e.g. an
/// orchestrator's chosen profile, which has no repo path to match on). Empty
/// when the name is None or doesn't match a defined environment.
pub fn resolve_vars_by_name(cfg: &EnvConfig, name: Option<&str>) -> Vec<(String, String)> {
    let Some(name) = name else {
        return Vec::new();
    };
    cfg.environments
        .iter()
        .find(|e| e.name == name)
        .map(|e| e.vars.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default()
}

/// Resolve the env vars for a worktree: honors a persisted `env_profile` (an
/// orchestrator's explicitly chosen account, which has no repo path to
/// re-derive it from) when set, else falls back to path-based resolution
/// against the repo. Every place that re-resolves a worktree's env after
/// creation (reattach, resume-on-input, the Reader) must go through this, not
/// `resolve_vars` directly, or a persisted profile choice is silently dropped
/// on the next resolution — this is what broke orchestrator resume: the
/// profile was honored at launch but recomputed by path (which matches no
/// binding for the orchestrators scratch dir) on every later reattach.
pub fn resolve_vars_for_worktree(
    cfg: &EnvConfig,
    env_profile: Option<&str>,
    repo_path: &str,
) -> Vec<(String, String)> {
    match env_profile {
        Some(name) => resolve_vars_by_name(cfg, Some(name)),
        None => resolve_vars(cfg, repo_path),
    }
}

/// The resolved `CLAUDE_CONFIG_DIR` for a worktree — honoring a persisted
/// `env_profile` override, else path-based resolution, same precedence as
/// `resolve_vars_for_worktree`. `None` means the default `~/.claude` account
/// (either nothing bound, or the bound environment doesn't override the
/// config dir — e.g. "Thanx" only sets `GH_CONFIG_DIR`). This is deliberately
/// narrower than comparing full env-var sets or binding names: two bindings
/// can differ (e.g. "Thanx" vs. no binding) while resolving the *same* Claude
/// account, and other vars (tokens, `GH_CONFIG_DIR`) can differ without ever
/// changing which Claude account/subscription a session authenticates as.
/// Used to detect a cross-account task_create (see `commands::start_task_core`).
pub fn claude_config_dir_for_worktree(
    cfg: &EnvConfig,
    env_profile: Option<&str>,
    repo_path: &str,
) -> Option<String> {
    resolve_vars_for_worktree(cfg, env_profile, repo_path)
        .into_iter()
        .find(|(k, _)| k == "CLAUDE_CONFIG_DIR")
        .map(|(_, v)| v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> EnvConfig {
        EnvConfig {
            environments: vec![
                Environment {
                    name: "Work".into(),
                    vars: BTreeMap::new(),
                },
                Environment {
                    name: "Personal-A".into(),
                    vars: BTreeMap::from([("RENDER_API_KEY".into(), "tok_a".into())]),
                },
                Environment {
                    name: "Personal-B".into(),
                    vars: BTreeMap::from([("RENDER_API_KEY".into(), "tok_b".into())]),
                },
            ],
            bindings: vec![
                Binding {
                    path: "/Users/y/Code/work".into(),
                    env: "Work".into(),
                },
                Binding {
                    path: "/Users/y/Code/Personal/render-a".into(),
                    env: "Personal-A".into(),
                },
                Binding {
                    path: "/Users/y/Code/Personal/render-b".into(),
                    env: "Personal-B".into(),
                },
            ],
        }
    }

    #[test]
    fn folder_binding_is_inherited() {
        let v = resolve_vars(&cfg(), "/Users/y/Code/work/some-work-repo");
        assert!(v.is_empty()); // Work has no vars (no Render)
    }

    #[test]
    fn longest_prefix_wins() {
        let v = resolve_vars(&cfg(), "/Users/y/Code/Personal/render-a");
        assert_eq!(v, vec![("RENDER_API_KEY".to_string(), "tok_a".to_string())]);
        let v = resolve_vars(&cfg(), "/Users/y/Code/Personal/render-b/sub");
        assert_eq!(v, vec![("RENDER_API_KEY".to_string(), "tok_b".to_string())]);
    }

    #[test]
    fn no_binding_means_no_vars() {
        assert!(resolve_vars(&cfg(), "/Users/y/Code/Other/repo").is_empty());
    }

    #[test]
    fn prefix_is_component_aware() {
        assert!(path_is_prefix("/Code/Personal", "/Code/Personal/x"));
        assert!(path_is_prefix("/Code/Personal", "/Code/Personal"));
        assert!(!path_is_prefix("/Code/Personal", "/Code/PersonalX/x"));
        assert!(!path_is_prefix("", "/anything"));
    }

    #[test]
    fn by_name_returns_that_env_vars() {
        let v = resolve_vars_by_name(&cfg(), Some("Personal-A"));
        assert_eq!(v, vec![("RENDER_API_KEY".to_string(), "tok_a".to_string())]);
    }

    #[test]
    fn by_name_unknown_or_none_is_empty() {
        assert!(resolve_vars_by_name(&cfg(), Some("Nope")).is_empty());
        assert!(resolve_vars_by_name(&cfg(), None).is_empty());
    }

    #[test]
    fn for_worktree_persisted_profile_wins_over_path() {
        // Regression: an orchestrator scratch dir matches no path binding, so
        // without the persisted profile this resolved empty on every reattach
        // even though the account was chosen correctly at creation.
        let v = resolve_vars_for_worktree(
            &cfg(),
            Some("Personal-A"),
            "/Users/y/Library/Application Support/Flock/orchestrators/kyoto",
        );
        assert_eq!(v, vec![("RENDER_API_KEY".to_string(), "tok_a".to_string())]);
    }

    #[test]
    fn for_worktree_no_profile_falls_back_to_path() {
        let v = resolve_vars_for_worktree(&cfg(), None, "/Users/y/Code/Personal/render-b/sub");
        assert_eq!(v, vec![("RENDER_API_KEY".to_string(), "tok_b".to_string())]);
    }

    fn accounts_cfg() -> EnvConfig {
        EnvConfig {
            environments: vec![
                Environment {
                    // Mirrors real "Thanx": binds a folder but doesn't override
                    // CLAUDE_CONFIG_DIR — same Claude account as no binding at all.
                    name: "Thanx".into(),
                    vars: BTreeMap::from([("GH_CONFIG_DIR".into(), "gh-thanx".into())]),
                },
                Environment {
                    name: "Personal".into(),
                    vars: BTreeMap::from([(
                        "CLAUDE_CONFIG_DIR".into(),
                        "/Users/y/.claude-personal".into(),
                    )]),
                },
            ],
            bindings: vec![
                Binding {
                    path: "/Users/y/Code/Thanx".into(),
                    env: "Thanx".into(),
                },
                Binding {
                    path: "/Users/y/Code/Personal".into(),
                    env: "Personal".into(),
                },
            ],
        }
    }

    #[test]
    fn claude_config_dir_is_none_when_binding_does_not_override_it() {
        // "Thanx" only sets GH_CONFIG_DIR — same default Claude account as no
        // binding at all, not a distinct one.
        assert_eq!(
            claude_config_dir_for_worktree(&accounts_cfg(), None, "/Users/y/Code/Thanx/nexus"),
            None
        );
    }

    #[test]
    fn claude_config_dir_matches_the_overriding_binding() {
        assert_eq!(
            claude_config_dir_for_worktree(
                &accounts_cfg(),
                None,
                "/Users/y/Code/Personal/ixi/backend"
            ),
            Some("/Users/y/.claude-personal".to_string())
        );
    }

    #[test]
    fn claude_config_dir_persisted_profile_wins_over_path() {
        // An orchestrator with no repo path still resolves its explicitly
        // chosen profile's config dir.
        assert_eq!(
            claude_config_dir_for_worktree(
                &accounts_cfg(),
                Some("Personal"),
                "/Users/y/Library/Application Support/Flock/orchestrators/kyoto"
            ),
            Some("/Users/y/.claude-personal".to_string())
        );
    }

    #[test]
    fn claude_config_dir_unbound_path_is_default_account() {
        assert_eq!(
            claude_config_dir_for_worktree(&accounts_cfg(), None, "/Users/y/Code/Other/repo"),
            None
        );
    }
}
