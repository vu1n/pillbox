use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::contract::{Event, Payload, Usage, UsageSource};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ModelUsageCost {
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) cache_read_input_tokens: u64,
    pub(crate) cache_creation_input_tokens: u64,
    pub(crate) provider_reported_cost_usd: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct InfrastructureUsageCost {
    pub(crate) d1_rows_read: u64,
    pub(crate) d1_rows_written: u64,
    pub(crate) r2_reads: u64,
    pub(crate) r2_writes: u64,
    pub(crate) r2_bytes_read: u64,
    pub(crate) r2_bytes_written: u64,
    pub(crate) analytics_points_written: u64,
    pub(crate) sandbox_duration_ms: u64,
    pub(crate) sandbox_profile: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct RunCostEnvelope {
    pub(crate) version: u8,
    pub(crate) status: String,
    pub(crate) model: ModelUsageCost,
    pub(crate) infrastructure: Option<InfrastructureUsageCost>,
    pub(crate) known_cost_usd: Option<f64>,
    pub(crate) estimated_total_cost_usd: Option<f64>,
    pub(crate) rate_card_version: Option<String>,
}

impl RunCostEnvelope {
    pub(crate) fn validate_untrusted(&self, terminal_status: &str) -> bool {
        let Some(infrastructure) = &self.infrastructure else {
            return false;
        };
        let dollar_fields_agree = self.known_cost_usd == self.model.provider_reported_cost_usd;
        let estimate_has_rate_card = match (
            self.estimated_total_cost_usd,
            self.rate_card_version.as_deref(),
        ) {
            (None, None) => true,
            (Some(estimate), Some(version)) => {
                !version.is_empty()
                    && version.len() <= 128
                    && self.known_cost_usd.is_none_or(|known| estimate >= known)
            }
            _ => false,
        };
        self.version == 1
            && self.status == terminal_status
            && matches!(
                self.status.as_str(),
                "completed" | "failed" | "cancelled" | "interrupted"
            )
            && finite_non_negative(self.model.provider_reported_cost_usd)
            && finite_non_negative(self.known_cost_usd)
            && finite_non_negative(self.estimated_total_cost_usd)
            && dollar_fields_agree
            && estimate_has_rate_card
            && self.model.input_tokens <= 10_000_000_000
            && self.model.output_tokens <= 10_000_000_000
            && self.model.cache_read_input_tokens <= 10_000_000_000
            && self.model.cache_creation_input_tokens <= 10_000_000_000
            && infrastructure.d1_rows_read <= 10_000
            && infrastructure.d1_rows_written <= 10_000
            && infrastructure.r2_reads <= 1_000
            && infrastructure.r2_writes <= 1_000
            && infrastructure.r2_bytes_read <= 1024 * 1024 * 1024
            && infrastructure.r2_bytes_written <= 1024 * 1024 * 1024
            && infrastructure.analytics_points_written <= 1
            && infrastructure.sandbox_duration_ms <= 24 * 60 * 60 * 1_000
            && self
                .infrastructure
                .as_ref()
                .and_then(|usage| usage.sandbox_profile.as_ref())
                .is_none_or(|profile| profile.len() <= 128)
    }

    pub(crate) fn from_events(events: &[Event]) -> Self {
        let mut by_message: BTreeMap<&str, &Usage> = BTreeMap::new();
        let mut managed_envelope = None;
        let mut fallback_provider_cost = 0.0;
        let mut has_fallback_cost = false;
        let mut status = "running";

        for event in events {
            match &event.payload {
                Payload::Usage(usage) => {
                    let replace = by_message
                        .get(usage.message_id.as_str())
                        .is_none_or(|current| {
                            source_rank(usage.source) > source_rank(current.source)
                        });
                    if replace {
                        by_message.insert(&usage.message_id, usage);
                    }
                }
                Payload::RunFinished(_) => status = "completed",
                Payload::RunFailed(_) => status = "failed",
                Payload::Custom(custom) if custom.name == "usage" => {
                    if let Some(cost) = custom
                        .payload
                        .as_ref()
                        .and_then(|value| value.get("total_cost_usd"))
                        .and_then(serde_json::Value::as_f64)
                    {
                        if let Some(cost) = bounded_cost(cost) {
                            fallback_provider_cost = bounded_cost_add(fallback_provider_cost, cost);
                            has_fallback_cost = true;
                        }
                    }
                }
                Payload::Custom(custom) if custom.name == "run_cost" => {
                    managed_envelope = custom
                        .payload
                        .clone()
                        .and_then(|value| serde_json::from_value(value).ok());
                }
                _ => {}
            }
        }

        // The managed runtime observes infrastructure usage that cannot be
        // reconstructed from the local event stream. Its terminal envelope is
        // canonical for that turn and already includes model usage.
        if let Some(envelope) = managed_envelope {
            return envelope;
        }

        let mut model = ModelUsageCost {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            provider_reported_cost_usd: None,
        };
        let mut usage_cost = 0.0;
        let mut has_usage_cost = false;
        for usage in by_message.values() {
            model.input_tokens = model
                .input_tokens
                .saturating_add(usage.input_tokens.unwrap_or(0));
            model.output_tokens = model
                .output_tokens
                .saturating_add(usage.output_tokens.unwrap_or(0));
            model.cache_read_input_tokens = model
                .cache_read_input_tokens
                .saturating_add(usage.cache_read_input_tokens.unwrap_or(0));
            model.cache_creation_input_tokens = model
                .cache_creation_input_tokens
                .saturating_add(usage.cache_creation_input_tokens.unwrap_or(0));
            if let Some(cost) = usage.cost_usd.and_then(bounded_cost) {
                usage_cost = bounded_cost_add(usage_cost, cost);
                has_usage_cost = true;
            }
        }
        let known_cost = if has_usage_cost {
            Some(usage_cost)
        } else if has_fallback_cost {
            Some(fallback_provider_cost)
        } else {
            None
        };
        model.provider_reported_cost_usd = known_cost;
        Self {
            version: 1,
            status: status.to_string(),
            model,
            infrastructure: None,
            known_cost_usd: known_cost,
            // Without a versioned infrastructure rate card, provider spend is
            // known spend, not an all-in estimate.
            estimated_total_cost_usd: None,
            rate_card_version: None,
        }
    }
}

fn finite_non_negative(value: Option<f64>) -> bool {
    value.is_none_or(|number| number.is_finite() && (0.0..=1_000_000.0).contains(&number))
}

fn bounded_cost(value: f64) -> Option<f64> {
    value.is_finite().then(|| value.clamp(0.0, 1_000_000.0))
}

fn bounded_cost_add(left: f64, right: f64) -> f64 {
    (left + right).min(1_000_000.0)
}

fn source_rank(source: UsageSource) -> u8 {
    match source {
        UsageSource::Native => 2,
        UsageSource::Wire => 1,
        UsageSource::Unspecified => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{Event, RunFinished};

    fn usage(message: &str, source: UsageSource, input: u64, cost: Option<f64>) -> Event {
        Event::session(
            "session-1",
            Payload::Usage(Usage {
                message_id: message.into(),
                input_tokens: Some(input),
                output_tokens: Some(2),
                cache_read_input_tokens: Some(3),
                cache_creation_input_tokens: Some(4),
                cost_usd: cost,
                source,
            }),
        )
    }

    #[test]
    fn native_usage_wins_over_wire_for_the_same_message() {
        let events = vec![
            usage("m1", UsageSource::Wire, 100, None),
            usage("m1", UsageSource::Native, 10, Some(0.01)),
            Event::session(
                "session-1",
                Payload::RunFinished(RunFinished {
                    result_snapshot: String::new(),
                    exit_code: 0,
                    served_model: None,
                    effective_limits: None,
                }),
            ),
        ];
        let summary = RunCostEnvelope::from_events(&events);
        assert_eq!(summary.status, "completed");
        assert_eq!(summary.model.input_tokens, 10);
        assert_eq!(summary.known_cost_usd, Some(0.01));
        assert_eq!(summary.estimated_total_cost_usd, None);
    }

    #[test]
    fn managed_terminal_envelope_preserves_infrastructure_usage() {
        let expected = RunCostEnvelope {
            version: 1,
            status: "completed".into(),
            model: ModelUsageCost {
                input_tokens: 3,
                output_tokens: 4,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
                provider_reported_cost_usd: Some(0.02),
            },
            infrastructure: Some(InfrastructureUsageCost {
                d1_rows_read: 1,
                d1_rows_written: 2,
                r2_reads: 0,
                r2_writes: 1,
                r2_bytes_read: 0,
                r2_bytes_written: 512,
                analytics_points_written: 1,
                sandbox_duration_ms: 50,
                sandbox_profile: Some("standard-2".into()),
            }),
            known_cost_usd: Some(0.02),
            estimated_total_cost_usd: None,
            rate_card_version: None,
        };
        let event = Event::session(
            "session-1",
            Payload::Custom(crate::contract::Custom {
                name: "run_cost".into(),
                payload: Some(serde_json::to_value(&expected).unwrap()),
            }),
        );
        assert_eq!(RunCostEnvelope::from_events(&[event]), expected);
    }

    #[test]
    fn untrusted_managed_cost_rejects_impossible_ranges() {
        let mut envelope = RunCostEnvelope {
            version: 1,
            status: "completed".into(),
            model: ModelUsageCost {
                input_tokens: 1,
                output_tokens: 1,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
                provider_reported_cost_usd: Some(0.01),
            },
            infrastructure: Some(InfrastructureUsageCost {
                d1_rows_read: 1,
                d1_rows_written: 2,
                r2_reads: 0,
                r2_writes: 1,
                r2_bytes_read: 0,
                r2_bytes_written: 512,
                analytics_points_written: 1,
                sandbox_duration_ms: 50,
                sandbox_profile: Some("standard-2".into()),
            }),
            known_cost_usd: Some(0.01),
            estimated_total_cost_usd: None,
            rate_card_version: None,
        };
        assert!(envelope.validate_untrusted("completed"));
        envelope
            .infrastructure
            .as_mut()
            .unwrap()
            .analytics_points_written = 2;
        assert!(!envelope.validate_untrusted("completed"));
    }

    #[test]
    fn untrusted_managed_cost_requires_consistent_dollar_provenance() {
        let mut envelope = RunCostEnvelope {
            version: 1,
            status: "completed".into(),
            model: ModelUsageCost {
                input_tokens: 1,
                output_tokens: 1,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
                provider_reported_cost_usd: Some(0.01),
            },
            infrastructure: Some(InfrastructureUsageCost {
                d1_rows_read: 1,
                d1_rows_written: 2,
                r2_reads: 0,
                r2_writes: 1,
                r2_bytes_read: 0,
                r2_bytes_written: 512,
                analytics_points_written: 1,
                sandbox_duration_ms: 50,
                sandbox_profile: Some("standard-2".into()),
            }),
            known_cost_usd: Some(0.02),
            estimated_total_cost_usd: None,
            rate_card_version: None,
        };
        assert!(!envelope.validate_untrusted("completed"));

        envelope.known_cost_usd = Some(0.01);
        envelope.estimated_total_cost_usd = Some(0.009);
        envelope.rate_card_version = Some("2026-08".into());
        assert!(!envelope.validate_untrusted("completed"));

        envelope.estimated_total_cost_usd = Some(0.02);
        assert!(envelope.validate_untrusted("completed"));
    }

    #[test]
    fn fallback_costs_are_finite_and_saturating() {
        let events = vec![
            usage("m1", UsageSource::Native, 1, Some(f64::INFINITY)),
            usage("m2", UsageSource::Native, 1, Some(900_000.0)),
            usage("m3", UsageSource::Native, 1, Some(900_000.0)),
        ];
        let summary = RunCostEnvelope::from_events(&events);
        assert_eq!(summary.known_cost_usd, Some(1_000_000.0));
    }
}
