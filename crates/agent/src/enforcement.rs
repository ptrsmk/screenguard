use anyhow::Result;
use chrono::{Datelike, Local, NaiveTime};
use common::models::EnforceAction;
use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::db::Db;

pub async fn evaluate_enforcement(
    uid: u32,
    db: &Arc<Mutex<Db>>,
    online: bool,
) -> Result<EnforceAction> {
    let db = db.lock().await;

    if online {
        if let Some(remaining) = db.get_server_remaining(uid)? {
            return Ok(match remaining.enforce.as_str() {
                "warn" => EnforceAction::Warn,
                "lock" => EnforceAction::Lock,
                _ => EnforceAction::Allow,
            });
        }
        // Server hasn't sent remaining yet — allow (benefit of the doubt on startup).
        return Ok(EnforceAction::Allow);
    }

    // Offline: calculate locally.
    offline_evaluate(uid, &db)
}

fn offline_evaluate(uid: u32, db: &Db) -> Result<EnforceAction> {
    let now = Local::now();
    let today = now.date_naive();
    let weekday = today.weekday().num_days_from_monday() as u8;

    // 1. Check schedule windows.
    let schedules = db.get_cached_schedules(uid)?;
    let (in_window, window_remaining_minutes) = if schedules.is_empty() {
        (true, i32::MAX)
    } else {
        let current_time = now.time();
        let matching = schedules.iter().find(|s| {
            s.day_of_week == weekday
                && parse_time(&s.start_time)
                    .zip(parse_time(&s.end_time))
                    .map(|(start, end)| current_time >= start && current_time <= end)
                    .unwrap_or(false)
        });
        match matching {
            None => (false, 0),
            Some(s) => {
                let end = parse_time(&s.end_time).unwrap_or(NaiveTime::from_hms_opt(23, 59, 0).unwrap());
                let secs = (end - current_time).num_seconds().max(0);
                (true, (secs / 60) as i32)
            }
        }
    };

    if !in_window {
        tracing::info!("uid={uid}: outside allowed schedule window (offline evaluation)");
        return Ok(EnforceAction::Lock);
    }

    // 2. Check daily limit.
    let limits = db.get_cached_daily_limits(uid)?;
    let today_str = today.to_string();
    let time_remaining_minutes = if let Some(limit) = limits.iter().find(|l| l.day_of_week == weekday) {
        let used_secs = db.get_usage_seconds(uid, &today_str)?;
        let adjustment = db.get_cached_adjustment(uid, &today_str)?;
        let used_minutes = (used_secs / 60) as i32;
        limit.allowed_minutes as i32 + adjustment - used_minutes
    } else {
        i32::MAX // no limit for this day
    };

    // 3. Effective remaining.
    let remaining = time_remaining_minutes.min(window_remaining_minutes);

    if remaining <= 0 {
        tracing::info!("uid={uid}: daily time limit reached (offline evaluation)");
        return Ok(EnforceAction::Lock);
    }

    // 4. Warning thresholds — warn if remaining is at or below any configured threshold.
    let enforcement = db.get_cached_enforcement(uid)?;
    if enforcement.warning_thresholds.iter().any(|&t| remaining <= t as i32) {
        return Ok(EnforceAction::Warn);
    }

    Ok(EnforceAction::Allow)
}

fn parse_time(s: &str) -> Option<NaiveTime> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() < 2 {
        return None;
    }
    let h: u32 = parts[0].parse().ok()?;
    let m: u32 = parts[1].parse().ok()?;
    NaiveTime::from_hms_opt(h, m, 0)
}

#[derive(Debug, PartialEq, Eq)]
enum LockBehavior {
    TerminateAfterGrace,
    PreserveAndRelock,
}

fn lock_behavior(preserve_tasks_on_lock: bool) -> LockBehavior {
    if preserve_tasks_on_lock {
        LockBehavior::PreserveAndRelock
    } else {
        LockBehavior::TerminateAfterGrace
    }
}

/// Whether to show the "screen locks in 4 seconds" courtesy notification
/// before actually locking the session.
///
/// Skipped when the admin has configured a zero grace period: that setting
/// is documented as "immediate logout" and signals no leniency at all, so
/// the unconditional 4s notify-then-wait window would otherwise hand back
/// exactly the few seconds of unrestricted access a zero grace period was
/// meant to prevent (#19).
fn should_warn_before_lock(grace_minutes: u32) -> bool {
    grace_minutes > 0
}

