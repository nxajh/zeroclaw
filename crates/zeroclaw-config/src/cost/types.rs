use serde::{Deserialize, Serialize};

/// Token usage information from a single API call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Model identifier (e.g., "anthropic/claude-sonnet-4-20250514")
    pub model: String,
    /// Input/prompt tokens reported by the provider.
    pub input_tokens: u64,
    /// Output/completion tokens reported by the provider.
    pub output_tokens: u64,
    /// Total tokens (input + output).
    pub total_tokens: u64,
    /// Tokens served from the provider's prompt cache
    /// (OpenAI `prompt_tokens_details.cached_tokens`, Anthropic `cache_read_input_tokens`).
    pub cached_input_tokens: u64,
    /// Input tokens that were actually charged: `input_tokens.saturating_sub(cached_input_tokens)`.
    pub effective_input_tokens: u64,
    /// Calculated cost in USD (based on `effective_input_tokens`, not raw input).
    pub cost_usd: f64,
    /// Timestamp of the request.
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

impl TokenUsage {
    fn sanitize_price(value: f64) -> f64 {
        if value.is_finite() && value > 0.0 {
            value
        } else {
            0.0
        }
    }

    /// Create a new token usage record.
    ///
    /// `cached_input_tokens` is subtracted from `input_tokens` when computing
    /// the effective cost, because cache hits are billed at a heavily discounted
    /// rate (or free on some providers).
    pub fn new(
        model: impl Into<String>,
        input_tokens: u64,
        output_tokens: u64,
        cached_input_tokens: u64,
        input_price_per_million: f64,
        output_price_per_million: f64,
    ) -> Self {
        let model = model.into();
        let cached_input_tokens = cached_input_tokens.min(input_tokens);
        let effective_input = input_tokens.saturating_sub(cached_input_tokens);
        let total_tokens = effective_input.saturating_add(output_tokens);

        let input_cost =
            (effective_input as f64 / 1_000_000.0) * Self::sanitize_price(input_price_per_million);
        let output_cost =
            (output_tokens as f64 / 1_000_000.0) * Self::sanitize_price(output_price_per_million);
        let cost_usd = input_cost + output_cost;

        Self {
            model,
            input_tokens,
            output_tokens,
            total_tokens,
            cached_input_tokens,
            effective_input_tokens: effective_input,
            cost_usd,
            timestamp: chrono::Utc::now(),
        }
    }

    /// Get the total cost.
    pub fn cost(&self) -> f64 {
        self.cost_usd
    }

    /// Cache hit ratio as a fraction of input tokens (0.0–1.0).
    /// Returns 0.0 when `input_tokens` is zero.
    pub fn cache_hit_ratio(&self) -> f64 {
        if self.input_tokens == 0 {
            0.0
        } else {
            self.cached_input_tokens as f64 / self.input_tokens as f64
        }
    }
}

/// Time period for cost aggregation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UsagePeriod {
    Session,
    Day,
    Month,
}

/// A single cost record for persistent storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostRecord {
    /// Unique identifier
    pub id: String,
    /// Token usage details
    pub usage: TokenUsage,
    /// Session identifier (for grouping)
    pub session_id: String,
}

impl CostRecord {
    /// Create a new cost record.
    pub fn new(session_id: impl Into<String>, usage: TokenUsage) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            usage,
            session_id: session_id.into(),
        }
    }
}

/// Budget enforcement result.
#[derive(Debug, Clone)]
pub enum BudgetCheck {
    /// Within budget, request can proceed
    Allowed,
    /// Warning threshold exceeded but request can proceed
    Warning {
        current_usd: f64,
        limit_usd: f64,
        period: UsagePeriod,
    },
    /// Budget exceeded, request blocked
    Exceeded {
        current_usd: f64,
        limit_usd: f64,
        period: UsagePeriod,
    },
}

