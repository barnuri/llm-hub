//! Prefill-time fit and estimate per local model, as pure functions.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "Token counts and milliseconds stay far below 2^52, and float-to-int casts saturate"
)]

/// Upper bounds (inclusive, in tokens) of the log-size buckets; larger samples share one bucket.
const BUCKET_BOUNDS: [u64; 7] = [2_048, 4_096, 8_192, 16_384, 32_768, 65_536, 131_072];

/// A fit needs this many samples before it counts as confident.
const MIN_SAMPLES: usize = 3;

/// A fit needs this many non-empty buckets before it counts as confident.
const MIN_BUCKETS: usize = 2;

/// An estimate beyond this multiple of the largest seen prompt counts as far extrapolation.
const MAX_EXTRAPOLATION: u64 = 8;

/// The fit runs on thousands of tokens so the normal equations stay well-conditioned.
const SCALE: f64 = 1_000.0;

/// One measured request: uncached prompt tokens and the time to first token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrefillSample {
    /// Uncached prompt tokens the model had to prefill.
    pub tokens: u64,
    /// Time to first token in milliseconds.
    pub prefill_ms: u64,
}

/// Fitted prefill curve `time_ms = a·n + b·n²` for `n` uncached prompt tokens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PrefillFit {
    /// Linear cost in milliseconds per token, never negative.
    pub a: f64,
    /// Quadratic cost in milliseconds per token squared, never negative.
    pub b: f64,
    /// True when the fit rests on too few samples or buckets to reject a request on.
    pub low_confidence: bool,
    /// Largest prompt size among the samples, in tokens.
    pub max_tokens_seen: u64,
}

impl PrefillFit {
    /// Expected prefill time in milliseconds for `tokens` uncached prompt tokens.
    #[must_use]
    pub fn estimate_ms(&self, tokens: u64) -> u64 {
        let n = tokens as f64;
        // `as` saturates on overflow and maps NaN to 0.
        (self.a * n + self.b * n * n).round() as u64
    }

    /// Fitted marginal prefill speed at `tokens`, in tokens per second.
    #[must_use]
    pub fn marginal_tok_per_s(&self, tokens: u64) -> f64 {
        let ms_per_token = self.a + 2.0 * self.b * tokens as f64;
        if ms_per_token > 0.0 {
            1_000.0 / ms_per_token
        } else {
            f64::INFINITY
        }
    }

    /// Largest multiple of `step` tokens whose estimate fits in `budget_ms`; a `step` of 0 acts as 1.
    #[must_use]
    pub fn max_tokens_within(&self, budget_ms: u64, step: u64) -> u64 {
        let step = step.max(1);
        let budget = budget_ms as f64;
        let exact = if self.b > 0.0 {
            (-self.a + (self.a * self.a + 4.0 * self.b * budget).sqrt()) / (2.0 * self.b)
        } else if self.a > 0.0 {
            budget / self.a
        } else {
            f64::INFINITY
        };
        let mut n = (exact as u64 / step).saturating_mul(step);
        // Rounding in `estimate_ms` may push the boundary step just over budget.
        while n > 0 && self.estimate_ms(n) > budget_ms {
            n = n.saturating_sub(step);
        }
        n
    }

    /// True when the fit is confident and `tokens` is not far beyond the largest seen prompt.
    #[must_use]
    pub fn confident_at(&self, tokens: u64) -> bool {
        !self.low_confidence && tokens <= self.max_tokens_seen.saturating_mul(MAX_EXTRAPOLATION)
    }
}

/// Fits `time_ms = a·n + b·n²` with `a, b ≥ 0` on per-bucket medians.
///
/// Samples with 0 tokens are dropped. Returns `None` with no usable sample or when every
/// coefficient fits to 0. One bucket fits linear `a·n` only.
#[must_use]
pub fn fit(samples: &[PrefillSample]) -> Option<PrefillFit> {
    let usable: Vec<&PrefillSample> = samples.iter().filter(|s| s.tokens > 0).collect();
    let max_tokens_seen = usable.iter().map(|s| s.tokens).max()?;
    let mut buckets: Vec<Vec<&PrefillSample>> = vec![Vec::new(); BUCKET_BOUNDS.len() + 1];
    for s in &usable {
        let idx = BUCKET_BOUNDS
            .iter()
            .position(|&bound| s.tokens <= bound)
            .unwrap_or(BUCKET_BOUNDS.len());
        if let Some(bucket) = buckets.get_mut(idx) {
            bucket.push(s);
        }
    }
    let points: Vec<(f64, f64)> = buckets
        .iter()
        .filter(|b| !b.is_empty())
        .map(|b| {
            let n = median(b.iter().map(|s| s.tokens).collect()) / SCALE;
            let t = median(b.iter().map(|s| s.prefill_ms).collect());
            (n, t)
        })
        .collect();
    let (a_k, b_k) = if points.len() >= 2 {
        nnls_quadratic(&points)
    } else {
        (linear_only(&points), 0.0)
    };
    if !(a_k > 0.0 || b_k > 0.0) {
        return None;
    }
    Some(PrefillFit {
        a: a_k / SCALE,
        b: b_k / (SCALE * SCALE),
        low_confidence: usable.len() < MIN_SAMPLES || points.len() < MIN_BUCKETS,
        max_tokens_seen,
    })
}