/// UIDs currently in their "save your work" window. The heartbeat's re-lock loop skips
/// them, otherwise it would lock the screen within ~10 s and cut the window short.
static SAVE_WINDOW: std::sync::Mutex<BTreeSet<u32>> = std::sync::Mutex::new(BTreeSet::new());

pub fn in_save_window(uid: u32) -> bool {
    SAVE_WINDOW.lock().map(|s| s.contains(&uid)).unwrap_or(false)
}

/// Marks `uid` as being in its save window until dropped.
struct SaveWindowGuard(u32);

impl SaveWindowGuard {
    fn enter(uid: u32) -> Self {
        if let Ok(mut s) = SAVE_WINDOW.lock() {
            s.insert(uid);
        }
        SaveWindowGuard(uid)
    }
}

impl Drop for SaveWindowGuard {
    fn drop(&mut self) {
        if let Ok(mut s) = SAVE_WINDOW.lock() {
            s.remove(&self.0);
        }
    }
}

/// What happens between "time is up" and the lock.
#[derive(Debug, PartialEq, Eq)]
enum PreLock {
    /// Critical "save your work now" warning, session stays usable for this long.
    SaveWindow(u32),
    /// The short courtesy notice (no save window configured).
    Notice,
    /// Lock at once.
    Immediate,
}

fn pre_lock(save_window_seconds: u32, grace_minutes: u32) -> PreLock {
    if save_window_seconds > 0 {
        PreLock::SaveWindow(save_window_seconds)
    } else if should_warn_before_lock(grace_minutes) {
        PreLock::Notice
    } else {
        PreLock::Immediate
    }
}

