//! Budget notifications: provider quotas and the hub's monthly spend against the operator's steps.

use std::collections::HashMap;

use crate::schemas::notification::{
    Notification, NotificationKind, NotificationSeverity, NotificationStatus,
};
use crate::services::profile_usage::{ProfileUsageQuota, QuotaUnit};
use crate::utils::time::utc_iso8601;

/// The `source` of every budget notification.
pub const SOURCE: &str = "budget";
/// The quota name of a hub spend budget reading.
pub const SPEND_QUOTA: &str = "spend_usd";
/// The id segment of a reading without a profile.
const GLOBAL: &str = "global";
const FULL_PERCENT: f64 = 100.0;
const MS_PER_DAY: u64 = 86_400_000;
/// `YYYY-MM`, the length of a period.
const PERIOD_CHARS: usize = "2026-11".len();
const DAY_OF_MONTH: std::ops::Range<usize> = 8..10;

/// What a reading counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BudgetUnit {
    Usd,
    Requests,
    /// AI credits.
    Credits,
    Tokens,
}

/// One quota or spend budget, as used against its limit in one period.
#[derive(Clone, Debug, PartialEq)]
pub struct BudgetReading {
    /// The profile the budget belongs to. `None` is the hub-wide spend budget.
    pub profile: Option<String>,
    /// The quota id, for example `premium_interactions`, or [`SPEND_QUOTA`].
    pub quota: String,
    pub used: f64,
    pub limit: f64,
    /// The month the reading counts in, `YYYY-MM`.
    pub period: String,
    pub unit: BudgetUnit,
}

impl BudgetReading {
    /// A provider quota from the live usage cache. The period is the month of the quota reset
    /// when the provider reports one, else the calendar month of `now_ms`.
    #[must_use]
    pub fn from_usage_quota(profile: &str, quota: &ProfileUsageQuota, now_ms: u64) -> Self {
        let period = quota
            .reset_date
            .as_deref()
            .and_then(period_of_date)
            .unwrap_or_else(|| period_of(now_ms));
        Self {
            profile: Some(profile.to_owned()),
            quota: quota.id.clone(),
            used: quota.used,
            limit: quota.entitlement,
            period,
            unit: match quota.unit {
                QuotaUnit::Aic => BudgetUnit::Credits,
                QuotaUnit::Requests => BudgetUnit::Requests,
                QuotaUnit::Tokens => BudgetUnit::Tokens,
                QuotaUnit::Usd => BudgetUnit::Usd,
            },
        }
    }

    /// The hub's measured spend this calendar month against a USD budget. `None` profile is global.
    #[must_use]
    pub fn spend(profile: Option<&str>, spent_usd: f64, limit_usd: f64, now_ms: u64) -> Self {
        Self {
            profile: profile.map(str::to_owned),
            quota: SPEND_QUOTA.to_owned(),
            used: spent_usd,
            limit: limit_usd,
            period: period_of(now_ms),
            unit: BudgetUnit::Usd,
        }
    }
}

/// Spend readings for the global budget (the sum of every profile) and each profile budget.
/// `spend` maps a profile to its spend this month; a profile missing from it spent nothing.
#[must_use]
pub fn spend_readings(
    global_limit_usd: Option<f64>,
    profile_limits_usd: &[(String, f64)],
    spend: &HashMap<String, f64>,
    now_ms: u64,
) -> Vec<BudgetReading> {
    let mut readings = Vec::with_capacity(profile_limits_usd.len() + 1);
    if let Some(limit) = global_limit_usd {
        readings.push(BudgetReading::spend(
            None,
            spend.values().sum(),
            limit,
            now_ms,
        ));
    }
    for (profile, limit) in profile_limits_usd {
        let used_usd = spend.get(profile).copied().unwrap_or(0.0);
        readings.push(BudgetReading::spend(
            Some(profile),
            used_usd,
            *limit,
            now_ms,
        ));
    }
    readings
}

