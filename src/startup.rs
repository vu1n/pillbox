//! Host-side startup timing for sandbox launches.
//!
//! These timings are intentionally modest: they measure the host work pillbox
//! can see today (preflight, workspace/rootfs prep, spawn, readiness where the
//! backend has a readiness check). A future guest-side ready handshake can add
//! lower-level stages without changing the lifecycle event surface.

use std::time::{Duration, Instant};

use serde_json::json;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StartupStage {
    pub(crate) name: String,
    pub(crate) duration_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StartupMetrics {
    pub(crate) total_ms: i64,
    pub(crate) stages: Vec<StartupStage>,
}

impl StartupMetrics {
    pub(crate) fn stages_json(&self) -> serde_json::Value {
        serde_json::Value::Array(
            self.stages
                .iter()
                .map(|stage| {
                    json!({
                        "name": stage.name,
                        "duration_ms": stage.duration_ms,
                    })
                })
                .collect(),
        )
    }
}

#[derive(Debug)]
pub(crate) struct StartupTimer {
    started: Instant,
    last: Instant,
    stages: Vec<StartupStage>,
}

impl StartupTimer {
    pub(crate) fn start() -> Self {
        let now = Instant::now();
        Self {
            started: now,
            last: now,
            stages: Vec::new(),
        }
    }

    pub(crate) fn mark(&mut self, name: impl Into<String>) {
        let now = Instant::now();
        self.stages.push(StartupStage {
            name: name.into(),
            duration_ms: duration_ms(now.duration_since(self.last)),
        });
        self.last = now;
    }

    pub(crate) fn finish(mut self, final_stage: impl Into<String>) -> StartupMetrics {
        self.mark(final_stage);
        StartupMetrics {
            total_ms: duration_ms(self.last.duration_since(self.started)),
            stages: self.stages,
        }
    }
}

fn duration_ms(duration: Duration) -> i64 {
    let ms = duration.as_millis();
    if ms > i64::MAX as u128 {
        i64::MAX
    } else {
        ms as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_render_ordered_stage_json() {
        let metrics = StartupMetrics {
            total_ms: 7,
            stages: vec![
                StartupStage {
                    name: "preflight".into(),
                    duration_ms: 3,
                },
                StartupStage {
                    name: "spawn".into(),
                    duration_ms: 4,
                },
            ],
        };
        let json = metrics.stages_json();
        assert_eq!(json[0]["name"], "preflight");
        assert_eq!(json[0]["duration_ms"], 3);
        assert_eq!(json[1]["name"], "spawn");
        assert_eq!(json[1]["duration_ms"], 4);
    }
}
