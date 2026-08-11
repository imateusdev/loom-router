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

use serde::{Deserialize, Serialize};
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

/// Provenance for one image analysed during a request. This deliberately
/// excludes the image, prompt, evidence, and provider credentials.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VisualImageProvenance {
    pub model: String,
    pub attempts: u32,
    pub duration_ms: u64,
    pub cache_hit: bool,
}

/// Redacted summary of one visual-provider call. Error text never includes
/// request bodies, source image URLs, evidence, or credentials.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VisualAttemptProvenance {
    pub model: String,
    pub retryable: bool,
    pub status: Option<u16>,
    pub duration_ms: u64,
    pub error: String,
}

/// Visual-analysis provenance grouped by request. Per-image records prevent a
/// multi-image request from claiming one model or cache state for every image.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VisualAssistanceMetadata {
    pub images: Vec<VisualImageProvenance>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attempts: Vec<VisualAttemptProvenance>,
}

/// One recorded request (a completed or failed turn through the proxy).
#[derive(Debug, Clone, Serialize)]
pub struct RequestEntry {
    pub ts: u64,
    pub provider: String,
    pub model: String,
    /// Stable key id that served this request; None for legacy/provider rows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_id: Option<String>,
    /// "http" or "ws".
    pub transport: String,
    /// Log category for triage: "request" for normal turns, or a specific
    /// label such as "compaction" for auxiliary/protocol diagnostics.
    pub kind: String,
    /// "ok" or "error".
    pub status: String,
    pub error: Option<String>,
    pub latency_ms: Option<u64>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visual_assistance: Option<VisualAssistanceMetadata>,
    /// How the turn ended: the upstream's own `finish_reason` when it speaks
    /// Chat Completions, otherwise derived from what the turn produced. A turn
    /// that announced an action and handed control back records "stop" with no
    /// tool call, which is what separates a model that chose to stop from a
    /// routing fault.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    /// Estimated USD cost, filled at query time from the pricing table.
    /// None for subscription/unknown models.
    pub cost_usd: Option<f64>,
}

impl RequestEntry {
    fn ok_entry(
        provider: &str,
        model: &str,
        transport: &str,
        latency_ms: Option<u64>,
        usage: &serde_json::Value,
    ) -> Self {
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
        Self {
            ts: now_unix(),
            provider: provider.to_string(),
            model: model.to_string(),
            key_id: None,
            transport: transport.to_string(),
            kind: "request".to_string(),
            status: "ok".to_string(),
            error: None,
            latency_ms,
            input_tokens: input,
            output_tokens: output,
            cached_tokens: cached,
            visual_assistance: None,
            finish_reason: None,
            cost_usd: None,
        }
    }

    /// Successful turn with a Responses-format usage object.
    pub fn ok(
        provider: &str,
        model: &str,
        transport: &str,
        latency_ms: Option<u64>,
        usage: &serde_json::Value,
    ) -> Option<Self> {
        let entry = Self::ok_entry(provider, model, transport, latency_ms, usage);
        let input = entry.input_tokens;
        let output = entry.output_tokens;
        if input == 0 && output == 0 {
            // Nothing useful to store. Logged because an unrecognised usage
            // dialect looks exactly like this, and used to fail silently -
            // callers must normalize via `translate::normalize_usage` first.
            tracing::debug!(
                provider,
                model,
                "usage carried no token counts; not recorded"
            );
            return None;
        }
        Some(entry)
    }

