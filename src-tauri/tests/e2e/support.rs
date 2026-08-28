use axum::Router;
use loom_router_lib::config::{AppConfig, Provider, ProviderModel, ProviderProtocol};
use loom_router_lib::proxy;
use loom_router_lib::stats::Stats;
use std::sync::Arc;
use tokio::sync::RwLock;

pub(super) const UPSTREAM_SSE: &str = concat!(
    "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":2,\"total_tokens\":6}}\n\n",
    "data: [DONE]\n\n",
);

pub(super) async fn spawn(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

pub(super) fn provider(
    id: &str,
    name: &str,
    protocol: ProviderProtocol,
    base_url: String,
    api_key: &str,
    model_id: &str,
    model_context_window: Option<u32>,
) -> Provider {
    Provider {
        id: id.into(),
        name: name.into(),
        protocol,
        base_url,
        api_key: Some(api_key.into()),
        keys: vec![],
        rotation_enabled: false,
        has_key: true,
        context_window: None,
        user_agent: None,
        prompt_cache: None,
        models: vec![ProviderModel {
            id: model_id.into(),
            label: None,
            context_window: model_context_window,
            protocol: None,
            fast_mode: false,
            enabled: true,
            supports_vision: false,
        }],
        enabled: true,
    }
}

pub(super) async fn spawn_proxy(config: AppConfig) -> String {
    spawn(proxy::router(
        Arc::new(RwLock::new(config)),
        Arc::new(RwLock::new(Stats::in_memory())),
    ))
    .await
}

pub(super) async fn spawn_proxy_with_stats(config: AppConfig, stats: Arc<RwLock<Stats>>) -> String {
    spawn(proxy::router(Arc::new(RwLock::new(config)), stats)).await
}

pub(super) fn ws_url(proxy_url: &str) -> String {
    format!(
        "{}/v1/responses?token={}",
        proxy_url.replacen("http", "ws", 1),
        proxy::local_token()
    )
}
