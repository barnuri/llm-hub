//! USD-per-1M-token rate types and env parsing (kept in configs to avoid a
//! services ↔ configs cycle).

use std::collections::HashMap;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
#[allow(clippy::struct_field_names)] // intentional USD-per-1M suffix on every rate field
pub struct TokenRates {
    pub input_per_1m: f64,
    pub output_per_1m: f64,
    pub cache_read_per_1m: f64,
    pub cache_write_per_1m: f64,
}

impl TokenRates {
    #[must_use]
    pub fn is_zero(self) -> bool {
        self.input_per_1m == 0.0
            && self.output_per_1m == 0.0
            && self.cache_read_per_1m == 0.0
            && self.cache_write_per_1m == 0.0
    }

    /// Fill missing cache rates from input when the caller only set in/out.
    #[must_use]
    pub fn with_cache_defaults(self) -> Self {
        let cache_read = if self.cache_read_per_1m > 0.0 {
            self.cache_read_per_1m
        } else if self.input_per_1m > 0.0 {
            self.input_per_1m * 0.1
        } else {
            0.0
        };
        let cache_write = if self.cache_write_per_1m > 0.0 {
            self.cache_write_per_1m
        } else {
            self.input_per_1m
        };
        Self {
            cache_read_per_1m: cache_read,
            cache_write_per_1m: cache_write,
            ..self
        }
    }
}

/// Parse `model:in/out`, `model:in/out/cache_read`, or `model:in/out/cache_read/cache_write`.
pub fn parse_model_prices(raw: &str) -> Result<HashMap<String, TokenRates>, String> {
    let mut out = HashMap::new();
    for part in raw.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (model, rates_raw) = part
            .split_once(':')
            .ok_or_else(|| format!("model price entry must be model:in/out[...]: {part}"))?;
        let model = model.trim();
        if model.is_empty() {
            return Err(format!("empty model name in price entry: {part}"));
        }
        out.insert(model.to_string(), parse_rate_list(rates_raw)?);
    }
    Ok(out)
}

fn parse_rate_list(raw: &str) -> Result<TokenRates, String> {
    let nums: Vec<&str> = raw
        .split('/')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if !(2..=4).contains(&nums.len()) {
        return Err(format!(
            "price rates must be in/out or in/out/cache_read[/cache_write], got: {raw}"
        ));
    }
    let parse = |s: &str| {
        s.parse::<f64>()
            .map_err(|_| format!("not a number in price rates: {s}"))
    };
    Ok(TokenRates {
        input_per_1m: parse(nums[0])?,
        output_per_1m: parse(nums[1])?,
        cache_read_per_1m: nums.get(2).map(|s| parse(s)).transpose()?.unwrap_or(0.0),
        cache_write_per_1m: nums.get(3).map(|s| parse(s)).transpose()?.unwrap_or(0.0),
    })
}

pub fn parse_optional_f64(raw: Option<String>, label: &str) -> Result<f64, String> {
    match raw {
        None => Ok(0.0),
        Some(v) if v.trim().is_empty() => Ok(0.0),
        Some(v) => v
            .trim()
            .parse()
            .map_err(|_| format!("{label} is not a number: {v}")),
    }
}

pub fn profile_rates_from_env(
    profile: &str,
    get: &dyn Fn(&str) -> Option<String>,
) -> Result<(TokenRates, HashMap<String, TokenRates>), String> {
    let upper = profile.to_uppercase().replace('-', "_");
    let input = parse_optional_f64(
        get("INPUT_USD_PER_1M"),
        &format!("LLM_HUB_{upper}_INPUT_USD_PER_1M"),
    )?;
    let output = parse_optional_f64(
        get("OUTPUT_USD_PER_1M"),
        &format!("LLM_HUB_{upper}_OUTPUT_USD_PER_1M"),
    )?;
    let cache_read = parse_optional_f64(
        get("CACHE_READ_USD_PER_1M"),
        &format!("LLM_HUB_{upper}_CACHE_READ_USD_PER_1M"),
    )?;
    let cache_write = parse_optional_f64(
        get("CACHE_WRITE_USD_PER_1M"),
        &format!("LLM_HUB_{upper}_CACHE_WRITE_USD_PER_1M"),
    )?;
    let model_prices = match get("MODEL_PRICES") {
        Some(raw) if !raw.trim().is_empty() => parse_model_prices(&raw)?,
        _ => HashMap::new(),
    };
    Ok((
        TokenRates {
            input_per_1m: input,
            output_per_1m: output,
            cache_read_per_1m: cache_read,
            cache_write_per_1m: cache_write,
        },
        model_prices,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_model_prices_list() {
        let map = parse_model_prices("glm:0.1/0.2, other:1/2/0.1/1.5").unwrap();
        assert!((map["glm"].input_per_1m - 0.1).abs() < f64::EPSILON);
        assert!((map["other"].cache_write_per_1m - 1.5).abs() < f64::EPSILON);
    }
}