/// Median of `values`: the mean of the two middle values for an even count, 0 when empty.
fn median(mut values: Vec<u64>) -> f64 {
    values.sort_unstable();
    let mid = values.len() / 2;
    match (values.get(mid.wrapping_sub(1)), values.get(mid)) {
        (Some(&lo), Some(&hi)) if values.len().is_multiple_of(2) => {
            f64::midpoint(lo as f64, hi as f64)
        }
        (_, Some(&v)) => v as f64,
        _ => 0.0,
    }
}

/// Least-squares `t = a·x` with `a ≥ 0`.
fn linear_only(points: &[(f64, f64)]) -> f64 {
    let sxx: f64 = points.iter().map(|(x, _)| x * x).sum();
    let sxt: f64 = points.iter().map(|(x, t)| x * t).sum();
    if sxx > 0.0 { (sxt / sxx).max(0.0) } else { 0.0 }
}

/// Least-squares `t = b·x²` with `b ≥ 0`.
fn quadratic_only(points: &[(f64, f64)]) -> f64 {
    let s4: f64 = points.iter().map(|(x, _)| x.powi(4)).sum();
    let s2t: f64 = points.iter().map(|(x, t)| x * x * t).sum();
    if s4 > 0.0 { (s2t / s4).max(0.0) } else { 0.0 }
}

/// Sum of squared residuals of `t = a·x + b·x²`.
fn residual(points: &[(f64, f64)], a: f64, b: f64) -> f64 {
    points
        .iter()
        .map(|(x, t)| (t - a * x - b * x * x).powi(2))
        .sum()
}

