mod api;
mod commands;
mod db;
mod env_profiles;
mod error;
mod git;
mod monitor;
mod pty;
mod state;

use state::AppState;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    env_logger::init();
    let _ = pty::ensure_tmux_config();
    let app_state = AppState::new().expect("failed to init app state");

    tauri::Builder::default()
        // Two Flocks attached to the same tmux session would fight for the
        // `-D` detach. Easier to just forbid a second instance outright.
        .plugin(tauri_plugin_single_instance::init(|_app, _argv, _cwd| {}))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_notification::init())
        .manage(app_state)
        .setup(|app| {
            monitor::spawn(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::repo_add,
            commands::repos_list,
            commands::repo_remove,
            commands::repo_branches,
            commands::repo_default_branch,
            commands::repo_all_branches,
            commands::worktree_create,
            commands::worktrees_list,
            commands::worktree_remove,
            commands::worktree_dirty,
            commands::worktree_current_branch,
            commands::worktree_set_permission_mode,
            commands::worktree_set_title,
            commands::env_config_get,
            commands::env_config_set,
            commands::task_create,
            commands::session_open,
            commands::session_write,
            commands::session_resize,
            commands::session_close,
            commands::tmux_check,
            api::remote_start,
            api::remote_stop,
            api::remote_info,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
