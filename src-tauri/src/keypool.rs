//! In-memory health and selection for multiple provider API keys.
//!
//! Health is intentionally not persisted: a restart clears every cooldown,
//! which is acceptable for a local desktop router and avoids persisting
//! secret-adjacent runtime state.

use crate::config::{Provider, ProviderKey};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    Auth,
    Transient,
}

#[derive(Debug, Clone)]
pub enum KeyHealth {
    CoolingDown { retry_at: Instant },
}

/// How long an auth failure sidelines a key. Long enough that a genuinely
/// revoked key stops burning requests, finite so it can heal on its own.
const AUTH_COOLDOWN: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Default)]
struct ProviderKeyPool {
    health: HashMap<String, KeyHealth>,
    failures: HashMap<String, u32>,
    rotation_cursor: usize,
}

#[derive(Debug, Clone, Default)]
pub struct KeyPools {
    inner: Arc<RwLock<HashMap<String, ProviderKeyPool>>>,
}

impl KeyPools {
    pub fn new() -> Self {
        Self::default()
    }

    /// Enabled healthy keys in attempt order. With rotation off the first
    /// eligible key is primary; with rotation on the cursor picks the first
    /// key and moves to the next eligible key for the following request.
    pub async fn eligible_keys(&self, provider: &Provider, rotation: bool) -> Vec<ProviderKey> {
        let mut pools = self.inner.write().await;
        let pool = pools.entry(provider.id.clone()).or_default();
        let now = Instant::now();
        let mut eligible: Vec<ProviderKey> = provider
            .keys
            .iter()
            .filter(|key| key.enabled && pool.is_eligible(key, now))
            .cloned()
            .collect();
        if eligible.is_empty() {
            return eligible;
        }
        if rotation {
            let cursor = pool.rotation_cursor.min(eligible.len() - 1);
            eligible.rotate_left(cursor);
            pool.rotation_cursor = (cursor + 1) % eligible.len();
        }
        eligible
    }

    pub async fn record_success(&self, provider_id: &str, key_id: &str) {
        let mut pools = self.inner.write().await;
        if let Some(pool) = pools.get_mut(provider_id) {
            pool.health.remove(key_id);
            pool.failures.remove(key_id);
        }
    }

    pub async fn record_failure(
        &self,
        provider_id: &str,
        key_id: &str,
        failure: FailureKind,
        retry_after: Option<u64>,
    ) {
        let mut pools = self.inner.write().await;
        let pool = pools.entry(provider_id.to_string()).or_default();
        let failures = pool.failures.entry(key_id.to_string()).or_insert(0);
        match failure {
            FailureKind::Auth => {
                // Cool down instead of parking the key for good: a key that is
                // never selected again can never record the success that would
                // clear the flag, so one 401 from a flaky gateway used to kill
                // it until the app restarted, leaving the user with nothing but
                // "provider has no enabled API key". The window is its own
                // ladder — auth failures never feed the transient backoff.
                pool.health.insert(
                    key_id.to_string(),
                    KeyHealth::CoolingDown {
                        retry_at: Instant::now() + AUTH_COOLDOWN,
                    },
                );
                *failures = 0;
            }
            FailureKind::Transient => {
                *failures += 1;
                let seconds = retry_after
                    .filter(|seconds| (1..=3600).contains(seconds))
                    .unwrap_or(match *failures {
                        1 => 60,
                        2 => 300,
                        _ => 1500,
                    });
                pool.health.insert(
                    key_id.to_string(),
                    KeyHealth::CoolingDown {
                        retry_at: Instant::now() + Duration::from_secs(seconds),
                    },
                );
            }
        }
    }