/// Non-negative least squares for `t = a·x + b·x²`: the free solution, else the best boundary fit.
fn nnls_quadratic(points: &[(f64, f64)]) -> (f64, f64) {
    let s2: f64 = points.iter().map(|(x, _)| x * x).sum();
    let s3: f64 = points.iter().map(|(x, _)| x.powi(3)).sum();
    let s4: f64 = points.iter().map(|(x, _)| x.powi(4)).sum();
    let r1: f64 = points.iter().map(|(x, t)| x * t).sum();
    let r2: f64 = points.iter().map(|(x, t)| x * x * t).sum();
    let det = s2 * s4 - s3 * s3;
    if det > 1e-9 * s2 * s4 {
        let a = (r1 * s4 - r2 * s3) / det;
        let b = (s2 * r2 - s3 * r1) / det;
        if a >= 0.0 && b >= 0.0 {
            return (a, b);
        }
    }
    let linear = (linear_only(points), 0.0);
    let quadratic = (0.0, quadratic_only(points));
    if residual(points, linear.0, linear.1) <= residual(points, quadratic.0, quadratic.1) {
        linear
    } else {
        quadratic
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(tokens: u64, prefill_ms: u64) -> PrefillSample {
        PrefillSample { tokens, prefill_ms }
    }

    /// The diagnosed slow local model: cumulative prompt sizes and their TTFT.
    fn slow_curve() -> Vec<PrefillSample> {
        vec![
            s(4_096, 14_330),
            s(8_192, 47_700),
            s(12_288, 108_600),
            s(16_384, 207_700),
        ]
    }

    #[test]
    fn slow_curve_predicts_94k_over_15_minutes() {
        let fit = fit(&slow_curve()).expect("fit");
        assert!(!fit.low_confidence);
        assert_eq!(fit.max_tokens_seen, 16_384);
        assert!(fit.a >= 0.0 && fit.b > 0.0);
        assert!(fit.estimate_ms(94_000) > 15 * 60 * 1_000);
        assert!(fit.confident_at(94_000));
        assert!(!fit.confident_at(200_000));
    }

    #[test]
    fn slow_curve_tracks_measured_points_and_slows_down() {
        let fit = fit(&slow_curve()).expect("fit");
        let at_4k = fit.estimate_ms(4_096) as f64;
        assert!((at_4k - 14_330.0).abs() / 14_330.0 < 0.2, "{at_4k}");
        let fast = fit.marginal_tok_per_s(1_000);
        let slow = fit.marginal_tok_per_s(16_384);
        assert!(fast > slow && slow > 0.0, "{fast} {slow}");
    }

    #[test]
    fn fast_model_single_sample_is_linear_and_low_confidence() {
        let fit = fit(&[s(22_570, 53_500)]).expect("fit");
        assert!(fit.low_confidence);
        assert!(fit.b.abs() < f64::EPSILON);
        assert!(fit.estimate_ms(20_000) < 60_000);
        assert!(!fit.confident_at(20_000));
        let rate = fit.marginal_tok_per_s(20_000);
        assert!((rate - 22_570.0 / 53.5).abs() < 1.0, "{rate}");
    }

    #[test]
    fn two_samples_in_two_buckets_are_low_confidence() {
        let fit = fit(&[s(1_000, 2_000), s(8_000, 20_000)]).expect("fit");
        assert!(fit.low_confidence);
    }

    #[test]
    fn many_samples_in_one_bucket_are_low_confidence() {
        let fit = fit(&[s(1_000, 2_000), s(1_500, 3_000), s(2_000, 4_000)]).expect("fit");
        assert!(fit.low_confidence);
        assert_eq!(fit.estimate_ms(1_500), 3_000);
    }

    #[test]
    fn bucket_median_ignores_an_outlier() {
        let samples = [
            s(1_000, 1_000),
            s(1_000, 1_000),
            s(1_000, 900_000),
            s(10_000, 10_000),
        ];
        let fit = fit(&samples).expect("fit");
        assert!(fit.estimate_ms(1_000) < 2_000);
    }

    #[test]
    fn oversized_prompt_lands_in_overflow_bucket() {
        let fit = fit(&[s(1_000, 1_000), s(200_000, 200_000), s(300_000, 300_000)]).expect("fit");
        assert!(!fit.low_confidence);
        assert_eq!(fit.max_tokens_seen, 300_000);
    }

    #[test]
    fn negative_curvature_clamps_to_linear() {
        // Concave data would fit b < 0 unconstrained.
        let samples = [s(1_000, 10_000), s(4_000, 25_000), s(16_000, 40_000)];
        let fit = fit(&samples).expect("fit");
        assert!(fit.a > 0.0);
        assert!(fit.b.abs() < f64::EPSILON);
    }

    #[test]
    fn negative_linear_term_clamps_to_quadratic() {
        // Strongly convex data with an offset would fit a < 0 unconstrained.
        let samples = [s(1_000, 100), s(4_000, 1_000), s(16_000, 100_000)];
        let fit = fit(&samples).expect("fit");
        assert!(fit.a.abs() < f64::EPSILON);
        assert!(fit.b > 0.0);
    }

    #[test]
    fn empty_and_degenerate_inputs_return_none() {
        assert_eq!(fit(&[]), None);
        assert_eq!(fit(&[s(0, 5_000), s(0, 9_000)]), None);
        assert_eq!(fit(&[s(1_000, 0), s(8_000, 0), s(30_000, 0)]), None);
        assert_eq!(fit(&[s(1_000, 0)]), None);
    }

    #[test]
    fn max_tokens_within_inverts_the_estimate() {
        let fit = fit(&slow_curve()).expect("fit");
        let budget = 5 * 60 * 1_000;
        let n = fit.max_tokens_within(budget, 1_024);
        assert_eq!(n % 1_024, 0);
        assert!(n > 0);
        assert!(fit.estimate_ms(n) <= budget);
        assert!(fit.estimate_ms(n + 1_024) > budget);
        assert_eq!(fit.max_tokens_within(0, 1_024), 0);
    }

    #[test]
    fn max_tokens_within_handles_linear_zero_step_and_flat_fits() {
        let linear = PrefillFit {
            a: 2.0,
            b: 0.0,
            low_confidence: false,
            max_tokens_seen: 1_000,
        };
        assert_eq!(linear.max_tokens_within(10_000, 0), 5_000);
        assert_eq!(linear.max_tokens_within(10_001, 1_000), 5_000);
        let flat = PrefillFit {
            a: 0.0,
            b: 0.0,
            low_confidence: false,
            max_tokens_seen: 1_000,
        };
        assert_eq!(flat.max_tokens_within(1, 1_000), u64::MAX / 1_000 * 1_000);
        assert!(flat.marginal_tok_per_s(1_000).is_infinite());
    }

    #[test]
    fn median_handles_odd_and_even_counts() {
        assert!((median(vec![3, 1, 2]) - 2.0).abs() < f64::EPSILON);
        assert!((median(vec![4, 1, 3, 2]) - 2.5).abs() < f64::EPSILON);
        assert!(median(Vec::new()).abs() < f64::EPSILON);
    }
}
