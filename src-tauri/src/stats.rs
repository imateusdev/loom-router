//! Request statistics and per-request logs, stored in a local SQLite
//! database at `~/.loomrouter/loom.db`. Recorded by the proxy and
//! aggregated for the Overview page (and a future per-request Logs tab).
//!
//! On first open, any legacy `stats.json` is migrated into the database
//! and renamed to `stats.json.migrated`.
//!
//! Retention: the `requests` log is pruned at startup and again every
//! ~500 inserts, keeping at most the last `LOOM_STATS_RETENTION_DAYS`
//! days (default 90) and at most `LOOM_STATS_MAX_ROWS` rows
//! (default 100_000).

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;

pub type SharedStats = Arc<RwLock<Stats>>;

/// Default retention window for the `requests` log, in days.
/// Overridable via the `LOOM_STATS_RETENTION_DAYS` env var.
pub const DEFAULT_RETENTION_DAYS: u64 = 90;
/// Default hard cap on stored rows.
/// Overridable via the `LOOM_STATS_MAX_ROWS` env var.
pub const DEFAULT_MAX_ROWS: u64 = 100_000;
/// Prune cadence: one sweep every N inserts (in addition to startup).
const PRUNE_EVERY_INSERTS: u64 = 500;

pub struct Stats {
    // rusqlite::Connection is Send but not Sync; the mutex serializes
    // access so Stats is Sync and can live behind the shared tokio
    // RwLock. The Arc lets blocking closures (spawn_blocking) share the
    // same connection. All public methods take `&self`, so callers only
    // ever need a read() guard on the outer RwLock.
    conn: Arc<std::sync::Mutex<rusqlite::Connection>>,
    /// Total inserts since open, used to schedule periodic prune sweeps.
    inserts_since_open: AtomicU64,
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
    /// Estimated USD cost, filled at query time from the pricing table.
    /// None for subscription/unknown models.
    pub cost_usd: Option<f64>,
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
            // Nothing useful to store. Logged because an unrecognised usage
            // dialect looks exactly like this, and used to fail silently —
            // callers must normalize via `translate::normalize_usage` first.
            tracing::debug!(
                provider,
                model,
                "usage carried no token counts; not recorded"
            );
            return None;
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
            cost_usd: None,
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
            cost_usd: None,
        }
    }
}