    pub async fn prune_provider(&self, provider_id: &str, keys: &[ProviderKey]) {
        let mut pools = self.inner.write().await;
        let Some(pool) = pools.get_mut(provider_id) else {
            return;
        };
        let ids: HashSet<String> = keys.iter().map(|key| key.id.clone()).collect();
        pool.health.retain(|id, _| ids.contains(id));
        pool.failures.retain(|id, _| ids.contains(id));
        // The cursor indexes into the eligible list, whose shape just changed;
        // restarting at the head costs at most one repeated key and is cheaper
        // than tracking where every surviving key moved to.
        pool.rotation_cursor = 0;
        if keys.is_empty() {
            pools.remove(provider_id);
        }
    }
}

impl ProviderKeyPool {
    fn is_eligible(&self, key: &ProviderKey, now: Instant) -> bool {
        match self.health.get(&key.id) {
            None => true,
            Some(KeyHealth::CoolingDown { retry_at }) => *retry_at <= now,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(id: &str, enabled: bool) -> ProviderKey {
        ProviderKey {
            id: id.to_string(),
            name: id.to_string(),
            enabled,
            api_key: Some("secret".to_string()),
            has_key: true,
        }
    }

    fn provider(keys: Vec<ProviderKey>) -> Provider {
        Provider {
            id: "provider".to_string(),
            name: "Provider".to_string(),
            protocol: crate::config::ProviderProtocol::OpenAI,
            base_url: "https://example.test/v1".to_string(),
            api_key: None,
            keys,
            rotation_enabled: false,
            has_key: true,
            context_window: None,
            user_agent: None,
            prompt_cache: None,
            models: Vec::new(),
            enabled: true,
        }
    }

    #[tokio::test]
    async fn ut_010_rotation_off_returns_first_enabled_healthy_key() {
        let pools = KeyPools::new();
        let p = provider(vec![key("a", true), key("b", true)]);

        let eligible = pools.eligible_keys(&p, false).await;

        assert_eq!(
            eligible.iter().map(|k| k.id.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
    }

    #[tokio::test]
    async fn ut_011_disabled_keys_are_excluded() {
        let pools = KeyPools::new();
        let p = provider(vec![key("a", false), key("b", true)]);

        let eligible = pools.eligible_keys(&p, false).await;

        assert_eq!(
            eligible.iter().map(|k| k.id.as_str()).collect::<Vec<_>>(),
            ["b"]
        );
    }

    #[tokio::test]
    async fn ut_012_no_enabled_healthy_key_returns_empty() {
        let pools = KeyPools::new();
        pools
            .record_failure("provider", "a", FailureKind::Auth, None)
            .await;
        let p = provider(vec![key("a", true)]);

        assert!(pools.eligible_keys(&p, false).await.is_empty());
    }

    #[tokio::test]
    async fn ut_014_transient_failure_cools_down_and_skips_the_key() {
        let pools = KeyPools::new();
        pools
            .record_failure("provider", "a", FailureKind::Transient, None)
            .await;
        let p = provider(vec![key("a", true), key("b", true)]);

        let eligible = pools.eligible_keys(&p, false).await;

        assert_eq!(
            eligible.iter().map(|k| k.id.as_str()).collect::<Vec<_>>(),
            ["b"]
        );
    }

    #[tokio::test]
    async fn ut_015_success_resets_health() {
        let pools = KeyPools::new();
        pools
            .record_failure("provider", "a", FailureKind::Auth, None)
            .await;
        pools.record_success("provider", "a").await;
        let p = provider(vec![key("a", true)]);

        assert_eq!(pools.eligible_keys(&p, false).await.len(), 1);
    }

    #[tokio::test]
    async fn ut_016_auth_failure_marks_key_for_attention() {
        let pools = KeyPools::new();
        pools
            .record_failure("provider", "a", FailureKind::Auth, None)
            .await;
        let p = provider(vec![key("a", true), key("b", true)]);

        assert_eq!(pools.eligible_keys(&p, false).await.len(), 1);
    }

    #[tokio::test]
    async fn ut_031_rotation_round_robins_in_list_order() {
        let pools = KeyPools::new();
        let p = provider(vec![key("a", true), key("b", true)]);

        let first = pools.eligible_keys(&p, true).await;
        let second = pools.eligible_keys(&p, true).await;

        assert_eq!(first[0].id, "a");
        assert_eq!(second[0].id, "b");
    }

    #[tokio::test]
    async fn ut_034_rotation_wraps_around() {
        let pools = KeyPools::new();
        let p = provider(vec![key("a", true), key("b", true)]);
        pools.eligible_keys(&p, true).await;
        pools.eligible_keys(&p, true).await;

        let third = pools.eligible_keys(&p, true).await;

        assert_eq!(third[0].id, "a");
    }

    #[tokio::test]
    async fn ut_048_retry_after_is_capped() {
        let pools = KeyPools::new();
        pools
            .record_failure("provider", "a", FailureKind::Transient, Some(7200))
            .await;
        let p = provider(vec![key("a", true), key("b", true)]);

        let eligible = pools.eligible_keys(&p, false).await;
        assert_eq!(eligible[0].id, "b");
    }

    #[tokio::test]
    async fn ut_051_pruning_removes_deleted_key_health() {
        let pools = KeyPools::new();
        pools
            .record_failure("provider", "a", FailureKind::Auth, None)
            .await;
        pools.prune_provider("provider", &[key("b", true)]).await;
        let p = provider(vec![key("b", true)]);

        assert_eq!(pools.eligible_keys(&p, false).await.len(), 1);
    }

    #[tokio::test]
    async fn ut_013_changing_primary_between_requests_changes_the_first_key() {
        let pools = KeyPools::new();
        let mut p = provider(vec![key("a", true), key("b", true)]);
        assert_eq!(pools.eligible_keys(&p, false).await[0].id, "a");
        p.keys.reverse();

        assert_eq!(pools.eligible_keys(&p, false).await[0].id, "b");
    }

    #[tokio::test]
    async fn ut_025_request_captures_an_eligible_list_snapshot() {
        let pools = KeyPools::new();
        let p = provider(vec![key("a", true), key("b", true)]);

        let snapshot = pools.eligible_keys(&p, false).await;
        let mut changed = p;
        changed.keys.retain(|key| key.id == "b");

        assert_eq!(snapshot.len(), 2);
        assert_eq!(changed.keys.len(), 1);
    }

    #[tokio::test]
    async fn ut_026_mid_session_primary_change_affects_the_next_request_only() {
        let pools = KeyPools::new();
        let mut p = provider(vec![key("a", true), key("b", true)]);
        let first = pools.eligible_keys(&p, false).await;
        p.keys.reverse();
        let second = pools.eligible_keys(&p, false).await;

        assert_eq!(first[0].id, "a");
        assert_eq!(second[0].id, "b");
    }

    #[tokio::test]
    async fn ut_032_rotation_with_one_healthy_key_uses_it() {
        let pools = KeyPools::new();
        let p = provider(vec![key("a", true)]);

        assert_eq!(pools.eligible_keys(&p, true).await[0].id, "a");
    }

    #[tokio::test]
    async fn ut_033_rotation_with_all_keys_disabled_is_empty() {
        let pools = KeyPools::new();
        let p = provider(vec![key("a", false), key("b", false)]);

        assert!(pools.eligible_keys(&p, true).await.is_empty());
    }

    #[tokio::test]
    async fn ut_035_new_key_participates_in_rotation() {
        let pools = KeyPools::new();
        let mut p = provider(vec![key("a", true), key("b", true)]);
        pools.eligible_keys(&p, true).await;
        p.keys.push(key("c", true));

        let next = pools.eligible_keys(&p, true).await;
        assert_eq!(next.len(), 3);
    }

    #[tokio::test]
    async fn ut_036_deleted_key_leaves_rotation_without_a_gap() {
        let pools = KeyPools::new();
        let mut p = provider(vec![key("a", true), key("b", true), key("c", true)]);
        pools.eligible_keys(&p, true).await;
        p.keys.remove(0);
        pools
            .prune_provider("provider", &[key("b", true), key("c", true)])
            .await;

        let next = pools.eligible_keys(&p, true).await;
        assert_eq!(next[0].id, "b");
    }

    #[tokio::test]
    async fn ut_039_rotation_position_is_captured_per_request() {
        let pools = KeyPools::new();
        let p = provider(vec![key("a", true), key("b", true)]);

        let first = pools.eligible_keys(&p, true).await;
        let mut changed = p;
        changed.keys.reverse();
        let second = pools.eligible_keys(&changed, true).await;

        assert_eq!(first[0].id, "a");
        assert_eq!(second[0].id, "a");
    }

    #[tokio::test]
    async fn ut_044_concurrent_requests_do_not_deadlock() {
        let pools = Arc::new(KeyPools::new());
        let p = Arc::new(provider(vec![key("a", true), key("b", true)]));

        let (left, right) = tokio::join!(
            pools.eligible_keys(&p, false),
            pools.eligible_keys(&p, false),
        );

        assert_eq!(left.len(), 2);
        assert_eq!(right.len(), 2);
    }

    #[tokio::test]
    async fn ut_045_concurrent_rotation_gets_distinct_cursors() {
        let pools = Arc::new(KeyPools::new());
        let p = Arc::new(provider(vec![key("a", true), key("b", true)]));

        let (left, right) =
            tokio::join!(pools.eligible_keys(&p, true), pools.eligible_keys(&p, true),);

        assert_ne!(left[0].id, right[0].id);
    }

    #[tokio::test]
    async fn ut_046_key_deleted_before_request_is_not_selected() {
        let pools = KeyPools::new();
        let mut p = provider(vec![key("a", true), key("b", true)]);
        p.keys.remove(0);
        pools.prune_provider("provider", &[key("b", true)]).await;

        let eligible = pools.eligible_keys(&p, false).await;
        assert_eq!(eligible[0].id, "b");
    }

    #[tokio::test]
    async fn ut_047_deleting_last_key_while_request_is_in_flight_does_not_crash() {
        let pools = KeyPools::new();
        let mut p = provider(vec![key("a", true)]);

        let snapshot = pools.eligible_keys(&p, false).await;
        p.keys.clear();
        pools.prune_provider("provider", &[]).await;

        assert_eq!(snapshot.len(), 1);
        assert!(pools.eligible_keys(&p, false).await.is_empty());
    }

    #[tokio::test]
    async fn ut_049_repeated_transient_failures_increment_backoff() {
        let pools = KeyPools::new();
        pools
            .record_failure("provider", "a", FailureKind::Transient, None)
            .await;
        pools
            .record_failure("provider", "a", FailureKind::Transient, None)
            .await;
        pools
            .record_failure("provider", "a", FailureKind::Transient, None)
            .await;

        let inner = pools.inner.read().await;
        assert_eq!(inner["provider"].failures["a"], 3);
    }

    #[tokio::test]
    async fn ut_052_auth_cooldown_expires_and_the_key_is_eligible_again() {
        let pools = KeyPools::new();
        let p = provider(vec![key("a", true)]);
        pools
            .record_failure("provider", "a", FailureKind::Auth, None)
            .await;

        assert!(pools.eligible_keys(&p, false).await.is_empty());

        // Rewind the cooldown rather than sleeping out the real window.
        {
            let mut inner = pools.inner.write().await;
            let KeyHealth::CoolingDown { retry_at } = inner
                .get_mut("provider")
                .unwrap()
                .health
                .get_mut("a")
                .unwrap();
            *retry_at = Instant::now() - Duration::from_secs(1);
        }

        assert_eq!(pools.eligible_keys(&p, false).await[0].id, "a");
    }

    #[tokio::test]
    async fn ut_050_cooldown_expires_and_the_key_is_eligible_again() {
        let pools = KeyPools::new();
        pools
            .record_failure("provider", "a", FailureKind::Transient, Some(1))
            .await;
        tokio::time::sleep(Duration::from_millis(1100)).await;

        let p = provider(vec![key("a", true)]);
        assert_eq!(pools.eligible_keys(&p, false).await[0].id, "a");
    }
}
