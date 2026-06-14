//! Scheduled tasks — the safest loop: a spec fires a fresh prompted task on a
//! cadence.
//!
//! Spec syntax (deliberately small and unambiguous — no cron-dialect traps):
//!   - `@every <N>{m,h,d}`  → repeat every N minutes / hours / days
//!   - `HH:MM`              → daily at that local time
//!
//! A background thread ticks each minute, fires anything due via
//! `start_task_core`, and rolls `next_run` forward.

use crate::state::AppState;
use chrono::{Local, NaiveTime, TimeZone};
use std::time::Duration;
use tauri::{AppHandle, Manager};

const TICK: Duration = Duration::from_secs(30);

#[derive(Debug, PartialEq, Eq)]
pub enum Spec {
    /// Fixed interval in seconds.
    Every(i64),
    /// Daily at (hour, minute) local time.
    DailyAt(u32, u32),
}

/// Parse a schedule spec. Returns None on anything malformed.
pub fn parse_spec(s: &str) -> Option<Spec> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("@every ") {
        let rest = rest.trim();
        let (num, unit) = rest.split_at(rest.find(|c: char| !c.is_ascii_digit())?);
        let n: i64 = num.parse().ok()?;
        if n <= 0 {
            return None;
        }
        let secs = match unit.trim() {
            "m" | "min" => n * 60,
            "h" | "hr" => n * 3600,
            "d" | "day" => n * 86400,
            _ => return None,
        };
        return Some(Spec::Every(secs));
    }
    // HH:MM
    let (h, m) = s.split_once(':')?;
    let h: u32 = h.parse().ok()?;
    let m: u32 = m.parse().ok()?;
    if h > 23 || m > 59 {
        return None;
    }
    Some(Spec::DailyAt(h, m))
}

/// Next fire time (unix seconds) for a spec, given the current unix time.
pub fn next_run(spec: &Spec, now_unix: i64) -> i64 {
    match spec {
        Spec::Every(secs) => now_unix + secs,
        Spec::DailyAt(h, m) => {
            let now = Local.timestamp_opt(now_unix, 0).single();
            let Some(now) = now else {
                return now_unix + 86400;
            };
            let time = NaiveTime::from_hms_opt(*h, *m, 0).unwrap_or_default();
            let today = now.date_naive().and_time(time);
            let today_local = match Local.from_local_datetime(&today).single() {
                Some(dt) => dt,
                None => return now_unix + 86400, // DST gap — good enough
            };
            let target = if today_local.timestamp() > now_unix {
                today_local
            } else {
                today_local + chrono::Duration::days(1)
            };
            target.timestamp()
        }
    }
}

/// Compute the initial next_run for a freshly created schedule.
pub fn initial_next_run(spec: &Spec, now_unix: i64) -> i64 {
    next_run(spec, now_unix)
}

fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Spawn the scheduler loop. Fires due schedules and rolls them forward.
pub fn spawn(app: AppHandle) {
    std::thread::spawn(move || loop {
        std::thread::sleep(TICK);
        let Some(state) = app.try_state::<AppState>() else {
            continue;
        };
        let Ok(schedules) = state.db.list_schedules() else {
            continue;
        };
        let now = now_unix();
        for s in schedules {
            if !s.enabled || now < s.next_run {
                continue;
            }
            let Some(spec) = parse_spec(&s.spec) else {
                continue; // malformed; skip (won't fire, won't crash)
            };
            let title = s
                .title
                .clone()
                .filter(|t| !t.trim().is_empty())
                .or_else(|| Some(format!("scheduled: {}", s.spec)));
            // Fire the task. Errors are logged but must not stall the loop.
            if let Err(e) = crate::commands::start_task_core(
                &state,
                s.repo_id,
                &s.prompt,
                None,
                None,
                title,
                None,
            ) {
                eprintln!("flock: schedule {} fire failed: {e}", s.id);
            }
            let next = next_run(&spec, now);
            let _ = state.db.mark_schedule_run(s.id, now, next);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_every() {
        assert_eq!(parse_spec("@every 30m"), Some(Spec::Every(1800)));
        assert_eq!(parse_spec("@every 2h"), Some(Spec::Every(7200)));
        assert_eq!(parse_spec("@every 1d"), Some(Spec::Every(86400)));
        assert_eq!(parse_spec("@every 0m"), None);
        assert_eq!(parse_spec("@every 5x"), None);
        assert_eq!(parse_spec("@every abc"), None);
    }

    #[test]
    fn parse_daily() {
        assert_eq!(parse_spec("09:30"), Some(Spec::DailyAt(9, 30)));
        assert_eq!(parse_spec("23:59"), Some(Spec::DailyAt(23, 59)));
        assert_eq!(parse_spec("24:00"), None);
        assert_eq!(parse_spec("9:99"), None);
        assert_eq!(parse_spec("nope"), None);
    }

    #[test]
    fn next_run_every_is_offset() {
        assert_eq!(next_run(&Spec::Every(1800), 1000), 2800);
    }

    #[test]
    fn next_run_daily_is_within_a_day_ahead() {
        let now = 1_700_000_000;
        let n = next_run(&Spec::DailyAt(9, 0), now);
        assert!(n > now && n <= now + 86400);
    }
}