/// One model's behaviour over the summarised window.
///
/// The grouping query has always been per `(provider, model)`; the model
/// dimension was computed and then folded away, so the UI could only ever
/// show a provider total. These are the numbers that actually distinguish
/// one model from another: how fast it answers, how much of its input it
/// gets for the cached price, and how often it fails.
#[derive(Debug, Clone, Serialize)]
pub struct ModelAggregate {
    pub model: String,
    /// Successful turns.
    pub requests: u64,
    /// Failed turns in the same window.
    pub errors: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    /// cached / input, 0..1.
    pub cache_ratio: f64,
    /// Mean latency of successful turns, ms. None when none succeeded.
    pub avg_latency_ms: Option<u64>,
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderAggregate {
    pub provider: String,
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    /// Sum of per-model estimates; None when no request had a known price.
    pub cost_usd: Option<f64>,
    /// The models behind this provider's totals, busiest first.
    pub models: Vec<ModelAggregate>,
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
    /// Total estimated USD across priced models (0 when none priced).
    pub cost_usd: f64,
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

// ---------------------------------------------------------------------------
// Estimated pricing (USD per 1M tokens: input, output, cached input).
// Best-effort public list prices, matched by substring on the model id —
// they drift over time, so treat the numbers as estimates, not invoices.
// Subscription/quota plans (e.g. Kimi Code) are intentionally absent: there
// is no per-token price to estimate.
// ---------------------------------------------------------------------------

const PRICES: &[(&str, f64, f64, f64)] = &[
    // (pattern, input $/1M, output $/1M, cached input $/1M)
    ("gpt-5", 1.25, 10.0, 0.125),
    ("deepseek-v4-pro", 1.74, 3.48, 0.145),
    ("deepseek-v4-flash", 0.14, 0.28, 0.028),
    ("deepseek-reasoner", 0.55, 2.19, 0.14),
    ("deepseek-chat", 0.27, 1.10, 0.07),
    ("kimi-k3", 3.00, 15.00, 0.30),
    ("kimi-k2", 0.60, 2.50, 0.15),
    ("glm-5", 1.40, 4.40, 0.26),
    ("minimax-m", 0.30, 1.20, 0.06),
    ("claude-sonnet", 3.00, 15.00, 0.30),
    ("claude-opus", 5.00, 25.00, 0.50),
    ("claude-haiku", 1.00, 5.00, 0.10),
];

/// Estimated cost in USD for one request, None when the model has no
/// per-token pricing (subscriptions, unknown models).
///
/// In the OpenAI usage object `cached_tokens` is a *subset* of
/// `input_tokens`, so cached tokens are billed at the cached rate and
/// only the remainder at the full input rate:
/// `(input - cached) * pin + cached * pcached + output * pout`.
pub fn estimate_cost(model: &str, input: u64, output: u64, cached: u64) -> Option<f64> {
    let (_, pin, pout, pcached) = PRICES.iter().find(|(pat, ..)| model.contains(pat))?;
    // Clamp defensively: some providers may report cached > input.
    let cached = cached.min(input);
    let uncached = input - cached;
    Some((uncached as f64 * pin + cached as f64 * pcached + output as f64 * pout) / 1_000_000.0)
}

// Idempotent migration: CREATE TABLE/INDEX IF NOT EXISTS run on every
// open, so adding an index here applies to existing databases too.
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
-- Matches the summarize filter (ts >= ? AND status = 'ok').
CREATE INDEX IF NOT EXISTS idx_requests_status_ts ON requests(status, ts);
";

/// Run a piece of SQLite work off the async runtime's core workers, so a
/// slow disk never stalls request handling. When no tokio runtime is
/// present (unit tests, very early startup) the work runs inline.
fn dispatch_db(work: impl FnOnce() + Send + 'static) {
    if tokio::runtime::Handle::try_current().is_ok() {
        // Fire-and-forget: the JoinHandle is dropped on purpose, the
        // blocking task keeps running detached.
        drop(tokio::task::spawn_blocking(work));
    } else {
        work();
    }
}

fn retention_days() -> u64 {
    std::env::var("LOOM_STATS_RETENTION_DAYS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&d| d > 0)
        .unwrap_or(DEFAULT_RETENTION_DAYS)
}

fn max_rows() -> u64 {
    std::env::var("LOOM_STATS_MAX_ROWS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_MAX_ROWS)
}

fn insert_row(conn: &rusqlite::Connection, e: &RequestEntry) -> rusqlite::Result<()> {
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

/// Retention sweep: drop rows older than `retention_days`, then trim to
/// the newest `max_rows` rows. Idempotent; safe to run at every startup.
fn prune_conn(
    conn: &rusqlite::Connection,
    retention_days: u64,
    max_rows: u64,
) -> rusqlite::Result<()> {
    let cutoff = now_unix().saturating_sub(retention_days.saturating_mul(86_400)) as i64;
    conn.execute("DELETE FROM requests WHERE ts < ?1", [cutoff])?;
    conn.execute(
        "DELETE FROM requests WHERE id NOT IN (
             SELECT id FROM requests ORDER BY ts DESC, id DESC LIMIT ?1)",
        [max_rows as i64],
    )?;
    Ok(())
}

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
        // Startup retention sweep (idempotent), so a long-untouched db
        // shrinks before serving new traffic.
        let _ = prune_conn(&conn, retention_days(), max_rows());
        Ok(Self {
            conn: Arc::new(std::sync::Mutex::new(conn)),
            inserts_since_open: AtomicU64::new(0),
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
                ts: r
                    .get("ts")
                    .and_then(|v| v.as_u64())
                    .unwrap_or_else(now_unix),
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
                cost_usd: None,
            };
            self.insert(&entry);
            imported += 1;
        }
        if imported > 0 {
            tracing::info!(imported, "migrated legacy stats.json into sqlite");
        }
        let _ = std::fs::rename(&path, path.with_extension("json.migrated"));
    }

    /// Enqueue one row for insertion. The SQLite write itself runs on the
    /// blocking thread pool (see `dispatch_db`), never on a core worker
    /// of the async runtime. Every `PRUNE_EVERY_INSERTS` inserts a
    /// retention sweep runs in the same blocking task.
    fn insert(&self, e: &RequestEntry) {
        let conn = Arc::clone(&self.conn);
        let entry = e.clone();
        let prune = self
            .inserts_since_open
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(PRUNE_EVERY_INSERTS);
        dispatch_db(move || {
            let Ok(conn) = conn.lock() else {
                tracing::warn!("stats db lock poisoned; dropping request entry");
                return;
            };
            // Never silently drop a write: a swallowed error here is
            // indistinguishable from "no traffic" in the dashboard.
            if let Err(e) = insert_row(&conn, &entry) {
                tracing::warn!(error = %e, provider = %entry.provider, model = %entry.model,
                    "failed to record request in stats db");
            }
            if prune {
                if let Err(e) = prune_conn(&conn, retention_days(), max_rows()) {
                    tracing::warn!(error = %e, "stats retention sweep failed");
                }
            }
        });
    }

    /// Record one completed turn. `usage` is a Responses-format usage
    /// object ({input_tokens, output_tokens, input_tokens_details}).
    pub fn record(&self, provider: &str, model: &str, usage: &serde_json::Value) {
        if let Some(entry) = RequestEntry::ok(provider, model, "http", None, usage) {
            self.insert(&entry);
        }
    }

    /// Record a full entry (success or failure, with transport/latency).
    pub fn record_entry(&self, entry: RequestEntry) {
        self.insert(&entry);
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
            cost_usd: 0.0,
            per_provider: Vec::new(),
        };
        let Ok(conn) = self.conn.lock() else {
            return summary;
        };
        // Group by (provider, model): each model's price applies to its own
        // token sums, and the per-model row is now kept rather than folded
        // away. Successes and failures are counted in the same pass with
        // conditional aggregates — the totals below still mean "successful
        // requests", so the tray and the summary tiles are unchanged.
        let mut stmt = match conn.prepare(
            "SELECT provider,
                    model,
                    COALESCE(SUM(CASE WHEN status = 'ok' THEN 1 ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN status <> 'ok' THEN 1 ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN status = 'ok' THEN input_tokens ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN status = 'ok' THEN output_tokens ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN status = 'ok' THEN cached_tokens ELSE 0 END), 0),
                    AVG(CASE WHEN status = 'ok' THEN latency_ms END)
             FROM requests
             WHERE ts >= ?1
             GROUP BY provider, model
             ORDER BY provider, model",
        ) {
            Ok(s) => s,
            Err(_) => return summary,
        };
        let rows = stmt.query_map([cutoff], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)? as u64,
                row.get::<_, i64>(3)? as u64,
                row.get::<_, i64>(4)? as u64,
                row.get::<_, i64>(5)? as u64,
                row.get::<_, i64>(6)? as u64,
                row.get::<_, Option<f64>>(7)?,
            ))
        });
        if let Ok(rows) = rows {
            for (provider, model, requests, errors, input, output, cached, latency) in
                rows.flatten()
            {
                // A model with only failures still belongs in the list — the
                // failures are the point — but it contributes no tokens.
                if requests == 0 && errors == 0 {
                    continue;
                }
                let cost = estimate_cost(&model, input, output, cached);
                summary.requests += requests;
                summary.input_tokens += input;
                summary.output_tokens += output;
                summary.cached_tokens += cached;
                summary.cost_usd += cost.unwrap_or(0.0);
                let agg = match summary
                    .per_provider
                    .iter_mut()
                    .find(|a| a.provider == provider)
                {
                    Some(a) => a,
                    None => {
                        summary.per_provider.push(ProviderAggregate {
                            provider: provider.clone(),
                            requests: 0,
                            input_tokens: 0,
                            output_tokens: 0,
                            cached_tokens: 0,
                            cost_usd: None,
                            models: Vec::new(),
                        });
                        summary.per_provider.last_mut().expect("just pushed")
                    }
                };
                agg.requests += requests;
                agg.input_tokens += input;
                agg.output_tokens += output;
                agg.cached_tokens += cached;
                if let Some(c) = cost {
                    *agg.cost_usd.get_or_insert(0.0) += c;
                }
                agg.models.push(ModelAggregate {
                    model,
                    requests,
                    errors,
                    input_tokens: input,
                    output_tokens: output,
                    cached_tokens: cached,
                    cache_ratio: if input > 0 {
                        cached as f64 / input as f64
                    } else {
                        0.0
                    },
                    avg_latency_ms: latency.map(|v| v.round() as u64),
                    cost_usd: cost,
                });
            }
        }
        // Busiest model first: the one carrying the traffic is the one the
        // user is comparing everything else against.
        for agg in &mut summary.per_provider {
            agg.models
                .sort_by(|a, b| b.requests.cmp(&a.requests).then(a.model.cmp(&b.model)));
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
            let model = row.get::<_, String>(2)?;
            let input = row.get::<_, i64>(7)? as u64;
            let output = row.get::<_, i64>(8)? as u64;
            let cached = row.get::<_, i64>(9)? as u64;
            Ok(RequestEntry {
                ts: row.get::<_, i64>(0)? as u64,
                provider: row.get(1)?,
                cost_usd: estimate_cost(&model, input, output, cached),
                model,
                transport: row.get(3)?,
                status: row.get(4)?,
                error: row.get(5)?,
                latency_ms: row.get::<_, Option<i64>>(6)?.map(|v| v as u64),
                input_tokens: input,
                output_tokens: output,
                cached_tokens: cached,
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
        s.record(
            "kimi",
            "k3",
            &json!({"input_tokens": 0, "output_tokens": 0}),
        );
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
    fn cost_estimates_follow_model_pricing() {
        // gpt-5*: $1.25 in / $10 out / $0.125 cached per 1M tokens.
        // cached_tokens is a subset of input_tokens in the OpenAI usage
        // object, so a fully-cached 1M-token prompt costs 1M * $0.125
        // (cached rate), NOT 1M * $1.25 + 1M * $0.125.
        let c = estimate_cost("gpt-5.5", 1_000_000, 100_000, 1_000_000).unwrap();
        assert!((c - (0.125 + 1.0)).abs() < 1e-9);
        // Partial cache: 400k of 1M cached -> 600k * 1.25 + 400k * 0.125.
        let c = estimate_cost("gpt-5.5", 1_000_000, 0, 400_000).unwrap();
        assert!((c - (0.75 + 0.05)).abs() < 1e-9);
        // Subscription models (Kimi Code "k3") have no per-token estimate.
        assert!(estimate_cost("k3", 1_000, 1_000, 0).is_none());

        let s = test_stats();
        s.record(
            "codex-native",
            "gpt-5.5",
            &json!({"input_tokens": 1_000_000u64, "output_tokens": 0u64}),
        );
        let sum = s.summarize(86_400);
        assert!((sum.cost_usd - 1.25).abs() < 1e-9);
        assert_eq!(sum.per_provider[0].cost_usd, Some(1.25));
    }

    #[test]
    fn prune_enforces_retention_and_row_cap() {
        let s = test_stats();
        let mk = |ts: u64| RequestEntry {
            ts,
            provider: "kimi".into(),
            model: "k3".into(),
            transport: "http".into(),
            status: "ok".into(),
            error: None,
            latency_ms: None,
            input_tokens: 1,
            output_tokens: 1,
            cached_tokens: 0,
            cost_usd: None,
        };
        // One row far outside a 90-day retention window...
        s.record_entry(mk(now_unix() - 200 * 86_400));
        // ...plus 20 fresh rows.
        for i in 0..20u64 {
            s.record_entry(mk(now_unix() - i));
        }
        {
            let conn = s.conn.lock().unwrap();
            prune_conn(&conn, 90, 10).unwrap();
        }
        let rows = s.recent(100);
        assert_eq!(rows.len(), 10); // hard row cap keeps the newest
        assert!(rows.iter().all(|r| r.ts > now_unix() - 90 * 86_400));
    }

    #[test]
    fn persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("loom.db");
        {
            let s = Stats::open_at(&path).unwrap();
            s.record(
                "kimi",
                "k3",
                &json!({"input_tokens": 7, "output_tokens": 3}),
            );
        }
        let s = Stats::open_at(&path).unwrap();
        assert_eq!(s.summarize(86_400).requests, 1);
    }
}