/// The highest step at or below `used` as a percent of `limit`. `None` below the lowest step or
/// when the limit is not positive. Usage over the limit crosses steps above 100.
#[must_use]
pub fn crossed_step(steps: &[u32], used: f64, limit: f64) -> Option<u32> {
    if !(limit > 0.0 && used.is_finite()) {
        return None;
    }
    steps
        .iter()
        .copied()
        .filter(|step| f64::from(*step) * limit <= used * FULL_PERCENT)
        .max()
}

/// One notification per reading at its highest crossed step. The id names the profile, quota,
/// period and step, so the inbox keeps one per step and period however often this runs.
#[must_use]
pub fn evaluate(readings: &[BudgetReading], steps: &[u32], now_ms: u64) -> Vec<Notification> {
    readings
        .iter()
        .filter_map(|reading| {
            let step = crossed_step(steps, reading.used, reading.limit)?;
            Some(notification(reading, step, now_ms))
        })
        .collect()
}

/// The UTC calendar month of `ms`, `YYYY-MM`.
#[must_use]
pub fn period_of(ms: u64) -> String {
    let mut stamp = utc_iso8601(ms);
    stamp.truncate(PERIOD_CHARS);
    stamp
}

/// Unix ms of the first instant of the UTC calendar month of `ms`, the start of month-to-date spend.
#[must_use]
pub fn month_start_ms(ms: u64) -> u64 {
    let day: u64 = utc_iso8601(ms)
        .get(DAY_OF_MONTH)
        .and_then(|day| day.parse().ok())
        .unwrap_or(1);
    let midnight = ms - ms % MS_PER_DAY;
    midnight.saturating_sub(day.saturating_sub(1) * MS_PER_DAY)
}

/// The id of the notification for `reading` at `step`.
#[must_use]
pub fn notification_id(reading: &BudgetReading, step: u32) -> String {
    format!(
        "budget:{}:{}:{}:{step}",
        reading.profile.as_deref().unwrap_or(GLOBAL),
        reading.quota,
        reading.period
    )
}

fn period_of_date(date: &str) -> Option<String> {
    let period = date.get(..PERIOD_CHARS)?;
    let (year, month) = period.split_once('-')?;
    let valid = year.len() == 4
        && month.len() == 2
        && year
            .bytes()
            .chain(month.bytes())
            .all(|b| b.is_ascii_digit());
    valid.then(|| period.to_owned())
}

fn notification(reading: &BudgetReading, step: u32, now_ms: u64) -> Notification {
    let severity = if step >= 100 {
        NotificationSeverity::Critical
    } else {
        NotificationSeverity::Warning
    };
    Notification {
        id: notification_id(reading, step),
        kind: NotificationKind::Advisory,
        source: SOURCE.to_owned(),
        severity,
        title: title(reading, step),
        body: body(reading),
        profile: reading.profile.clone(),
        model: None,
        rule: Some(reading.quota.clone()),
        diff: None,
        actions: Notification::default_actions(NotificationKind::Advisory),
        status: NotificationStatus::Pending,
        created_ms: now_ms,
        updated_ms: now_ms,
        result: None,
        muted: false,
    }
}

fn title(reading: &BudgetReading, step: u32) -> String {
    let text = if reading.quota == SPEND_QUOTA {
        let subject = match &reading.profile {
            Some(profile) => format!("{profile} monthly spend"),
            None => "Monthly spend".to_owned(),
        };
        format!(
            "{subject} at {step}% ({} of {})",
            usd_short(reading.used),
            usd_short(reading.limit)
        )
    } else {
        let quota = quota_label(&reading.quota);
        match &reading.profile {
            Some(profile) => format!("{profile} {quota} at {step}%"),
            None => format!("{quota} at {step}%"),
        }
    };
    capitalize(&text)
}

fn body(reading: &BudgetReading) -> String {
    let percent = reading.used / reading.limit * FULL_PERCENT;
    let left = reading.limit - reading.used;
    let rest = if left >= 0.0 {
        format!("{} left", amount(reading.unit, left))
    } else {
        format!("over by {}", amount(reading.unit, -left))
    };
    format!(
        "Used {} of {} ({percent:.0}%), {rest} in {}.",
        amount(reading.unit, reading.used),
        amount(reading.unit, reading.limit),
        reading.period
    )
}

