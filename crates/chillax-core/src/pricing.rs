//! Model prices for estimating what a turn cost when the backend reports tokens but no
//! cost (ADR 0023). Pure: integer micro-dollars, no floats past config load.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The most one million tokens may cost, in USD. Rejects typos that would overflow.
const MAX_PER_MTOK: f64 = 1_000_000.0;

/// What `[pricing."model"]` says, as written in TOML: USD per million tokens.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct RawPrice {
    input_per_mtok: f64,
    output_per_mtok: f64,
    /// Cache reads; defaults to the input price.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cached_per_mtok: Option<f64>,
    /// Cache writes; defaults to the input price.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cache_write_per_mtok: Option<f64>,
}

/// USD per million tokens, as integer millionths of a dollar.
///
/// Assumes `input_tokens` EXCLUDES cached reads (they are priced by `cached`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawPrice", into = "RawPrice")]
pub struct ModelPrice {
    pub input_micros_per_mtok: u64,
    pub output_micros_per_mtok: u64,
    pub cached_micros_per_mtok: u64,
    pub cache_write_micros_per_mtok: u64,
}

fn micros(label: &str, value: f64) -> Result<u64, String> {
    if !value.is_finite() || value < 0.0 {
        return Err(format!("pricing {label} must be a non-negative number"));
    }
    if value > MAX_PER_MTOK {
        return Err(format!("pricing {label} is implausibly large"));
    }
    Ok((value * 1e6).round() as u64)
}

impl TryFrom<RawPrice> for ModelPrice {
    type Error = String;

    fn try_from(raw: RawPrice) -> Result<Self, String> {
        let input = micros("input_per_mtok", raw.input_per_mtok)?;
        Ok(Self {
            input_micros_per_mtok: input,
            output_micros_per_mtok: micros("output_per_mtok", raw.output_per_mtok)?,
            cached_micros_per_mtok: match raw.cached_per_mtok {
                Some(v) => micros("cached_per_mtok", v)?,
                None => input,
            },
            cache_write_micros_per_mtok: match raw.cache_write_per_mtok {
                Some(v) => micros("cache_write_per_mtok", v)?,
                None => input,
            },
        })
    }
}

impl From<ModelPrice> for RawPrice {
    fn from(p: ModelPrice) -> Self {
        let usd = |m: u64| m as f64 / 1e6;
        Self {
            input_per_mtok: usd(p.input_micros_per_mtok),
            output_per_mtok: usd(p.output_micros_per_mtok),
            cached_per_mtok: Some(usd(p.cached_micros_per_mtok)),
            cache_write_per_mtok: Some(usd(p.cache_write_micros_per_mtok)),
        }
    }
}

/// The configured prices, looked up by model name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PriceTable {
    prices: BTreeMap<String, ModelPrice>,
}

