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

/// An RFC 3339 timestamp (`2026-09-30T18:21:06.5Z`, `...+02:00`) as Unix milliseconds.
#[must_use]
pub fn parse_rfc3339_ms(text: &str) -> Option<i64> {
    let text = text.trim();
    let (date, rest) = text.split_once(['T', 't', ' '])?;
    let mut date_parts = date.splitn(3, '-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: i64 = date_parts.next()?.parse().ok()?;
    let day: i64 = date_parts.next()?.parse().ok()?;
    let zone_at = rest.find(['Z', 'z', '+', '-'])?;
    let (clock, zone) = rest.split_at(zone_at);
    let offset_seconds = match zone {
        "Z" | "z" => 0,
        _ => {
            let sign = if zone.starts_with('-') { -1 } else { 1 };
            let (hours, minutes) = zone[1..].split_once(':')?;
            sign * (hours.parse::<i64>().ok()? * 3_600 + minutes.parse::<i64>().ok()? * 60)
        }
    };
    let (whole, fraction) = clock.split_once('.').unwrap_or((clock, ""));
    let mut clock_parts = whole.splitn(3, ':');
    let hour: i64 = clock_parts.next()?.parse().ok()?;
    let minute: i64 = clock_parts.next()?.parse().ok()?;
    let second: i64 = clock_parts.next()?.parse().ok()?;
    let millis: i64 = format!("{fraction:0<3}").get(..3)?.parse().ok()?;
    let seconds = unix_seconds(year, month, day, hour * 3_600 + minute * 60 + second)?;
    Some((seconds - offset_seconds) * 1_000 + millis)
}

/// An HTTP date (`Sun, 06 Nov 1994 08:49:37 GMT`, RFC 9110 IMF-fixdate) as Unix milliseconds.
#[must_use]
pub fn parse_http_date_ms(text: &str) -> Option<i64> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let mut words = text.split_whitespace().skip(1);
    let day: i64 = words.next()?.parse().ok()?;
    let month_name = words.next()?;
    let month = MONTHS.iter().position(|name| *name == month_name)?;
    let year: i64 = words.next()?.parse().ok()?;
    let mut clock = words.next()?.splitn(3, ':');
    let hour: i64 = clock.next()?.parse().ok()?;
    let minute: i64 = clock.next()?.parse().ok()?;
    let second: i64 = clock.next()?.parse().ok()?;
    if words.next()? != "GMT" {
        return None;
    }
    let month = i64::try_from(month).ok()? + 1;
    unix_seconds(year, month, day, hour * 3_600 + minute * 60 + second).map(|s| s * 1_000)
}

/// Seconds since 1970-01-01 of a UTC date and second of day, or `None` for an impossible date.
fn unix_seconds(year: i64, month: i64, day: i64, second_of_day: i64) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || second_of_day >= 86_401 {
        return None;
    }
    let shifted_year = if month <= 2 { year - 1 } else { year };
    let era = shifted_year.div_euclid(400);
    let year_of_era = shifted_year - era * 400;
    let month_from_march = (month + 9) % 12;
    let day_of_year = (153 * month_from_march + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * DAYS_PER_ERA + day_of_era - UNIX_EPOCH_DAYS_FROM_MARCH_0000;
    Some(days * 86_400 + second_of_day)
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
    fn utc_iso8601_epoch_and_known_instants() {
        assert_eq!(utc_iso8601(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(utc_iso8601(951_782_400_000), "2000-02-29T00:00:00.000Z");
        assert_eq!(utc_iso8601(1_790_792_466_123), "2026-09-30T18:21:06.123Z");
    }

    #[test]
    fn elapsed_is_non_negative_and_small_for_fresh_instant() {
        assert!(elapsed_ms(Instant::now()) < 1_000);
    }

    #[test]
    fn rfc3339_parses_zones_and_fractions() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_ms("2026-09-30T18:21:06.123Z"),
            Some(1_790_792_466_123)
        );
        assert_eq!(
            parse_rfc3339_ms("2026-09-30T20:21:06.1+02:00"),
            Some(1_790_792_466_100)
        );
        assert_eq!(
            parse_rfc3339_ms("2026-09-30T13:21:06-05:00"),
            Some(1_790_792_466_000)
        );
        assert_eq!(
            parse_rfc3339_ms("2000-02-29T00:00:00Z"),
            Some(951_782_400_000)
        );
        for bad in [
            "",
            "2026-09-30",
            "2026-13-01T00:00:00Z",
            "2026-09-30T00:00Z",
            "x",
        ] {
            assert_eq!(parse_rfc3339_ms(bad), None, "{bad}");
        }
    }

    #[test]
    fn http_dates_parse_and_reject_other_zones() {
        assert_eq!(
            parse_http_date_ms("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784_111_777_000)
        );
        assert_eq!(parse_http_date_ms("Sun, 06 Nov 1994 08:49:37 PST"), None);
        assert_eq!(parse_http_date_ms("Sun, 06 Foo 1994 08:49:37 GMT"), None);
        assert_eq!(parse_http_date_ms("120"), None);
    }
}