    /// Successful routed turn. Unlike `ok`, zero-token responses are still
    /// recorded so the dashboard can count requests per key.
    pub fn ok_with_key(
        provider: &str,
        model: &str,
        transport: &str,
        latency_ms: Option<u64>,
        key_id: &str,
        usage: &serde_json::Value,
    ) -> Self {
        let entry = Self::ok_entry(provider, model, transport, latency_ms, usage);
        if entry.input_tokens == 0 && entry.output_tokens == 0 {
            // Kept, unlike `ok`, so per-key request counts stay right - but
            // still logged, because an unrecognised usage dialect looks
            // exactly like this and would otherwise fail silently.
            tracing::debug!(
                provider,
                model,
                key_id,
                "usage carried no token counts; recorded without them"
            );
        }
        entry.with_key_id(Some(key_id.to_string()))
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
            key_id: None,
            transport: transport.to_string(),
            kind: "request".to_string(),
            status: "error".to_string(),
            error: Some(error.chars().take(500).collect()),
            latency_ms,
            input_tokens: 0,
            output_tokens: 0,
            cached_tokens: 0,
            visual_assistance: None,
            finish_reason: None,
            cost_usd: None,
        }
    }

    /// Failed diagnostic event, not a normal provider request. Kept separate
    /// so logs can surface protocol/compaction problems without pretending
    /// they are ordinary provider errors.
    pub fn problem(
        provider: &str,
        model: &str,
        transport: &str,
        latency_ms: Option<u64>,
        kind: &str,
        error: &str,
    ) -> Self {
        Self {
            ts: now_unix(),
            provider: provider.to_string(),
            model: model.to_string(),
            key_id: None,
            transport: transport.to_string(),
            kind: kind.to_string(),
            status: "error".to_string(),
            error: Some(error.chars().take(500).collect()),
            latency_ms,
            input_tokens: 0,
            output_tokens: 0,
            cached_tokens: 0,
            visual_assistance: None,
            finish_reason: None,
            cost_usd: None,
        }
    }

    pub fn with_kind(mut self, kind: &str) -> Self {
        self.kind = kind.to_string();
        self
    }

    pub fn with_visual_assistance(mut self, metadata: Option<VisualAssistanceMetadata>) -> Self {
        self.visual_assistance = metadata;
        self
    }

    pub fn with_finish_reason(mut self, finish_reason: Option<String>) -> Self {
        self.finish_reason = finish_reason;
        self
    }

    pub fn with_key_id(mut self, key_id: Option<String>) -> Self {
        self.key_id = key_id;
        self
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
pub struct KeyUsage {
    pub key_id: String,
    /// Filled by the command layer from current config; empty while
    /// aggregating raw SQLite rows.
    pub key_name: String,
    pub requests: u64,
    pub errors: u64,
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
    /// Total estimated USD across priced models (0 when none priced).
    pub cost_usd: f64,
    pub per_provider: Vec<ProviderAggregate>,
    /// Always serialized, even when empty: the webview types this as a
    /// required array and omitting it makes `per_key.find(...)` throw during
    /// render, which unmounts the whole React tree into a blank window.
    pub per_key: Vec<KeyUsage>,
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
// Best-effort public list prices, matched by substring on the model id -
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
    kind          TEXT NOT NULL DEFAULT 'request',
    status        TEXT NOT NULL DEFAULT 'ok',
    error         TEXT,
    latency_ms    INTEGER,
    input_tokens  INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    cached_tokens INTEGER NOT NULL DEFAULT 0,
    visual_assistance TEXT,
    key_id TEXT,
    finish_reason TEXT
);
CREATE INDEX IF NOT EXISTS idx_requests_ts ON requests(ts);
-- Matches the summarize filter (ts >= ? AND status = 'ok').
CREATE INDEX IF NOT EXISTS idx_requests_status_ts ON requests(status, ts);
";

/// SQLite's `CREATE TABLE IF NOT EXISTS` does not add new columns to an
/// existing request log, so migrate the optional provenance field separately.
fn ensure_visual_assistance_column(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    let mut statement = conn.prepare("PRAGMA table_info(requests)")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        if row.get::<_, String>(1)? == "visual_assistance" {
            return Ok(());
        }
    }
    conn.execute("ALTER TABLE requests ADD COLUMN visual_assistance TEXT", [])?;
    Ok(())
}

/// Same as the visual column: schema creation is idempotent, but existing
/// databases need an explicit ALTER when a new column is added.
fn ensure_kind_column(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    let mut statement = conn.prepare("PRAGMA table_info(requests)")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        if row.get::<_, String>(1)? == "kind" {
            return Ok(());
        }
    }
    conn.execute(
        "ALTER TABLE requests ADD COLUMN kind TEXT NOT NULL DEFAULT 'request'",
        [],
    )?;
    Ok(())
}

/// Optional key attribution for requests recorded after multiple provider
/// keys shipped. Older rows keep a NULL key_id and stay provider-level.
fn ensure_key_id_column(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    let mut statement = conn.prepare("PRAGMA table_info(requests)")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        if row.get::<_, String>(1)? == "key_id" {
            return Ok(());
        }
    }
    conn.execute("ALTER TABLE requests ADD COLUMN key_id TEXT", [])?;
    Ok(())
}

