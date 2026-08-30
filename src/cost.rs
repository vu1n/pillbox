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
                        .map_or(true, |current| {
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
                        fallback_provider_cost += cost.max(0.0);
                        has_fallback_cost = true;
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
            model.input_tokens += usage.input_tokens.unwrap_or(0);
            model.output_tokens += usage.output_tokens.unwrap_or(0);
            model.cache_read_input_tokens += usage.cache_read_input_tokens.unwrap_or(0);
            model.cache_creation_input_tokens += usage.cache_creation_input_tokens.unwrap_or(0);
            if let Some(cost) = usage.cost_usd {
                usage_cost += cost.max(0.0);
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
}
