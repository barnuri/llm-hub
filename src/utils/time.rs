use std::time::Instant;

/// Milliseconds elapsed since `started`, saturating at `u64::MAX`.
pub fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed_is_non_negative_and_small_for_fresh_instant() {
        assert!(elapsed_ms(Instant::now()) < 1_000);
    }
}
