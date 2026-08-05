//! Request statistics and per-request logs, stored in a local SQLite
//! database at `~/.loomrouter/loom.db`. Recorded by the proxy and
//! aggregated for the Overview page (and a future per-request Logs tab).
//!
//! On first open, any legacy `stats.json` is migrated into the database
//! and renamed to `stats.json.migrated`.

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

pub type SharedStats = Arc<RwLock<Stats>>;

pub struct Stats {
    // rusqlite::Connection is Send but not Sync; the mutex makes Stats
    // Sync so it can live behind the shared tokio RwLock.
    conn: std::sync::Mutex<rusqlite::Connection>,
}

/// One recorded request (a completed or failed turn through the proxy).
#[derive(Debug, Clone, Serialize)]
pub struct RequestEntry {
    pub ts: u64,
    pub provider: String,
    pub model: String,
    /// "http" or "ws".
    pub transport: String,
    /// "ok" or "error".
    pub status: String,
    pub error: Option<String>,
    pub latency_ms: Option<u64>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
}

impl RequestEntry {
    /// Successful turn with a Responses-format usage object.
    pub fn ok(
        provider: &str,
        model: &str,
        transport: &str,
        latency_ms: Option<u64>,
        usage: &serde_json::Value,
    ) -> Option<Self> {
        let input = usage
            .get("input_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let output = usage
            .get("output_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        if input == 0 && output == 0 {
            return None; // nothing useful to store
        }
        let cached = usage
            .pointer("/input_tokens_details/cached_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        Some(Self {
            ts: now_unix(),
            provider: provider.to_string(),
            model: model.to_string(),
            transport: transport.to_string(),
            status: "ok".to_string(),
            error: None,
            latency_ms,
            input_tokens: input,
            output_tokens: output,
            cached_tokens: cached,
        })
    }

    /// Failed turn (upstream error, routing failure, ...).
    pub fn error(
        provider: &str,
        model: &str,
        transport: &str,
        latency_ms: Option<u64>,
        error: &str,
    ) -> Self {
        Self {
            ts: now_unix(),
            provider: provider.to_string(),
            model: model.to_string(),
            transport: transport.to_string(),
            status: "error".to_string(),
            error: Some(error.chars().take(500).collect()),
            latency_ms,
            input_tokens: 0,
            output_tokens: 0,
            cached_tokens: 0,
        }
    }
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

fn db_path() -> PathBuf {
    crate::config::config_dir().join("loom.db")
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS requests (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    ts            INTEGER NOT NULL,
    provider      TEXT NOT NULL,
    model         TEXT NOT NULL,
    transport     TEXT NOT NULL DEFAULT 'http',
    status        TEXT NOT NULL DEFAULT 'ok',
    error         TEXT,
    latency_ms    INTEGER,
    input_tokens  INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    cached_tokens INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_requests_ts ON requests(ts);
";

impl Stats {
    pub fn load() -> Self {
        let path = db_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let stats = Self::open_at(&path).unwrap_or_else(|e| {
            tracing::warn!("failed to open stats db ({e}); using in-memory db");
            Self::open_at(Path::new(":memory:")).expect("in-memory sqlite")
        });
        stats.migrate_legacy_json();
        stats
    }

    /// In-memory database, for tests.
    pub fn in_memory() -> Self {
        Self::open_at(Path::new(":memory:")).expect("in-memory sqlite")
    }

    fn open_at(path: &Path) -> rusqlite::Result<Self> {
        let conn = rusqlite::Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: std::sync::Mutex::new(conn),
        })
    }

    /// Import records from the pre-SQLite `stats.json`, then rename it so
    /// the migration only runs once.
    fn migrate_legacy_json(&self) {
        let path = crate::config::config_dir().join("stats.json");
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&raw) else {
            return;
        };
        let Some(records) = json.get("records").and_then(|r| r.as_array()) else {
            return;
        };
        let mut imported = 0u64;
        for r in records {
            let entry = RequestEntry {
                ts: r.get("ts").and_then(|v| v.as_u64()).unwrap_or_else(now_unix),
                provider: r
                    .get("provider")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
                model: r
                    .get("model")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
                transport: "http".to_string(),
                status: "ok".to_string(),
                error: None,
                latency_ms: None,
                input_tokens: r.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                output_tokens: r.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                cached_tokens: r.get("cached_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
            };
            if self.insert(&entry).is_ok() {
                imported += 1;
            }
        }
        if imported > 0 {
            tracing::info!(imported, "migrated legacy stats.json into sqlite");
        }
        let _ = std::fs::rename(&path, path.with_extension("json.migrated"));
    }

    fn insert(&self, e: &RequestEntry) -> rusqlite::Result<()> {
        let conn = self.conn.lock().map_err(|_| {
            rusqlite::Error::InvalidParameterName("stats db lock poisoned".into())
        })?;
        conn.execute(
            "INSERT INTO requests
             (ts, provider, model, transport, status, error, latency_ms,
              input_tokens, output_tokens, cached_tokens)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                e.ts as i64,
                e.provider,
                e.model,
                e.transport,
                e.status,
                e.error,
                e.latency_ms.map(|v| v as i64),
                e.input_tokens as i64,
                e.output_tokens as i64,
                e.cached_tokens as i64,
            ],
        )?;
        Ok(())
    }