/// How a turn ended. Without it, a turn that answered with a tool call and one
/// that just talked and handed control back look identical in the log - which
/// is the whole question when an agent announces an action and then stops.
fn ensure_finish_reason_column(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    let mut statement = conn.prepare("PRAGMA table_info(requests)")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        if row.get::<_, String>(1)? == "finish_reason" {
            return Ok(());
        }
    }
    conn.execute("ALTER TABLE requests ADD COLUMN finish_reason TEXT", [])?;
    Ok(())
}

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
    let visual_assistance = e
        .visual_assistance
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    conn.execute(
        "INSERT INTO requests
         (ts, provider, model, transport, kind, status, error, latency_ms,
          input_tokens, output_tokens, cached_tokens, visual_assistance, key_id,
          finish_reason)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        rusqlite::params![
            e.ts as i64,
            e.provider,
            e.model,
            e.transport,
            e.kind,
            e.status,
            e.error,
            e.latency_ms.map(|v| v as i64),
            e.input_tokens as i64,
            e.output_tokens as i64,
            e.cached_tokens as i64,
            visual_assistance,
            e.key_id,
            e.finish_reason,
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
        ensure_visual_assistance_column(&conn)?;
        ensure_kind_column(&conn)?;
        ensure_key_id_column(&conn)?;
        ensure_finish_reason_column(&conn)?;
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
                key_id: None,
                transport: "http".to_string(),
                kind: "request".to_string(),
                status: "ok".to_string(),
                error: None,
                latency_ms: None,
                input_tokens: r.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                output_tokens: r.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                cached_tokens: r.get("cached_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                visual_assistance: None,
                finish_reason: None,
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
            per_key: Vec::new(),
        };
        let Ok(conn) = self.conn.lock() else {
            return summary;
        };
        // Group by (provider, model): each model's price applies to its own
        // token sums, and the per-model row is now kept rather than folded
        // away. Successes and failures are counted in the same pass with
        // conditional aggregates - the totals below still mean "successful
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
                // A model with only failures still belongs in the list - the
                // failures are the point - but it contributes no tokens.
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
        let mut key_stmt = match conn.prepare(
            "SELECT key_id,
                    COALESCE(SUM(CASE WHEN status = 'ok' THEN 1 ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN status <> 'ok' THEN 1 ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN status = 'ok' THEN input_tokens ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN status = 'ok' THEN output_tokens ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN status = 'ok' THEN cached_tokens ELSE 0 END), 0)
             FROM requests
             WHERE ts >= ?1 AND key_id IS NOT NULL AND key_id <> ''
             GROUP BY key_id
             ORDER BY key_id",
        ) {
            Ok(s) => s,
            Err(_) => return summary,
        };
        if let Ok(rows) = key_stmt.query_map([cutoff], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)? as u64,
                row.get::<_, i64>(2)? as u64,
                row.get::<_, i64>(3)? as u64,
                row.get::<_, i64>(4)? as u64,
                row.get::<_, i64>(5)? as u64,
            ))
        }) {
            for (key_id, requests, errors, input, output, cached) in rows.flatten() {
                if requests == 0 && errors == 0 {
                    continue;
                }
                summary.per_key.push(KeyUsage {
                    key_id,
                    key_name: String::new(),
                    requests,
                    errors,
                    input_tokens: input,
                    output_tokens: output,
                    cached_tokens: cached,
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

    /// Most recent requests, newest first - feeds a future Logs tab.
    pub fn recent(&self, limit: u32) -> Vec<RequestEntry> {
        let Ok(conn) = self.conn.lock() else {
            return Vec::new();
        };
        let mut stmt = match conn.prepare(
            "SELECT ts, provider, model, transport, kind, status, error, latency_ms,
                    input_tokens, output_tokens, cached_tokens, visual_assistance, key_id,
                    finish_reason
             FROM requests ORDER BY ts DESC, id DESC LIMIT ?1",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map([limit as i64], |row| {
            let model = row.get::<_, String>(2)?;
            let input = row.get::<_, i64>(8)? as u64;
            let output = row.get::<_, i64>(9)? as u64;
            let cached = row.get::<_, i64>(10)? as u64;
            let visual_assistance = row
                .get::<_, Option<String>>(11)?
                .and_then(|raw| serde_json::from_str(&raw).ok());
            Ok(RequestEntry {
                ts: row.get::<_, i64>(0)? as u64,
                provider: row.get(1)?,
                key_id: row.get(12)?,
                cost_usd: estimate_cost(&model, input, output, cached),
                model,
                transport: row.get(3)?,
                kind: row.get(4)?,
                status: row.get(5)?,
                error: row.get(6)?,
                latency_ms: row.get::<_, Option<i64>>(7)?.map(|v| v as u64),
                input_tokens: input,
                output_tokens: output,
                cached_tokens: cached,
                visual_assistance,
                finish_reason: row.get(13)?,
            })
        });
        rows.map(|r| r.flatten().collect()).unwrap_or_default()
    }

    /// Newest request row id, used as the validation boundary cursor.
    pub fn latest_request_id(&self) -> Option<i64> {
        let Ok(conn) = self.conn.lock() else {
            return None;
        };
        conn.query_row("SELECT MAX(id) FROM requests", [], |row| row.get(0))
            .ok()
            .flatten()
    }

    /// First successful request and whether any request failed after the
    /// validation boundary, from the full log rather than `recent`'s window.
    pub fn validation_since(&self, started_at: u64, after_id: Option<i64>) -> (Option<u64>, bool) {
        let Ok(conn) = self.conn.lock() else {
            return (None, false);
        };
        let mut stmt = match conn.prepare(
            "SELECT MIN(CASE WHEN status = 'ok' THEN ts END),
                    COALESCE(MAX(CASE WHEN status = 'error' THEN 1 ELSE 0 END), 0)
             FROM requests
             WHERE ts > ?1 OR (ts = ?1 AND (?2 IS NULL OR id > ?2))",
        ) {
            Ok(s) => s,
            Err(_) => return (None, false),
        };
        stmt.query_row(rusqlite::params![started_at as i64, after_id], |row| {
            let first_ok = row.get::<_, Option<i64>>(0)?.map(|v| v as u64);
            let failed = row.get::<_, i64>(1)? != 0;
            Ok((first_ok, failed))
        })
        .unwrap_or((None, false))
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
    fn visual_metadata_from_older_logs_defaults_attempt_summaries() {
        let metadata: VisualAssistanceMetadata = serde_json::from_str(
            r#"{
            "images": [{
                "model": "vision/primary",
                "attempts": 1,
                "duration_ms": 42,
                "cache_hit": false
            }]
        }"#,
        )
        .unwrap();

        assert_eq!(metadata.images.len(), 1);
        assert!(metadata.attempts.is_empty());
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
    fn ut_070_successful_routed_request_records_key_id() {
        let s = test_stats();
        let usage = json!({"input_tokens": 10, "output_tokens": 5});
        s.record_entry(RequestEntry::ok_with_key(
            "provider", "model", "http", None, "key-a", &usage,
        ));

        let recent = s.recent(10);
        assert_eq!(recent[0].key_id.as_deref(), Some("key-a"));
    }

    #[test]
    fn ut_070b_empty_per_key_is_still_serialized() {
        // The webview types per_key as a required array; omitting it on a
        // fresh install blanks the whole window (see StatsSummary).
        let json = serde_json::to_value(test_stats().summarize(86_400)).unwrap();
        assert_eq!(json["per_key"], json!([]));
    }

    #[test]
    fn ut_071_zero_token_routed_request_is_recorded() {
        let s = test_stats();
        s.record_entry(RequestEntry::ok_with_key(
            "provider",
            "model",
            "http",
            None,
            "key-a",
            &json!({"input_tokens": 0, "output_tokens": 0}),
        ));

        let sum = s.summarize(86_400);
        assert_eq!(sum.requests, 1);
        assert_eq!(sum.per_key.len(), 1);
        assert_eq!(sum.per_key[0].requests, 1);
        assert_eq!(sum.per_key[0].input_tokens, 0);
    }

    #[test]
    fn ut_072_missing_token_usage_counts_requests_only() {
        let s = test_stats();
        s.record_entry(RequestEntry::ok_with_key(
            "provider",
            "model",
            "http",
            None,
            "key-a",
            &json!({}),
        ));

        let sum = s.summarize(86_400);
        assert_eq!(sum.per_key[0].requests, 1);
        assert_eq!(sum.per_key[0].input_tokens, 0);
        assert_eq!(sum.per_key[0].output_tokens, 0);
    }

    #[test]
    fn ut_073_failed_request_does_not_count_as_success() {
        let s = test_stats();
        s.record_entry(
            RequestEntry::error("provider", "model", "http", None, "401")
                .with_key_id(Some("key-a".to_string())),
        );

        let sum = s.summarize(86_400);
        assert_eq!(sum.per_key[0].requests, 0);
        assert_eq!(sum.per_key[0].errors, 1);
    }

    #[test]
    fn ut_074_failover_attributes_usage_to_the_successful_key() {
        let s = test_stats();
        s.record_entry(
            RequestEntry::error("provider", "model", "http", None, "429")
                .with_key_id(Some("key-a".to_string())),
        );
        let usage = json!({"input_tokens": 10, "output_tokens": 5});
        s.record_entry(RequestEntry::ok_with_key(
            "provider", "model", "http", None, "key-b", &usage,
        ));

        let sum = s.summarize(86_400);
        let key_b = sum.per_key.iter().find(|k| k.key_id == "key-b").unwrap();
        let key_a = sum.per_key.iter().find(|k| k.key_id == "key-a").unwrap();
        assert_eq!(key_b.requests, 1);
        assert_eq!(key_a.requests, 0);
        assert_eq!(key_a.errors, 1);
    }

    #[test]
    fn ut_075_rename_does_not_change_key_id_in_history() {
        let s = test_stats();
        s.record_entry(RequestEntry::ok_with_key(
            "provider",
            "model",
            "http",
            None,
            "stable-id",
            &json!({"input_tokens": 1, "output_tokens": 1}),
        ));

        let sum = s.summarize(86_400);
        assert_eq!(sum.per_key[0].key_id, "stable-id");
    }

    #[test]
    fn ut_084_null_key_rows_are_not_attributed_to_a_key() {
        let s = test_stats();
        s.record(
            "provider",
            "model",
            &json!({"input_tokens": 1, "output_tokens": 1}),
        );
        s.record_entry(RequestEntry::ok_with_key(
            "provider",
            "model",
            "http",
            None,
            "key-a",
            &json!({"input_tokens": 1, "output_tokens": 1}),
        ));

        let sum = s.summarize(86_400);
        assert_eq!(sum.per_provider[0].requests, 2);
        assert_eq!(sum.per_key.len(), 1);
        assert_eq!(sum.per_key[0].key_id, "key-a");
    }

    #[test]
    fn ut_085_deleted_key_history_stays_in_provider_aggregate() {
        let s = test_stats();
        s.record_entry(RequestEntry::ok_with_key(
            "provider",
            "model",
            "http",
            None,
            "deleted-key",
            &json!({"input_tokens": 1, "output_tokens": 1}),
        ));

        let sum = s.summarize(86_400);
        assert_eq!(sum.per_provider[0].requests, 1);
        assert_eq!(sum.per_key.len(), 1);
        assert_eq!(sum.per_key[0].key_id, "deleted-key");
    }

    #[test]
    fn ut_086_legacy_rows_stay_at_provider_level() {
        let s = test_stats();
        s.record(
            "provider",
            "model",
            &json!({"input_tokens": 1, "output_tokens": 1}),
        );

        let sum = s.summarize(86_400);
        assert_eq!(sum.per_provider[0].requests, 1);
        assert!(sum.per_key.is_empty());
    }

    #[test]
    fn prune_enforces_retention_and_row_cap() {
        let s = test_stats();
        let mk = |ts: u64| RequestEntry {
            ts,
            provider: "kimi".into(),
            model: "k3".into(),
            key_id: None,
            transport: "http".into(),
            kind: "request".into(),
            status: "ok".into(),
            error: None,
            latency_ms: None,
            input_tokens: 1,
            output_tokens: 1,
            cached_tokens: 0,
            visual_assistance: None,
            finish_reason: None,
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
    fn problem_rows_surface_kind_in_recent_logs() {
        let s = test_stats();
        s.record_entry(RequestEntry::problem(
            "opencode-go",
            "opencode-go/deepseek-v4-flash",
            "ws",
            Some(420),
            "compaction",
            "loom-router/0.2.7: compaction v2 mismatch",
        ));
        let rows = s.recent(10);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, "compaction");
        assert_eq!(rows[0].status, "error");
        assert!(rows[0]
            .error
            .as_deref()
            .unwrap()
            .contains("compaction v2 mismatch"));
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