impl PriceTable {
    pub fn new(prices: &BTreeMap<String, ModelPrice>) -> Self {
        Self {
            prices: prices
                .iter()
                .map(|(k, v)| (k.trim().to_lowercase(), *v))
                .collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.prices.is_empty()
    }

    /// Exact match (case-insensitive), then the longest key that prefixes the model.
    /// A leading `provider/` segment is stripped on a second try, so
    /// `deepseek/deepseek-v4.1-flash` matches the key `deepseek-v4.1-flash`.
    pub fn lookup(&self, model: &str) -> Option<&ModelPrice> {
        let model = model.trim().to_lowercase();
        let stripped = model.split_once('/').map(|(_, rest)| rest);
        [Some(model.as_str()), stripped]
            .into_iter()
            .flatten()
            .find_map(|candidate| {
                self.prices.get(candidate).or_else(|| {
                    self.prices
                        .iter()
                        .filter(|(key, _)| !key.is_empty() && candidate.starts_with(key.as_str()))
                        .max_by_key(|(key, _)| key.len())
                        .map(|(_, price)| price)
                })
            })
    }

    /// Estimated cost in micro-USD, or `None` when the model has no price.
    pub fn estimate(
        &self,
        model: &str,
        input: u64,
        output: u64,
        cached: u64,
        cache_write: u64,
    ) -> Option<u64> {
        let p = self.lookup(model)?;
        let total = u128::from(input) * u128::from(p.input_micros_per_mtok)
            + u128::from(output) * u128::from(p.output_micros_per_mtok)
            + u128::from(cached) * u128::from(p.cached_micros_per_mtok)
            + u128::from(cache_write) * u128::from(p.cache_write_micros_per_mtok);
        Some(u64::try_from((total + 500_000) / 1_000_000).unwrap_or(u64::MAX))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn price(i: u64, o: u64, c: u64) -> ModelPrice {
        ModelPrice {
            input_micros_per_mtok: i,
            output_micros_per_mtok: o,
            cached_micros_per_mtok: c,
            cache_write_micros_per_mtok: i,
        }
    }

    fn table() -> PriceTable {
        PriceTable::new(&BTreeMap::from([
            (
                "Claude-Sonnet".to_string(),
                price(3_000_000, 15_000_000, 300_000),
            ),
            (
                "claude-sonnet-5".to_string(),
                price(1_000_000, 2_000_000, 100_000),
            ),
            (
                "deepseek-v4.1-flash".to_string(),
                price(100_000, 200_000, 10_000),
            ),
        ]))
    }

    #[test]
    fn exact_match_ignores_case() {
        let t = table();
        assert_eq!(
            t.lookup("CLAUDE-sonnet").unwrap().input_micros_per_mtok,
            3_000_000
        );
    }

    #[test]
    fn longest_prefix_wins() {
        let t = table();
        assert_eq!(
            t.lookup("claude-sonnet-5-20261001")
                .unwrap()
                .input_micros_per_mtok,
            1_000_000
        );
        assert_eq!(
            t.lookup("claude-sonnet-4-5").unwrap().input_micros_per_mtok,
            3_000_000
        );
    }

    #[test]
    fn provider_prefix_is_stripped() {
        let t = table();
        assert!(t.lookup("deepseek/deepseek-v4.1-flash").is_some());
    }

    #[test]
    fn unknown_models_have_no_price() {
        let t = table();
        assert!(t.lookup("gpt-9").is_none());
        assert!(t.lookup("sonnet").is_none());
        assert_eq!(t.estimate("gpt-9", 1, 1, 1, 1), None);
        assert!(PriceTable::default().lookup("x").is_none());
    }

    #[test]
    fn estimate_prices_each_kind_of_token() {
        let t = table();
        // 1M in at $3, 1M out at $15, 1M cached at $0.30, 0 writes.
        assert_eq!(
            t.estimate("claude-sonnet", 1_000_000, 1_000_000, 1_000_000, 0),
            Some(18_300_000)
        );
        // 12_400 in, 3_100 out: 37_200 + 46_500 micros.
        assert_eq!(
            t.estimate("claude-sonnet", 12_400, 3_100, 0, 0),
            Some(83_700)
        );
    }

    #[test]
    fn estimate_does_not_overflow() {
        let t = table();
        assert!(
            t.estimate("claude-sonnet", u64::MAX, u64::MAX, u64::MAX, u64::MAX)
                .is_some()
        );
    }

    #[test]
    fn parses_floats_into_micros_and_defaults_cache_prices() {
        let p: ModelPrice =
            toml::from_str("input_per_mtok = 3.0\noutput_per_mtok = 15.5\n").expect("parses");
        assert_eq!(p.input_micros_per_mtok, 3_000_000);
        assert_eq!(p.output_micros_per_mtok, 15_500_000);
        assert_eq!(p.cached_micros_per_mtok, 3_000_000);
    }

    #[test]
    fn rejects_negative_and_non_finite_prices() {
        assert!(
            toml::from_str::<ModelPrice>("input_per_mtok = -1.0\noutput_per_mtok = 1.0").is_err()
        );
        assert!(
            toml::from_str::<ModelPrice>("input_per_mtok = nan\noutput_per_mtok = 1.0").is_err()
        );
        assert!(
            toml::from_str::<ModelPrice>("input_per_mtok = inf\noutput_per_mtok = 1.0").is_err()
        );
    }
}
