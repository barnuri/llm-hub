use serde_json::{Value, json};

/// The USD per one million tokens the hub lists for a model in `llm_hub_price`.
#[derive(Debug, Clone, PartialEq)]
pub struct HubModelPrice {
    input: f64,
    output: f64,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
}

impl HubModelPrice {
    /// A price with only input and output rates.
    #[cfg(test)]
    #[must_use]
    pub fn new(input: f64, output: f64) -> Self {
        Self {
            input,
            output,
            cache_read: None,
            cache_write: None,
        }
    }

    /// The price in a `/v1/models` entry, when it has input and output rates.
    #[must_use]
    pub fn from_entry(entry: &Value) -> Option<Self> {
        let price = entry.get("llm_hub_price")?;
        let rate = |key: &str| price.get(key).and_then(Value::as_f64);
        Some(Self {
            input: rate("input_usd_per_1m")?,
            output: rate("output_usd_per_1m")?,
            cache_read: rate("cache_read_usd_per_1m"),
            cache_write: rate("cache_write_usd_per_1m"),
        })
    }

    /// True when the input rate is zero, which opencode's model picker counts as a free model.
    #[must_use]
    pub fn has_free_input(&self) -> bool {
        self.input == 0.0
    }

    /// The price as an opencode model `cost` object.
    #[must_use]
    pub fn to_opencode_cost(&self) -> Value {
        let mut cost = json!({ "input": self.input, "output": self.output });
        if let Some(rate) = self.cache_read {
            cost["cache_read"] = json!(rate);
        }
        if let Some(rate) = self.cache_write {
            cost["cache_write"] = json!(rate);
        }
        cost
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_price_maps_every_rate_to_the_opencode_cost() {
        let entry = json!({"llm_hub_price": {
            "input_usd_per_1m": 4.0, "output_usd_per_1m": 20.0,
            "cache_read_usd_per_1m": 0.2, "cache_write_usd_per_1m": 5.0,
        }});

        let cost = HubModelPrice::from_entry(&entry)
            .unwrap()
            .to_opencode_cost();

        assert_eq!(
            cost,
            json!({"input": 4.0, "output": 20.0, "cache_read": 0.2, "cache_write": 5.0})
        );
    }

    #[test]
    fn price_without_cache_rates_omits_them() {
        let cost = HubModelPrice::new(1.0, 2.0).to_opencode_cost();

        assert_eq!(cost, json!({"input": 1.0, "output": 2.0}));
    }

    #[test]
    fn entry_without_output_rate_has_no_price() {
        let entry = json!({"llm_hub_price": {"input_usd_per_1m": 1.0}});

        assert_eq!(HubModelPrice::from_entry(&entry), None);
        assert_eq!(HubModelPrice::from_entry(&json!({})), None);
    }
}