/// Execute a lock for a UID, preserving or terminating the session according to cached config.
///
/// `locked_uids` is the same set the caller inserted `uid` into to arm this lock. It is
/// re-checked after the grace-period sleep: if the block was lifted in the meantime (admin
/// granted time, schedule window opened, midnight usage reset, preserve re-enabled, ...) the
/// heartbeat/RemainingUpdate handler already removed `uid` from it, and this call must not
/// blindly kill a session the user is now legitimately using just because one still exists.
pub async fn execute_lock(
    uid: u32,
    db: &Arc<Mutex<Db>>,
    locked_uids: &Arc<Mutex<HashSet<u32>>>,
) -> Result<bool> {
    let (session_ids, grace_minutes, language, preserve_tasks, save_window) = {
        let db = db.lock().await;
        let sessions = db.get_all_session_ids(uid)?;
        let enforcement = db.get_cached_enforcement(uid)?;
        (
            sessions,
            enforcement.lockout_grace_minutes,
            enforcement.language,
            enforcement.preserve_tasks_on_lock,
            enforcement.save_window_seconds,
        )
    };

    if session_ids.is_empty() {
        return Ok(true);
    }

    // Final warning before the screen locks.
    match pre_lock(save_window, grace_minutes) {
        PreLock::SaveWindow(secs) => {
            let _guard = SaveWindowGuard::enter(uid);
            tracing::info!("uid={uid}: time is up, {secs} s to save work before the lock");
            let _ = crate::dbus::send_urgent_notification(
                uid,
                crate::i18n::notif_lock_title(&language),
                &crate::i18n::notif_save_body(&language, secs),
            ).await;
            tokio::time::sleep(std::time::Duration::from_secs(secs as u64)).await;
            if !locked_uids.lock().await.contains(&uid) {
                tracing::info!("uid={uid}: block lifted during the save window — not locking");
                return Ok(true);
            }
        }
        PreLock::Notice => {
            let _ = crate::dbus::send_desktop_notification(
                uid,
                crate::i18n::notif_lock_title(&language),
                crate::i18n::notif_lock_body(&language),
            ).await;
            tokio::time::sleep(std::time::Duration::from_secs(4)).await;
        }
        PreLock::Immediate => {}
    }
    // Sessions may have changed while the child was saving.
    let session_ids = db.lock().await.get_all_session_ids(uid)?;
    if session_ids.is_empty() {
        return Ok(true);
    }

    tracing::info!("Locking sessions for uid={uid}: {:?}", session_ids);
    crate::dbus::lock_sessions(&session_ids).await?;

    if lock_behavior(preserve_tasks) == LockBehavior::PreserveAndRelock {
        tracing::info!("Keeping sessions running while uid={uid} remains locked");
        return Ok(false);
    }

    // Wait grace period, then terminate any still-active sessions — but only if the
    // block is still in effect.
    tokio::time::sleep(std::time::Duration::from_secs(grace_minutes as u64 * 60)).await;

    if !locked_uids.lock().await.contains(&uid) {
        tracing::info!(
            "uid={uid}: grace period elapsed but enforcement was lifted in the meantime \
             — not terminating"
        );
        return Ok(true);
    }

    let still_active = {
        let db = db.lock().await;
        db.get_all_session_ids(uid)?
    };

    if !still_active.is_empty() {
        tracing::warn!("Sessions still active after grace period for uid={uid}, terminating");
        crate::dbus::terminate_sessions(&still_active).await?;
    }

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::{lock_behavior, should_warn_before_lock, LockBehavior};

    #[test]
    fn preserve_off_terminates_after_grace() {
        assert_eq!(lock_behavior(false), LockBehavior::TerminateAfterGrace);
    }

    #[test]
    fn preserve_on_keeps_session_armed_for_relocking() {
        assert_eq!(lock_behavior(true), LockBehavior::PreserveAndRelock);
    }

    // Regression test for #19: a zero grace period must skip the unconditional
    // 4s "screen locks in 4 seconds" notification too, not just the
    // post-lock termination wait — otherwise "immediate logout" still hands
    // back a few seconds of unrestricted access.
    #[test]
    fn zero_grace_skips_the_pre_lock_warning() {
        assert!(!should_warn_before_lock(0));
    }

    #[test]
    fn save_window_takes_precedence_and_zero_falls_back() {
        use super::{pre_lock, PreLock};
        assert_eq!(pre_lock(30, 1), PreLock::SaveWindow(30));
        assert_eq!(pre_lock(30, 0), PreLock::SaveWindow(30));
        assert_eq!(pre_lock(0, 5), PreLock::Notice);
        assert_eq!(pre_lock(0, 0), PreLock::Immediate);
    }

    #[test]
    fn save_window_guard_marks_and_clears_the_uid() {
        use super::{in_save_window, SaveWindowGuard};
        assert!(!in_save_window(4242));
        {
            let _g = SaveWindowGuard::enter(4242);
            assert!(in_save_window(4242));
        }
        assert!(!in_save_window(4242));
    }

    #[test]
    fn positive_grace_still_shows_the_pre_lock_warning() {
        assert!(should_warn_before_lock(1));
        assert!(should_warn_before_lock(5));
    }
}

/// Midnight handler: called when the calendar date changes.
/// Resets daily usage and locks any sessions that fall outside the new day's schedule.
pub async fn handle_midnight(
    db: &Arc<Mutex<Db>>,
    locked_uids: &Arc<Mutex<HashSet<u32>>>,
) -> Result<()> {
    let yesterday = (Local::now() - chrono::Duration::days(1)).date_naive();
    let uids = {
        let db = db.lock().await;
        db.get_managed_uids()?
    };

    for uid in &uids {
        let db_guard = db.lock().await;
        let _ = db_guard.reset_usage_for_date(*uid, &yesterday);
    }
    tracing::info!("Midnight: reset daily usage counters for {} users", uids.len());

    // Lock any sessions that fall outside the new day's allowed schedule window.
    for uid in uids {
        let action = evaluate_enforcement(uid, db, false).await?;
        if action == EnforceAction::Lock {
            let is_new = locked_uids.lock().await.insert(uid);
            if is_new {
                tracing::info!("Midnight: locking uid={uid} (outside schedule for new day)");
                let db = db.clone();
                let locked_uids = locked_uids.clone();
                tokio::spawn(async move {
                    let rearm = match execute_lock(uid, &db, &locked_uids).await {
                        Ok(rearm) => rearm,
                        Err(e) => {
                            tracing::error!("Midnight lock failed for uid={uid}: {e}");
                            true
                        }
                    };
                    if rearm {
                        locked_uids.lock().await.remove(&uid);
                    }
                });
            }
        }
    }

    Ok(())
}