    /// Record one completed turn. `usage` is a Responses-format usage
    /// object ({input_tokens, output_tokens, input_tokens_details}).
    pub fn record(&self, provider: &str, model: &str, usage: &serde_json::Value) {
        if let Some(entry) = RequestEntry::ok(provider, model, "http", None, usage) {
            let _ = self.insert(&entry);
        }
    }

    /// Record a full entry (success or failure, with transport/latency).
    pub fn record_entry(&self, entry: RequestEntry) {
        let _ = self.insert(&entry);
    }

    pub fn summarize(&self, period_secs: u64) -> StatsSummary {
        let cutoff = now_unix().saturating_sub(period_secs) as i64;
        let mut summary = StatsSummary {
            period_secs,
            requests: 0,
            input_tokens: 0,
            output_tokens: 0,
            cached_tokens: 0,
            cache_ratio: 0.0,
            per_provider: Vec::new(),
        };
        let Ok(conn) = self.conn.lock() else {
            return summary;
        };
        let mut stmt = match conn.prepare(
            "SELECT provider,
                    COUNT(*),
                    COALESCE(SUM(input_tokens), 0),
                    COALESCE(SUM(output_tokens), 0),
                    COALESCE(SUM(cached_tokens), 0)
             FROM requests
             WHERE ts >= ?1 AND status = 'ok'
             GROUP BY provider
             ORDER BY provider",
        ) {
            Ok(s) => s,
            Err(_) => return summary,
        };
        let rows = stmt.query_map([cutoff], |row| {
            Ok(ProviderAggregate {
                provider: row.get(0)?,
                requests: row.get::<_, i64>(1)? as u64,
                input_tokens: row.get::<_, i64>(2)? as u64,
                output_tokens: row.get::<_, i64>(3)? as u64,
                cached_tokens: row.get::<_, i64>(4)? as u64,
            })
        });
        if let Ok(rows) = rows {
            for agg in rows.flatten() {
                summary.requests += agg.requests;
                summary.input_tokens += agg.input_tokens;
                summary.output_tokens += agg.output_tokens;
                summary.cached_tokens += agg.cached_tokens;
                summary.per_provider.push(agg);
            }
        }
        if summary.input_tokens > 0 {
            summary.cache_ratio = summary.cached_tokens as f64 / summary.input_tokens as f64;
        }
        summary
    }

    /// Most recent requests, newest first — feeds a future Logs tab.
    pub fn recent(&self, limit: u32) -> Vec<RequestEntry> {
        let Ok(conn) = self.conn.lock() else {
            return Vec::new();
        };
        let mut stmt = match conn.prepare(
            "SELECT ts, provider, model, transport, status, error, latency_ms,
                    input_tokens, output_tokens, cached_tokens
             FROM requests ORDER BY ts DESC, id DESC LIMIT ?1",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map([limit as i64], |row| {
            Ok(RequestEntry {
                ts: row.get::<_, i64>(0)? as u64,
                provider: row.get(1)?,
                model: row.get(2)?,
                transport: row.get(3)?,
                status: row.get(4)?,
                error: row.get(5)?,
                latency_ms: row.get::<_, Option<i64>>(6)?.map(|v| v as u64),
                input_tokens: row.get::<_, i64>(7)? as u64,
                output_tokens: row.get::<_, i64>(8)? as u64,
                cached_tokens: row.get::<_, i64>(9)? as u64,
            })
        });
        rows.map(|r| r.flatten().collect()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_stats() -> Stats {
        Stats::open_at(Path::new(":memory:")).unwrap()
    }

    #[test]
    fn summarize_aggregates_per_provider() {
        let s = test_stats();
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
        let s = test_stats();
        s.record("kimi", "k3", &json!({"input_tokens": 0, "output_tokens": 0}));
        assert_eq!(s.summarize(86_400).requests, 0);
    }

    #[test]
    fn failed_requests_are_logged_but_excluded_from_token_stats() {
        let s = test_stats();
        let usage = json!({"input_tokens": 10, "output_tokens": 5});
        if let Some(e) = RequestEntry::ok("kimi", "k3", "ws", Some(1200), &usage) {
            s.record_entry(e);
        }
        s.record_entry(RequestEntry::error(
            "kimi",
            "k3",
            "http",
            Some(300),
            "provider returned 401: Invalid Authentication",
        ));

        let sum = s.summarize(86_400);
        assert_eq!(sum.requests, 1); // only the successful one counts tokens

        let log = s.recent(10);
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].status, "error"); // newest first
        assert_eq!(log[0].latency_ms, Some(300));
        assert!(log[0].error.as_deref().unwrap().contains("401"));
        assert_eq!(log[1].transport, "ws");
    }

    #[test]
    fn persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("loom.db");
        {
            let s = Stats::open_at(&path).unwrap();
            s.record("kimi", "k3", &json!({"input_tokens": 7, "output_tokens": 3}));
        }
        let s = Stats::open_at(&path).unwrap();
        assert_eq!(s.summarize(86_400).requests, 1);
    }
}
