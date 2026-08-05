//! Request statistics: per-turn token usage recorded by the proxy and
//! aggregated for the Overview page. Persisted at
//! `~/.loomrouter/stats.json`; capped so the file stays small.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

const MAX_RECORDS: usize = 50_000;

pub type SharedStats = Arc<RwLock<Stats>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestRecord {
    /// Unix seconds.
    pub ts: u64,
    /// Provider id, or "codex-native" for ChatGPT passthrough turns.
    pub provider: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Stats {
    #[serde(default)]
    pub records: Vec<RequestRecord>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderAggregate {
    pub provider: String,
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatsSummary {
    pub period_secs: u64,
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    /// cached / input, 0..1
    pub cache_ratio: f64,
    pub per_provider: Vec<ProviderAggregate>,
}

fn stats_path() -> PathBuf {
    crate::config::config_dir().join("stats.json")
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Stats {
    pub fn load() -> Self {
        std::fs::read_to_string(stats_path())
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let path = stats_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string(self) {
            let _ = std::fs::write(path, json);
        }
    }

    /// Record one completed turn. `usage` is a Responses-format usage
    /// object ({input_tokens, output_tokens, input_tokens_details}).
    pub fn record(&mut self, provider: &str, model: &str, usage: &serde_json::Value) {
        let input = usage
            .get("input_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let output = usage
            .get("output_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let cached = usage
            .pointer("/input_tokens_details/cached_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        if input == 0 && output == 0 {
            return; // nothing useful to store
        }
        if self.records.len() >= MAX_RECORDS {
            let drop = MAX_RECORDS / 5;
            self.records.drain(..drop);
        }
        self.records.push(RequestRecord {
            ts: now_unix(),
            provider: provider.to_string(),
            model: model.to_string(),
            input_tokens: input,
            output_tokens: output,
            cached_tokens: cached,
        });
    }

    pub fn summarize(&self, period_secs: u64) -> StatsSummary {
        let cutoff = now_unix().saturating_sub(period_secs);
        let mut per_provider: std::collections::BTreeMap<String, ProviderAggregate> =
            std::collections::BTreeMap::new();
        let mut summary = StatsSummary {
            period_secs,
            requests: 0,
            input_tokens: 0,
            output_tokens: 0,
            cached_tokens: 0,
            cache_ratio: 0.0,
            per_provider: Vec::new(),
        };
        for r in self.records.iter().filter(|r| r.ts >= cutoff) {
            summary.requests += 1;
            summary.input_tokens += r.input_tokens;
            summary.output_tokens += r.output_tokens;
            summary.cached_tokens += r.cached_tokens;
            let agg = per_provider
                .entry(r.provider.clone())
                .or_insert_with(|| ProviderAggregate {
                    provider: r.provider.clone(),
                    requests: 0,
                    input_tokens: 0,
                    output_tokens: 0,
                    cached_tokens: 0,
                });
            agg.requests += 1;
            agg.input_tokens += r.input_tokens;
            agg.output_tokens += r.output_tokens;
            agg.cached_tokens += r.cached_tokens;
        }
        if summary.input_tokens > 0 {
            summary.cache_ratio = summary.cached_tokens as f64 / summary.input_tokens as f64;
        }
        summary.per_provider = per_provider.into_values().collect();
        summary
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn summarize_aggregates_per_provider() {
        let mut s = Stats::default();
        let usage = json!({
            "input_tokens": 100,
            "output_tokens": 20,
            "input_tokens_details": {"cached_tokens": 80},
        });
        s.record("kimi-coding", "k3", &usage);
        s.record("kimi-coding", "k3", &usage);
        s.record("codex-native", "gpt-5.5", &usage);

        let sum = s.summarize(86_400);
        assert_eq!(sum.requests, 3);
        assert_eq!(sum.input_tokens, 300);
        assert_eq!(sum.cached_tokens, 240);
        assert!((sum.cache_ratio - 0.8).abs() < 1e-9);
        assert_eq!(sum.per_provider.len(), 2);
        assert_eq!(sum.per_provider[1].requests, 2);
    }

    #[test]
    fn empty_usage_is_not_recorded() {
        let mut s = Stats::default();
        s.record("kimi", "k3", &json!({"input_tokens": 0, "output_tokens": 0}));
        assert!(s.records.is_empty());
    }
}
