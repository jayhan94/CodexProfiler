use serde::{Deserialize, Serialize};

pub const AS_OF: &str = "2026-11-14";
pub const SOURCE: &str = "https://developers.openai.com/api/docs/pricing";

#[derive(Clone, Copy, Default, Serialize, Deserialize)]
pub struct CostBreakdown {
    pub uncached_usd: f64,
    pub cached_usd: f64,
    pub write_usd: f64,
    pub output_usd: f64,
}

impl CostBreakdown {
    pub fn total(&self) -> f64 {
        self.uncached_usd + self.cached_usd + self.write_usd + self.output_usd
    }
}

/// Snapshot of official Standard API USD rates per million tokens, using 30m cache writes.
/// Model pages document the >272K full-request surcharge; this is not a subscription bill.
pub fn request(
    model: &str,
    input: u64,
    cached: u64,
    written: u64,
    output: u64,
) -> Option<CostBreakdown> {
    let (fresh_rate, cached_rate, write_rate, output_rate) = match model {
        "gpt-6.1-sol" => (2.0, 0.10, 2.50, 10.0),
        "gpt-6-sol" => (2.0, 0.20, 2.50, 10.0),
        "gpt-6.1-terra" => (1.0, 0.05, 1.25, 5.0),
        "gpt-6-luna" => (0.10, 0.01, 0.125, 0.50),
        "gpt-6-astra" => (10.0, 1.0, 12.50, 50.0),
        _ => return None,
    };
    let fresh = input.checked_sub(cached.checked_add(written)?)?;
    let (input_multiplier, output_multiplier) = if input > 272_000 {
        (2.0, 1.5)
    } else {
        (1.0, 1.0)
    };
    Some(CostBreakdown {
        uncached_usd: fresh as f64 * fresh_rate * input_multiplier / 1_000_000.0,
        cached_usd: cached as f64 * cached_rate * input_multiplier / 1_000_000.0,
        write_usd: written as f64 * write_rate * input_multiplier / 1_000_000.0,
        output_usd: output as f64 * output_rate * output_multiplier / 1_000_000.0,
    })
}

/// Speed is recorded per request; UI speed-up labels are not billing multipliers.
pub fn request_for_tier(
    model: &str,
    input: u64,
    cached: u64,
    written: u64,
    output: u64,
    tier: &str,
) -> Option<CostBreakdown> {
    let multiplier = match tier {
        "default" | "standard" | "auto" => 1.0,
        "fast" | "priority" => 2.0,
        "batch" | "flex" => 0.5,
        "ultrafast" if matches!(model, "gpt-6.1-sol" | "gpt-6-astra") => 6.0,
        _ => return None,
    };
    let mut cost = request(model, input, cached, written, output)?;
    cost.uncached_usd *= multiplier;
    cost.cached_usd *= multiplier;
    cost.write_usd *= multiplier;
    cost.output_usd *= multiplier;
    Some(cost)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Cache read/write are input subsets and are charged once, at their own prices.
    #[test]
    fn cached_and_written_input_are_not_double_charged() {
        let cost = request("gpt-6.1-sol", 1_000_000, 600_000, 100_000, 100_000).unwrap();
        assert!((cost.uncached_usd - 1.2).abs() < 1e-9);
        assert!((cost.cached_usd - 0.12).abs() < 1e-9);
        assert!((cost.write_usd - 0.5).abs() < 1e-9);
        assert!((cost.output_usd - 1.5).abs() < 1e-9);
        assert!((cost.total() - 3.32).abs() < 1e-9);
    }

    // Crossing 272K changes the applicable price for the complete request, including output.
    #[test]
    fn long_context_threshold_is_per_request() {
        let at = request("gpt-6.1-sol", 272_000, 0, 0, 100).unwrap();
        let above = request("gpt-6.1-sol", 272_001, 0, 0, 100).unwrap();
        assert!((at.total() - 0.545).abs() < 1e-9);
        assert!((above.total() - 1.089504).abs() < 1e-9);
    }

    // Fast pricing uses the published API rate, independent of a 1.5x speed label.
    #[test]
    fn fast_mode_uses_api_multiplier() {
        let normal = request_for_tier("gpt-6.1-sol", 1000, 800, 0, 100, "default").unwrap();
        let fast = request_for_tier("gpt-6.1-sol", 1000, 800, 0, 100, "priority").unwrap();
        assert!((fast.total() - normal.total() * 2.0).abs() < 1e-12);
        assert!(request_for_tier("gpt-6-luna", 1000, 0, 0, 100, "ultrafast").is_none());
    }

    // Unsupported prices and inconsistent telemetry remain unknown rather than zero.
    #[test]
    fn missing_price_or_invalid_usage_has_no_quote() {
        assert!(request("unknown-model", 100, 0, 0, 10).is_none());
        assert!(request("gpt-6-sol", 100, 90, 20, 10).is_none());
    }
}
