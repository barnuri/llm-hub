//! Copilot quota readings kept every 15 minutes, and the daily burn between them.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::configs::CopilotAuth;
use crate::dependencies::state::AppState;
use crate::schemas::quota_daily_delta::QuotaDailyDelta;
use crate::schemas::quota_snapshot_row::QuotaSnapshotRow;
use crate::services::copilot_usage;
use crate::services::store::Store;
use crate::utils::time::utc_iso8601;

/// Copilot quota readings: the 15-minute poll, the stored rows, and the daily burn.
#[derive(Debug)]
pub struct QuotaSnapshots;

impl QuotaSnapshots {
    const POLL_INTERVAL: Duration = Duration::from_mins(15);
    const UTC_DAY_CHARS: usize = "2026-11-10".len();

    /// Polls every enabled Copilot profile on a 15-minute tick, starting at boot. Runs only on the sqlite store.
    pub fn spawn_poll_loop(state: AppState) {
        if !matches!(state.store(), Some(Store::Sqlite(_))) {
            return;
        }
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Self::POLL_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                Self::poll_once(&state).await;
            }
        });
    }

    async fn poll_once(state: &AppState) {
        let config = state.config();
        let targets: Vec<(String, CopilotAuth)> = config
            .profiles
            .iter()
            .filter(|profile| profile.enabled)
            .filter_map(|profile| {
                profile
                    .copilot
                    .clone()
                    .map(|auth| (profile.name.clone(), auth))
            })
            .collect();
        for (profile, auth) in targets {
            let Some(client) = state.client(&profile) else {
                continue;
            };
            let Some(usage) = copilot_usage::refresh_now(state, &client, &profile, &auth).await
            else {
                continue;
            };
            let rows: Vec<QuotaSnapshotRow> = usage
                .quotas
                .iter()
                .map(|quota| QuotaSnapshotRow {
                    ts_ms: usage.updated_at,
                    profile: profile.clone(),
                    quota: quota.id.clone(),
                    used: quota.used,
                    entitlement: quota.entitlement,
                    used_usd: quota.used_usd,
                    limit_usd: quota.limit_usd,
                })
                .collect();
            let writer = state.clone();
            let saved = tokio::task::spawn_blocking(move || match writer.store() {
                Some(store) => Self::save(store, &rows),
                None => Ok(()),
            })
            .await;
            match saved {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::warn!("copilot quota snapshot not stored: {error}"),
                Err(error) => tracing::warn!("copilot quota snapshot task failed: {error}"),
            }
        }
    }

    /// Stores one poll's readings in one transaction. The json store keeps no readings and writes nothing.
    pub fn save(store: &Store, rows: &[QuotaSnapshotRow]) -> Result<(), String> {
        let Store::Sqlite(conn) = store else {
            return Ok(());
        };
        let mut guard = conn.lock().map_err(|_| "sqlite lock poisoned")?;
        let tx = guard.transaction().map_err(|e| e.to_string())?;
        for row in rows {
            tx.execute(
                "INSERT INTO quota_snapshots
                    (ts_ms, profile, quota, used, entitlement, used_usd, limit_usd)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    i64::try_from(row.ts_ms).unwrap_or(i64::MAX),
                    row.profile,
                    row.quota,
                    row.used,
                    row.entitlement,
                    row.used_usd,
                    row.limit_usd
                ],
            )
            .map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())
    }

    /// Every reading at or after `since_ms`, ordered by profile, quota and time.
    pub fn history(store: &Store, since_ms: u64) -> Result<Vec<QuotaSnapshotRow>, String> {
        let Store::Sqlite(conn) = store else {
            return Err("quota history needs the sqlite store".into());
        };
        let guard = conn.lock().map_err(|_| "sqlite lock poisoned")?;
        let mut stmt = guard
            .prepare(
                "SELECT ts_ms, profile, quota, used, entitlement, used_usd, limit_usd
                 FROM quota_snapshots WHERE ts_ms >= ?1 ORDER BY profile, quota, ts_ms",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([i64::try_from(since_ms).unwrap_or(i64::MAX)], |row| {
                Ok(QuotaSnapshotRow {
                    ts_ms: u64::try_from(row.get::<_, i64>(0)?).unwrap_or(0),
                    profile: row.get(1)?,
                    quota: row.get(2)?,
                    used: row.get(3)?,
                    entitlement: row.get(4)?,
                    used_usd: row.get(5)?,
                    limit_usd: row.get(6)?,
                })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    /// The burn per UTC day, profile and quota: each change from the previous reading of its series.
    /// A drop means a reset, so the new reading counts in full; a series' first reading counts nothing.
    pub fn daily_deltas(rows: &[QuotaSnapshotRow]) -> Vec<QuotaDailyDelta> {
        let mut ordered: Vec<&QuotaSnapshotRow> = rows.iter().collect();
        ordered.sort_by(|left, right| {
            (&left.profile, &left.quota, left.ts_ms).cmp(&(
                &right.profile,
                &right.quota,
                right.ts_ms,
            ))
        });
        let mut days: BTreeMap<(String, String, String), (f64, Option<f64>)> = BTreeMap::new();
        for pair in ordered.windows(2) {
            let (previous, current) = (pair[0], pair[1]);
            if previous.profile != current.profile || previous.quota != current.quota {
                continue;
            }
            let reset = current.used < previous.used;
            let used_delta = if reset {
                current.used
            } else {
                current.used - previous.used
            };
            let used_usd_delta = match (previous.used_usd, current.used_usd) {
                (Some(before), Some(after)) => Some(if reset { after } else { after - before }),
                _ => None,
            };
            let key = (
                Self::utc_day(current.ts_ms),
                current.profile.clone(),
                current.quota.clone(),
            );
            let (burned, burned_usd) = days.entry(key).or_insert((0.0, None));
            *burned += used_delta;
            if let Some(delta) = used_usd_delta {
                *burned_usd = Some(burned_usd.unwrap_or(0.0) + delta);
            }
        }
        days.into_iter()
            .map(
                |((day, profile, quota), (used_delta, used_usd_delta))| QuotaDailyDelta {
                    day,
                    profile,
                    quota,
                    used_delta,
                    used_usd_delta,
                },
            )
            .collect()
    }

    fn utc_day(ts_ms: u64) -> String {
        let iso = utc_iso8601(ts_ms);
        iso.get(..Self::UTC_DAY_CHARS)
            .unwrap_or_default()
            .to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOV_10_START_MS: u64 = 1_794_268_800_000;
    const HOUR_MS: u64 = 3_600_000;

    /// Quota readings and daily deltas for the tests below.
    struct Readings;

    impl Readings {
        fn reading(ts_ms: u64, quota: &str, used: f64, used_usd: Option<f64>) -> QuotaSnapshotRow {
            QuotaSnapshotRow {
                ts_ms,
                profile: "demo".into(),
                quota: quota.into(),
                used,
                entitlement: 300.0,
                used_usd,
                limit_usd: used_usd.map(|_| 3.0),
            }
        }

        fn day_of(deltas: &[QuotaDailyDelta], day: &str) -> Option<QuotaDailyDelta> {
            deltas.iter().find(|delta| delta.day == day).cloned()
        }
    }

    #[test]
    fn utc_day_constant_is_the_day_it_names() {
        assert!(utc_iso8601(NOV_10_START_MS).starts_with("2026-11-10T00:00:00"));
    }

    #[test]
    fn steady_growth_books_each_step_to_its_day_and_skips_the_first_reading() {
        let rows = [
            Readings::reading(NOV_10_START_MS + 10 * HOUR_MS, "premium", 100.0, Some(1.0)),
            Readings::reading(
                NOV_10_START_MS + 10 * HOUR_MS + 900_000,
                "premium",
                103.0,
                Some(1.03),
            ),
            Readings::reading(NOV_10_START_MS + 11 * HOUR_MS, "premium", 110.0, Some(1.10)),
        ];
        let deltas = QuotaSnapshots::daily_deltas(&rows);
        assert_eq!(deltas.len(), 1);
        let day = Readings::day_of(&deltas, "2026-11-10").expect("the day has deltas");
        assert_eq!(
            (day.profile.as_str(), day.quota.as_str()),
            ("demo", "premium")
        );
        assert!((day.used_delta - 10.0).abs() < 1e-9);
        assert!((day.used_usd_delta.expect("usd present") - 0.10).abs() < 1e-9);
    }

    #[test]
    fn a_reset_counts_the_new_reading_in_full() {
        let rows = [
            Readings::reading(NOV_10_START_MS + HOUR_MS, "premium", 250.0, Some(2.5)),
            Readings::reading(NOV_10_START_MS + 2 * HOUR_MS, "premium", 4.0, Some(0.04)),
            Readings::reading(NOV_10_START_MS + 3 * HOUR_MS, "premium", 9.0, Some(0.09)),
        ];
        let deltas = QuotaSnapshots::daily_deltas(&rows);
        let day = Readings::day_of(&deltas, "2026-11-10").expect("the day has deltas");
        assert!((day.used_delta - 9.0).abs() < 1e-9);
        assert!((day.used_usd_delta.expect("usd present") - 0.09).abs() < 1e-9);
    }

    #[test]
    fn a_change_across_midnight_is_booked_to_the_later_day() {
        let rows = [
            Readings::reading(NOV_10_START_MS - 10 * 60_000, "premium", 50.0, None),
            Readings::reading(NOV_10_START_MS + 10 * 60_000, "premium", 53.0, None),
        ];
        let deltas = QuotaSnapshots::daily_deltas(&rows);
        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].day, "2026-11-10");
        assert!((deltas[0].used_delta - 3.0).abs() < 1e-9);
    }

    #[test]
    fn series_of_each_quota_and_profile_are_kept_apart_even_when_interleaved() {
        let mut other_profile = Readings::reading(NOV_10_START_MS + HOUR_MS, "premium", 40.0, None);
        other_profile.profile = "other".into();
        let rows = [
            Readings::reading(NOV_10_START_MS, "premium", 10.0, None),
            Readings::reading(NOV_10_START_MS + HOUR_MS, "chat", 7.0, None),
            Readings::reading(NOV_10_START_MS + 2 * HOUR_MS, "premium", 12.0, None),
            other_profile,
            Readings::reading(NOV_10_START_MS + 3 * HOUR_MS, "chat", 9.0, None),
        ];
        let deltas = QuotaSnapshots::daily_deltas(&rows);
        let find = |profile: &str, quota: &str| {
            deltas
                .iter()
                .find(|delta| delta.profile == profile && delta.quota == quota)
                .map(|delta| delta.used_delta)
        };
        assert_eq!(find("demo", "premium"), Some(2.0));
        assert_eq!(find("demo", "chat"), Some(2.0));
        assert_eq!(find("other", "premium"), None);
    }

    #[test]
    fn requests_quota_has_no_usd_delta() {
        let rows = [
            Readings::reading(NOV_10_START_MS, "premium", 10.0, None),
            Readings::reading(NOV_10_START_MS + HOUR_MS, "premium", 14.0, None),
        ];
        let deltas = QuotaSnapshots::daily_deltas(&rows);
        assert_eq!(deltas[0].used_usd_delta, None);
        assert!((deltas[0].used_delta - 4.0).abs() < 1e-9);
    }

    #[test]
    fn saved_readings_come_back_in_range_and_order_on_sqlite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quota.db");
        let store = Store::open("sqlite", Some(path.to_str().unwrap())).unwrap();
        let mut other = Readings::reading(NOV_10_START_MS + HOUR_MS, "chat", 5.0, Some(0.05));
        other.profile = "other".into();
        QuotaSnapshots::save(
            &store,
            &[
                Readings::reading(NOV_10_START_MS, "premium", 10.0, Some(0.10)),
                Readings::reading(NOV_10_START_MS + HOUR_MS, "premium", 11.0, Some(0.11)),
                other,
            ],
        )
        .unwrap();

        let all = QuotaSnapshots::history(&store, 0).unwrap();
        let keys: Vec<(&str, &str, u64)> = all
            .iter()
            .map(|row| (row.profile.as_str(), row.quota.as_str(), row.ts_ms))
            .collect();
        assert_eq!(
            keys,
            [
                ("demo", "premium", NOV_10_START_MS),
                ("demo", "premium", NOV_10_START_MS + HOUR_MS),
                ("other", "chat", NOV_10_START_MS + HOUR_MS),
            ]
        );
        assert_eq!(all[1].used_usd, Some(0.11));
        assert_eq!(all[1].limit_usd, Some(3.0));

        let recent = QuotaSnapshots::history(&store, NOV_10_START_MS + HOUR_MS).unwrap();
        assert_eq!(recent.len(), 2);
    }

    #[test]
    fn json_store_saves_nothing_and_reads_no_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stats.json");
        let store = Store::open("json", Some(path.to_str().unwrap())).unwrap();
        QuotaSnapshots::save(
            &store,
            &[Readings::reading(NOV_10_START_MS, "premium", 1.0, None)],
        )
        .unwrap();
        assert!(QuotaSnapshots::history(&store, 0).is_err());
    }
}
