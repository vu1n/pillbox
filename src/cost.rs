use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::contract::{Event, Payload, Usage, UsageSource};

/// Snapshot of normalized usage observations, rather than a billing envelope.
/// A counter is unknown if any selected observation omits it or its sum overflows.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct ObservedUsage {
    pub(crate) input_tokens: Option<u64>,
    pub(crate) output_tokens: Option<u64>,
    pub(crate) cache_read_input_tokens: Option<u64>,
    pub(crate) cache_creation_input_tokens: Option<u64>,
}

impl ObservedUsage {
    pub(crate) fn from_events(events: &[Event]) -> Option<Self> {
        let usages = usage_by_message(events);
        if usages.is_empty() {
            return None;
        }
        let sum = |counter: fn(&Usage) -> Option<u64>| {
            usages
                .values()
                .try_fold(0u64, |total, usage| total.checked_add(counter(usage)?))
        };
        Some(Self {
            input_tokens: sum(|u| u.input_tokens),
            output_tokens: sum(|u| u.output_tokens),
            cache_read_input_tokens: sum(|u| u.cache_read_input_tokens),
            cache_creation_input_tokens: sum(|u| u.cache_creation_input_tokens),
        })
    }
}

/// Preserve the cost fold's identity and source preference across consumers.
/// Equal-ranked redeliveries keep the first observation; fields aren't merged
/// across sources because the normalized accounting semantics may differ.
fn usage_by_message(events: &[Event]) -> BTreeMap<&str, &Usage> {
    let mut by_message: BTreeMap<&str, &Usage> = BTreeMap::new();
    for event in events {
        if let Payload::Usage(usage) = &event.payload {
            let replace = by_message
                .get(usage.message_id.as_str())
                .is_none_or(|current| source_rank(usage.source) > source_rank(current.source));
            if replace {
                by_message.insert(&usage.message_id, usage);
            }
        }
    }
    by_message
}

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
    pub(crate) analytics_points_planned: u64,
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
            && infrastructure.analytics_points_planned <= 1
            && infrastructure.sandbox_duration_ms <= 24 * 60 * 60 * 1_000
            && self
                .infrastructure
                .as_ref()
                .and_then(|usage| usage.sandbox_profile.as_ref())
                .is_none_or(|profile| profile.len() <= 128)
    }

    pub(crate) fn from_events(events: &[Event]) -> Self {
        let by_message = usage_by_message(events);
        let mut managed_envelope = None;
        let mut fallback_provider_cost = 0.0;
        let mut has_fallback_cost = false;
        let mut status = "running";

        for event in events {
            match &event.payload {
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
    fn observed_usage_distinguishes_missing_from_reported_zero() {
        assert_eq!(ObservedUsage::from_events(&[]), None);
        let mut zero = usage("m1", UsageSource::Native, 0, None);
        let Payload::Usage(u) = &mut zero.payload else {
            unreachable!()
        };
        u.output_tokens = Some(0);
        u.cache_read_input_tokens = Some(0);
        u.cache_creation_input_tokens = Some(0);
        assert_eq!(
            serde_json::to_value(ObservedUsage::from_events(&[zero])).unwrap(),
            serde_json::json!({"input_tokens":0,"output_tokens":0,
                "cache_read_input_tokens":0,"cache_creation_input_tokens":0})
        );
    }

    #[test]
    fn observed_usage_nulls_incomplete_counters_without_losing_other_fields() {
        let first = usage("m1", UsageSource::Native, 10, None);
        let mut second = usage("m2", UsageSource::Native, 20, None);
        let Payload::Usage(u) = &mut second.payload else {
            unreachable!()
        };
        u.input_tokens = None;
        u.cache_creation_input_tokens = None;
        let total = ObservedUsage::from_events(&[first, second]).unwrap();
        assert_eq!(total.input_tokens, None); // 10 is an incomplete subtotal.
        assert_eq!(total.output_tokens, Some(4));
        assert_eq!(total.cache_read_input_tokens, Some(6));
        assert_eq!(total.cache_creation_input_tokens, None);
    }

    #[test]
    fn observed_usage_reuses_cost_identity_and_source_preference() {
        for events in [
            vec![
                usage("m1", UsageSource::Wire, 100, None),
                usage("m1", UsageSource::Native, 10, None),
                usage("m1", UsageSource::Native, 50, None),
            ],
            vec![
                usage("m1", UsageSource::Native, 10, None),
                usage("m1", UsageSource::Wire, 100, None),
            ],
        ] {
            let observed = ObservedUsage::from_events(&events).unwrap();
            assert_eq!(observed.input_tokens, Some(10));
            assert_eq!(observed.output_tokens, Some(2));
            assert_eq!(RunCostEnvelope::from_events(&events).model.input_tokens, 10);
        }
        let mut native = usage("m1", UsageSource::Native, 10, None);
        let Payload::Usage(u) = &mut native.payload else {
            unreachable!()
        };
        u.output_tokens = None;
        // A wire field must not fill a missing native field using different semantics.
        let total =
            ObservedUsage::from_events(&[usage("m1", UsageSource::Wire, 100, None), native])
                .unwrap();
        assert_eq!(total.output_tokens, None);
    }

    #[test]
    fn observed_usage_adds_normalized_events_and_ignores_session_snapshots() {
        let events = [
            usage("turn-1", UsageSource::Native, 10, None),
            usage("retry-1", UsageSource::Native, 20, None),
            usage("segment-2", UsageSource::Native, 30, None),
            usage("retry-1", UsageSource::Native, 20, None),
            Event::session(
                "session-1",
                Payload::Custom(crate::contract::Custom {
                    name: "usage".into(),
                    payload: Some(serde_json::json!({"input_tokens": 60})),
                }),
            ),
            Event::session(
                "session-1",
                Payload::Custom(crate::contract::Custom {
                    name: "run_cost".into(),
                    payload: Some(serde_json::json!({"model":{"input_tokens": 60}})),
                }),
            ),
        ];
        let total = ObservedUsage::from_events(&events).unwrap();
        assert_eq!(total.input_tokens, Some(60));
        assert_eq!(total.output_tokens, Some(6));
        assert_eq!(total.cache_read_input_tokens, Some(9));
        assert_eq!(total.cache_creation_input_tokens, Some(12));
    }

    #[test]
    fn observed_usage_overflow_is_unknown_per_counter() {
        let first = usage("m1", UsageSource::Native, u64::MAX, None);
        assert_eq!(
            ObservedUsage::from_events(std::slice::from_ref(&first))
                .unwrap()
                .input_tokens,
            Some(u64::MAX)
        );
        let second = usage("m2", UsageSource::Native, 1, None);
        let total = ObservedUsage::from_events(&[first, second]).unwrap();
        assert_eq!(total.input_tokens, None);
        assert_eq!(total.output_tokens, Some(4));
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
                analytics_points_planned: 1,
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
                analytics_points_planned: 1,
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
            .analytics_points_planned = 2;
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
                analytics_points_planned: 1,
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