/// Cost summary for reporting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostSummary {
    /// Total cost for the session
    pub session_cost_usd: f64,
    /// Total cost for the day
    pub daily_cost_usd: f64,
    /// Total cost for the month
    pub monthly_cost_usd: f64,
    /// Total tokens used (input_effective + output)
    pub total_tokens: u64,
    /// Number of requests
    pub request_count: usize,
    /// Total raw input tokens across all requests
    pub total_input_tokens: u64,
    /// Total cached input tokens across all requests
    pub total_cached_tokens: u64,
    /// Total effective (non-cached) input tokens
    pub total_effective_input_tokens: u64,
    /// Overall cache hit ratio (0.0–1.0), NaN when no input tokens recorded
    pub cache_hit_ratio: f64,
    /// Breakdown by model
    pub by_model: std::collections::HashMap<String, ModelStats>,
}

/// Statistics for a specific model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelStats {
    /// Model name
    pub model: String,
    /// Total cost for this model
    pub cost_usd: f64,
    /// Total tokens for this model (effective + output)
    pub total_tokens: u64,
    /// Number of requests for this model
    pub request_count: usize,
    /// Cache hit ratio for this model (0.0–1.0)
    pub cache_hit_ratio: f64,
}

impl Default for CostSummary {
    fn default() -> Self {
        Self {
            session_cost_usd: 0.0,
            daily_cost_usd: 0.0,
            monthly_cost_usd: 0.0,
            total_tokens: 0,
            request_count: 0,
            total_input_tokens: 0,
            total_cached_tokens: 0,
            total_effective_input_tokens: 0,
            cache_hit_ratio: f64::NAN,
            by_model: std::collections::HashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_usage_calculation() {
        let usage = TokenUsage::new("test/model", 1000, 500, 0, 3.0, 15.0);

        // Effective input = 1000 (no cache), cost = (1000/1M)*3 + (500/1M)*15 = 0.003 + 0.0075 = 0.0105
        assert!((usage.cost_usd - 0.0105).abs() < 0.0001);
        assert_eq!(usage.input_tokens, 1000);
        assert_eq!(usage.output_tokens, 500);
        assert_eq!(usage.total_tokens, 1500);
        assert_eq!(usage.cached_input_tokens, 0);
        assert_eq!(usage.effective_input_tokens, 1000);
    }

    #[test]
    fn token_usage_zero_tokens() {
        let usage = TokenUsage::new("test/model", 0, 0, 0, 3.0, 15.0);
        assert!(usage.cost_usd.abs() < f64::EPSILON);
        assert_eq!(usage.total_tokens, 0);
    }

    #[test]
    fn token_usage_negative_or_non_finite_prices_are_clamped() {
        let usage = TokenUsage::new("test/model", 1000, 1000, 0, -3.0, f64::NAN);
        assert!(usage.cost_usd.abs() < f64::EPSILON);
        assert_eq!(usage.total_tokens, 2000);
    }

    #[test]
    fn token_usage_with_cache_hit() {
        // 80% cache hit: 1000 input, 800 cached → effective = 200
        let usage = TokenUsage::new("test/model", 1000, 200, 800, 3.0, 15.0);
        // Cost = (200/1M)*3 + (200/1M)*15 = 0.0006 + 0.003 = 0.0036
        assert!((usage.cost_usd - 0.0036).abs() < 0.0001);
        assert_eq!(usage.effective_input_tokens, 200);
        assert_eq!(usage.total_tokens, 400);
        assert!((usage.cache_hit_ratio() - 0.8).abs() < f64::EPSILON);
    }

    #[test]
    fn token_usage_cache_cannot_exceed_input() {
        // Cache > input should be clamped
        let usage = TokenUsage::new("test/model", 100, 50, 200, 1.0, 2.0);
        assert_eq!(usage.cached_input_tokens, 100);
        assert_eq!(usage.effective_input_tokens, 0);
    }

    #[test]
    fn cost_record_creation() {
        let usage = TokenUsage::new("test/model", 100, 50, 1.0, 2.0);
        let record = CostRecord::new("session-123", usage);

        assert_eq!(record.session_id, "session-123");
        assert!(!record.id.is_empty());
        assert_eq!(record.usage.model, "test/model");
    }
}