fn quota_label(quota: &str) -> String {
    match quota {
        "premium_interactions" => "premium requests".to_owned(),
        other => other.replace('_', " "),
    }
}

fn amount(unit: BudgetUnit, value: f64) -> String {
    match unit {
        BudgetUnit::Usd => format!("${value:.2}"),
        BudgetUnit::Requests => format!("{} requests", count(value)),
        BudgetUnit::Credits => format!("{} credits", count(value)),
        BudgetUnit::Tokens => format!("{} tokens", count(value)),
    }
}

/// A whole number without decimals, any other with one.
fn count(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}

/// Whole dollars without cents, any other amount with them.
fn usd_short(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("${value:.0}")
    } else {
        format!("${value:.2}")
    }
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const STEPS: [u32; 7] = [25, 50, 75, 80, 85, 95, 100];
    /// 2026-11-21T12:00:00Z.
    const NOV_21_NOON: u64 = 1_795_262_400_000;

    fn quota(
        id: &str,
        unit: QuotaUnit,
        used: f64,
        limit: f64,
        reset: Option<&str>,
    ) -> ProfileUsageQuota {
        ProfileUsageQuota {
            id: id.into(),
            unit,
            used,
            entitlement: limit,
            remaining: None,
            remaining_percentage: 0.0,
            overage_permitted: None,
            reset_date: reset.map(str::to_owned),
            used_usd: None,
            limit_usd: None,
        }
    }

    #[test]
    fn the_test_clock_is_november_21_noon_utc() {
        assert_eq!(utc_iso8601(NOV_21_NOON), "2026-11-21T12:00:00.000Z");
    }

    #[test]
    fn crossed_step_picks_the_highest_step_at_or_below_the_percent() {
        assert_eq!(crossed_step(&STEPS, 0.0, 300.0), None);
        assert_eq!(crossed_step(&STEPS, 74.0, 300.0), None);
        assert_eq!(crossed_step(&STEPS, 75.0, 300.0), Some(25));
        assert_eq!(crossed_step(&STEPS, 240.0, 300.0), Some(80));
        assert_eq!(crossed_step(&STEPS, 239.9, 300.0), Some(75));
        assert_eq!(crossed_step(&STEPS, 300.0, 300.0), Some(100));
        assert_eq!(crossed_step(&STEPS, 0.3, 0.1), Some(100));
    }

    #[test]
    fn crossed_step_reaches_steps_over_100_when_usage_exceeds_the_limit() {
        let steps = [50, 100, 150, 1000];
        assert_eq!(crossed_step(&steps, 140.0, 100.0), Some(100));
        assert_eq!(crossed_step(&steps, 150.0, 100.0), Some(150));
        assert_eq!(crossed_step(&steps, 5_000.0, 100.0), Some(1000));
    }

    #[test]
    fn crossed_step_needs_a_positive_limit_and_a_finite_use() {
        assert_eq!(crossed_step(&STEPS, 10.0, 0.0), None);
        assert_eq!(crossed_step(&STEPS, 10.0, -5.0), None);
        assert_eq!(crossed_step(&STEPS, 10.0, f64::NAN), None);
        assert_eq!(crossed_step(&STEPS, f64::NAN, 10.0), None);
        assert_eq!(crossed_step(&STEPS, f64::INFINITY, 10.0), None);
        assert_eq!(crossed_step(&[], 10.0, 10.0), None);
    }

    #[test]
    fn period_is_the_utc_month() {
        assert_eq!(period_of(NOV_21_NOON), "2026-11");
        assert_eq!(period_of(0), "1970-01");
        // 2026-12-31T23:59:59.999Z and one ms later.
        assert_eq!(period_of(1_798_761_599_999), "2026-12");
        assert_eq!(period_of(1_798_761_600_000), "2027-01");
    }

    #[test]
    fn month_start_is_midnight_on_the_first() {
        assert_eq!(
            utc_iso8601(month_start_ms(NOV_21_NOON)),
            "2026-11-01T00:00:00.000Z"
        );
        assert_eq!(month_start_ms(1_798_761_600_000), 1_798_761_600_000);
        assert_eq!(month_start_ms(0), 0);
    }

    #[test]
    fn a_usage_quota_takes_the_reset_month_as_its_period() {
        let premium = quota(
            "premium_interactions",
            QuotaUnit::Requests,
            240.0,
            300.0,
            Some("2026-12-01T00:00:00Z"),
        );
        let reading = BudgetReading::from_usage_quota("copilot", &premium, NOV_21_NOON);
        assert_eq!(
            reading,
            BudgetReading {
                profile: Some("copilot".into()),
                quota: "premium_interactions".into(),
                used: 240.0,
                limit: 300.0,
                period: "2026-12".into(),
                unit: BudgetUnit::Requests,
            }
        );
    }

    #[test]
    fn a_usage_quota_without_a_usable_reset_date_takes_the_calendar_month() {
        for reset in [None, Some("soon"), Some("20x6-12-01"), Some("2026-1")] {
            let q = quota("q", QuotaUnit::Tokens, 1.0, 2.0, reset);
            let reading = BudgetReading::from_usage_quota("p", &q, NOV_21_NOON);
            assert_eq!(reading.period, "2026-11", "{reset:?}");
        }
    }

    #[test]
    fn usage_quota_units_map_to_budget_units() {
        for (unit, expected) in [
            (QuotaUnit::Aic, BudgetUnit::Credits),
            (QuotaUnit::Requests, BudgetUnit::Requests),
            (QuotaUnit::Tokens, BudgetUnit::Tokens),
            (QuotaUnit::Usd, BudgetUnit::Usd),
        ] {
            let q = quota("q", unit, 1.0, 2.0, None);
            assert_eq!(BudgetReading::from_usage_quota("p", &q, 0).unit, expected);
        }
    }

    #[test]
    fn spend_readings_sum_every_profile_for_the_global_budget() {
        let spend = HashMap::from([("a".to_owned(), 150.0), ("b".to_owned(), 40.0)]);
        let limits = vec![("b".to_owned(), 50.0), ("idle".to_owned(), 10.0)];
        let readings = spend_readings(Some(200.0), &limits, &spend, NOV_21_NOON);
        assert_eq!(readings.len(), 3);
        assert_eq!(readings[0].profile, None);
        assert!((readings[0].used - 190.0).abs() < 1e-9);
        assert_eq!(readings[1].profile.as_deref(), Some("b"));
        assert!((readings[1].used - 40.0).abs() < 1e-9);
        assert_eq!(readings[2].profile.as_deref(), Some("idle"));
        assert!(readings[2].used.abs() < f64::EPSILON);
        assert!(
            readings
                .iter()
                .all(|r| r.quota == SPEND_QUOTA && r.unit == BudgetUnit::Usd)
        );
        assert!(spend_readings(None, &[], &spend, NOV_21_NOON).is_empty());
    }

    #[test]
    fn evaluate_raises_one_notification_per_reading_at_its_highest_step() {
        let readings = vec![
            BudgetReading::spend(None, 190.0, 200.0, NOV_21_NOON),
            BudgetReading::spend(Some("b"), 1.0, 50.0, NOV_21_NOON),
            BudgetReading::from_usage_quota(
                "copilot",
                &quota(
                    "premium_interactions",
                    QuotaUnit::Requests,
                    240.0,
                    300.0,
                    Some("2026-12-01T00:00:00Z"),
                ),
                NOV_21_NOON,
            ),
        ];
        let notes = evaluate(&readings, &STEPS, NOV_21_NOON);
        assert_eq!(notes.len(), 2);

        let spend = &notes[0];
        assert_eq!(spend.id, "budget:global:spend_usd:2026-11:95");
        assert_eq!(spend.title, "Monthly spend at 95% ($190 of $200)");
        assert_eq!(
            spend.body,
            "Used $190.00 of $200.00 (95%), $10.00 left in 2026-11."
        );
        assert_eq!(spend.severity, NotificationSeverity::Warning);
        assert_eq!(spend.kind, NotificationKind::Advisory);
        assert_eq!(spend.source, "budget");
        assert_eq!(spend.profile, None);
        assert_eq!(spend.rule.as_deref(), Some("spend_usd"));
        assert_eq!(
            spend.actions,
            Notification::default_actions(NotificationKind::Advisory)
        );
        assert_eq!(spend.status, NotificationStatus::Pending);
        assert_eq!(
            (spend.created_ms, spend.updated_ms),
            (NOV_21_NOON, NOV_21_NOON)
        );
        assert!(!spend.muted && spend.diff.is_none() && spend.result.is_none());

        let premium = &notes[1];
        assert_eq!(premium.id, "budget:copilot:premium_interactions:2026-12:80");
        assert_eq!(premium.title, "Copilot premium requests at 80%");
        assert_eq!(
            premium.body,
            "Used 240 requests of 300 requests (80%), 60 requests left in 2026-12."
        );
        assert_eq!(premium.profile.as_deref(), Some("copilot"));
    }

    #[test]
    fn ids_stay_the_same_across_runs_and_change_with_the_step() {
        let first = evaluate(
            &[BudgetReading::spend(Some("a"), 81.0, 100.0, NOV_21_NOON)],
            &STEPS,
            NOV_21_NOON,
        );
        let later = evaluate(
            &[BudgetReading::spend(
                Some("a"),
                84.0,
                100.0,
                NOV_21_NOON + 60_000,
            )],
            &STEPS,
            NOV_21_NOON + 60_000,
        );
        assert_eq!(first[0].id, later[0].id);
        assert_eq!(first[0].id, "budget:a:spend_usd:2026-11:80");
        let next = evaluate(
            &[BudgetReading::spend(Some("a"), 85.0, 100.0, NOV_21_NOON)],
            &STEPS,
            NOV_21_NOON,
        );
        assert_eq!(next[0].id, "budget:a:spend_usd:2026-11:85");
    }

    #[test]
    fn at_or_over_the_limit_is_critical() {
        let at = evaluate(
            &[BudgetReading::spend(Some("a"), 100.0, 100.0, NOV_21_NOON)],
            &STEPS,
            NOV_21_NOON,
        );
        assert_eq!(at[0].severity, NotificationSeverity::Critical);
        assert_eq!(at[0].title, "A monthly spend at 100% ($100 of $100)");
        assert!(at[0].body.contains("$0.00 left"), "{}", at[0].body);

        let over = evaluate(
            &[BudgetReading::spend(None, 312.5, 250.0, NOV_21_NOON)],
            &[50, 100, 125],
            NOV_21_NOON,
        );
        assert_eq!(over[0].id, "budget:global:spend_usd:2026-11:125");
        assert_eq!(over[0].severity, NotificationSeverity::Critical);
        assert_eq!(over[0].title, "Monthly spend at 125% ($312.50 of $250)");
        assert_eq!(
            over[0].body,
            "Used $312.50 of $250.00 (125%), over by $62.50 in 2026-11."
        );
    }

    #[test]
    fn other_quotas_and_units_render_their_names_and_amounts() {
        let readings = [
            BudgetReading {
                profile: Some("copilot".into()),
                quota: "ai_credits".into(),
                used: 2_550.5,
                limit: 3_000.0,
                period: "2026-11".into(),
                unit: BudgetUnit::Credits,
            },
            BudgetReading {
                profile: None,
                quota: "tokens_month".into(),
                used: 900.0,
                limit: 1_000.0,
                period: "2026-11".into(),
                unit: BudgetUnit::Tokens,
            },
        ];
        let notes = evaluate(&readings, &STEPS, 0);
        assert_eq!(notes[0].title, "Copilot ai credits at 85%");
        assert_eq!(
            notes[0].body,
            "Used 2550.5 credits of 3000 credits (85%), 449.5 credits left in 2026-11."
        );
        assert_eq!(notes[1].title, "Tokens month at 85%");
        assert_eq!(notes[1].id, "budget:global:tokens_month:2026-11:85");
        assert!(
            notes[1].body.contains("100 tokens left"),
            "{}",
            notes[1].body
        );
    }

    #[test]
    fn capitalize_handles_an_empty_title() {
        assert_eq!(capitalize(""), "");
        assert_eq!(capitalize("über"), "Über");
    }
}
