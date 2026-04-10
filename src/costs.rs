//! Model cost estimation based on per-token pricing.
//!
//! Costs are loaded from a built-in CSV file at compile time, and can be
//! overridden at runtime via `--model-costs=PATH`.

use std::{collections::BTreeMap, path::Path};

use clap::ValueEnum;

use crate::{drivers::DriverType, prelude::*};

/// A key for looking up model costs, based on driver and model name. If the
/// driver is `None`, then the cost applies to any driver lacking a more
/// specific match.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct ModelKey {
    driver: Option<DriverType>,
    model: String,
}

impl ModelKey {
    /// Construct a key for a specific driver and model.
    fn for_driver(driver: DriverType, model: &str) -> Self {
        Self {
            driver: Some(driver),
            model: model.to_string(),
        }
    }

    /// Construct a key for any driver with the given model.
    fn for_any_driver(model: &str) -> Self {
        Self {
            driver: None,
            model: model.to_string(),
        }
    }
}

pub struct ModelCostDatabase {
    /// Map from model keys to cost information.
    costs: BTreeMap<ModelKey, ModelCost>,
}

impl ModelCostDatabase {
    pub fn new(path: Option<&Path>) -> Self {
        let costs = match build_model_cost_map(path) {
            Ok(map) => map,
            Err(err) => {
                warn!(%err, "Failed to load model costs; cost estimation will be unavailable");
                BTreeMap::new()
            }
        };
        debug!(count = costs.len(), "Loaded model costs");
        Self { costs }
    }

    /// Look up cost information for a model and driver, with fallback to a
    /// generic model match.
    pub fn lookup(&self, driver: DriverType, model: &str) -> Option<&ModelCost> {
        // First try a driver-specific match, then a generic match.
        self.costs
            .get(&ModelKey::for_driver(driver, model))
            .or_else(|| self.costs.get(&ModelKey::for_any_driver(model)))
    }
}

/// Per-token cost information for a model.
#[derive(Debug, Clone)]
pub struct ModelCost {
    /// Cost of input tokens in USD. May be 0 for local models.
    pub input_cost_per_token: f64,
    /// Cost of cached input tokens in USD, if the provider supports it.
    #[expect(
        dead_code,
        reason = "populated from CSV, will be used for cached-token-aware cost estimation"
    )]
    pub input_cost_per_cached_token: Option<f64>,
    /// Cost of output tokens in USD. May be 0 for local models.
    pub output_cost_per_token: f64,
}

/// A record from the model costs CSV file.
#[derive(Debug, Deserialize)]
struct ModelCostRecord {
    driver: Option<String>,
    model: String,
    input_cost_per_token: f64,
    input_cost_per_cached_token: Option<f64>,
    output_cost_per_token: f64,
    /// Not used at runtime — documentation only.
    #[allow(dead_code)]
    pricing_source_url: String,
}

/// Built-in model costs CSV, embedded at compile time.
const DEFAULT_MODEL_COSTS_CSV: &str = include_str!("default_model_costs.csv");

/// Build the model cost map from a file or the built-in defaults.
fn build_model_cost_map(path: Option<&Path>) -> Result<BTreeMap<ModelKey, ModelCost>> {
    match path {
        Some(path) => {
            let csv_data = std::fs::read_to_string(path).with_context(|| {
                format!("Failed to read model costs from {}", path.display())
            })?;
            parse_model_costs_csv(&csv_data)
        }
        None => parse_model_costs_csv(DEFAULT_MODEL_COSTS_CSV),
    }
}

/// Parse a model costs CSV (from a string) into a lookup map.
fn parse_model_costs_csv(csv_data: &str) -> Result<BTreeMap<ModelKey, ModelCost>> {
    let mut reader = csv::ReaderBuilder::new()
        .comment(Some(b'#'))
        .from_reader(csv_data.as_bytes());
    let mut map = BTreeMap::new();
    for result in reader.deserialize::<ModelCostRecord>() {
        let record = result.context("Failed to parse model cost record")?;
        let driver = match record.driver.as_deref() {
            None | Some("") => None,
            Some(s) => Some(
                DriverType::from_str(s, true)
                    .map_err(|e| anyhow!("Unknown driver {s:?}: {e}"))?,
            ),
        };
        let key = ModelKey {
            driver,
            model: record.model,
        };
        map.insert(
            key,
            ModelCost {
                input_cost_per_token: record.input_cost_per_token,
                input_cost_per_cached_token: record.input_cost_per_cached_token,
                output_cost_per_token: record.output_cost_per_token,
            },
        );
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_db() -> ModelCostDatabase {
        ModelCostDatabase::new(None)
    }

    #[test]
    fn embedded_csv_parses() {
        let db = default_db();
        assert!(
            db.costs.len() > 10,
            "expected at least 10 models, got {}",
            db.costs.len()
        );
    }

    #[test]
    fn known_model_has_expected_cost() {
        let db = default_db();
        let cost = db
            .lookup(DriverType::OpenAI, "gpt-4o-mini")
            .expect("gpt-4o-mini should be in database");
        assert!(
            (cost.input_cost_per_token - 0.00000015).abs() < 1e-12,
            "unexpected input cost: {}",
            cost.input_cost_per_token
        );
        assert!(
            (cost.output_cost_per_token - 0.0000006).abs() < 1e-12,
            "unexpected output cost: {}",
            cost.output_cost_per_token
        );
    }

    #[test]
    fn unknown_model_returns_none() {
        let db = default_db();
        assert!(
            db.lookup(DriverType::OpenAI, "nonexistent-model-xyz")
                .is_none()
        );
    }
}
