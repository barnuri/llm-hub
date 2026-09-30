use std::time::Instant;

/// Milliseconds elapsed since `started`, saturating at `u64::MAX`.
pub fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

const MS_PER_SECOND: u64 = 1_000;
const SECONDS_PER_DAY: u64 = 86_400;
/// Days from 0000-03-01 to 1970-01-01 in the proleptic Gregorian calendar.
const UNIX_EPOCH_DAYS_FROM_MARCH_0000: i64 = 719_468;
const DAYS_PER_ERA: i64 = 146_097;

/// Unix milliseconds as an RFC 3339 UTC timestamp, e.g. `2026-09-30T20:15:00.123Z`.
#[must_use]
pub fn utc_iso8601(unix_ms: u64) -> String {
    let seconds = unix_ms / MS_PER_SECOND;
    let millis = unix_ms % MS_PER_SECOND;
    let day_seconds = seconds % SECONDS_PER_DAY;
    let (year, month, day) =
        civil_from_days(i64::try_from(seconds / SECONDS_PER_DAY).unwrap_or(i64::MAX));
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        day_seconds / 3_600,
        day_seconds % 3_600 / 60,
        day_seconds % 60
    )
}

/// Howard Hinnant's days-to-civil conversion for days since 1970-01-01.
fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let shifted = days_since_epoch + UNIX_EPOCH_DAYS_FROM_MARCH_0000;
    let era = shifted.div_euclid(DAYS_PER_ERA);
    let day_of_era = shifted - era * DAYS_PER_ERA;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_iso8601__epoch_and_known_instants() {
        assert_eq!(utc_iso8601(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(utc_iso8601(951_782_400_000), "2000-02-29T00:00:00.000Z");
        assert_eq!(utc_iso8601(1_790_792_466_123), "2026-09-30T18:21:06.123Z");
    }

    #[test]
    fn elapsed_is_non_negative_and_small_for_fresh_instant() {
        assert!(elapsed_ms(Instant::now()) < 1_000);
    }
}
