//! Usage metering and quota enforcement for paid LLM backends.

use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::conversation::TokenDelta;
use crate::error::LlmError;

/// Dollar cost per million tokens.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Pricing {
    pub usd_per_million_input_tokens: f64,
    pub usd_per_million_output_tokens: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct QuotaConfig {
    /// Maximum total (input + output) tokens for the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_total_tokens: Option<u64>,
    /// Maximum dollar spend for the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_usd: Option<f64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub usd: f64,
}

impl UsageSnapshot {
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

/// Thread-safe token/dollar meter. Backends call [`UsageMeter::record`]
/// after every completion and [`UsageMeter::check_quota`] before issuing a
/// new request.
#[derive(Debug, Default)]
pub struct UsageMeter {
    pricing: Pricing,
    quota: QuotaConfig,
    input_tokens: AtomicU64,
    output_tokens: AtomicU64,
}

impl UsageMeter {
    pub fn new(pricing: Pricing, quota: QuotaConfig) -> Self {
        UsageMeter {
            pricing,
            quota,
            input_tokens: AtomicU64::new(0),
            output_tokens: AtomicU64::new(0),
        }
    }

    pub fn record(&self, delta: TokenDelta) {
        self.input_tokens
            .fetch_add(delta.input_tokens, Ordering::SeqCst);
        self.output_tokens
            .fetch_add(delta.output_tokens, Ordering::SeqCst);
    }

    pub fn snapshot(&self) -> UsageSnapshot {
        let input = self.input_tokens.load(Ordering::SeqCst);
        let output = self.output_tokens.load(Ordering::SeqCst);
        let usd = (input as f64) * self.pricing.usd_per_million_input_tokens / 1_000_000.0
            + (output as f64) * self.pricing.usd_per_million_output_tokens / 1_000_000.0;
        UsageSnapshot {
            input_tokens: input,
            output_tokens: output,
            usd,
        }
    }

    pub fn quota(&self) -> QuotaConfig {
        self.quota
    }

    /// Returns an error if the session has already consumed its quota.
    pub fn check_quota(&self) -> Result<(), LlmError> {
        let snap = self.snapshot();
        if let Some(max) = self.quota.max_total_tokens {
            if snap.total_tokens() >= max {
                return Err(LlmError::QuotaExceeded(format!(
                    "token quota reached: {} of {} tokens used",
                    snap.total_tokens(),
                    max
                )));
            }
        }
        if let Some(max) = self.quota.max_usd {
            if snap.usd >= max {
                return Err(LlmError::QuotaExceeded(format!(
                    "dollar quota reached: ${:.4} of ${:.2} used",
                    snap.usd, max
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meter_accumulates_and_prices() {
        let meter = UsageMeter::new(
            Pricing {
                usd_per_million_input_tokens: 3.0,
                usd_per_million_output_tokens: 15.0,
            },
            QuotaConfig::default(),
        );
        meter.record(TokenDelta {
            input_tokens: 1_000_000,
            output_tokens: 2_000_000,
        });
        let snap = meter.snapshot();
        assert_eq!(snap.input_tokens, 1_000_000);
        assert_eq!(snap.output_tokens, 2_000_000);
        assert!((snap.usd - 33.0).abs() < 1e-9);
        assert!(meter.check_quota().is_ok());
    }

    #[test]
    fn quota_blocks_when_exhausted() {
        let meter = UsageMeter::new(
            Pricing::default(),
            QuotaConfig {
                max_total_tokens: Some(10),
                max_usd: None,
            },
        );
        meter.record(TokenDelta {
            input_tokens: 10,
            output_tokens: 0,
        });
        assert!(matches!(
            meter.check_quota(),
            Err(LlmError::QuotaExceeded(_))
        ));
    }
}
