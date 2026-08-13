use crate::config::AppConfig;
use futures::{stream, StreamExt};
use rmcp::{
    handler::server::{tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router, ServerHandler, ServiceExt,
};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, process::Stdio};

const MAX_TASKS: usize = 8;
const MAX_CONCURRENCY: usize = 4;
const MAX_DEPTH: u8 = 3;
const DEPTH_ENV: &str = "LOOM_ROUTER_SUBAGENT_DEPTH";

fn model_is_enabled(config: &AppConfig, slug: &str) -> bool {
    config.providers.iter().any(|(provider_id, provider)| {
        provider.enabled
            && provider.models.iter().any(|model| {
                model.enabled
                    && super::published_slug(provider_id, &model.id, config.native_slug_mode)
                        == slug
            })
    })
}

fn enabled_model_slugs(config: &AppConfig) -> Vec<String> {
    config
        .providers
        .iter()
        .filter(|(_, provider)| provider.enabled)
        .flat_map(|(provider_id, provider)| {
            provider
                .models
                .iter()
                .filter(|model| model.enabled)
                .map(|model| super::published_slug(provider_id, &model.id, config.native_slug_mode))
        })
        .collect()
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SpawnTask {
    /// Stable short name used to identify the result.
    name: String,
    /// Any enabled LoomRouter model slug, exactly as shown in the Codex picker.
    model: String,
    /// Complete, self-contained instructions for this worker.
    prompt: String,
    /// Codex sandbox mode. Defaults to workspace-write.
    sandbox: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SpawnRequest {
    /// Independent tasks to execute in parallel, up to eight per call.
    tasks: Vec<SpawnTask>,
    /// Workspace root for every task. Defaults to the MCP process directory.
    cwd: Option<String>,
}

#[derive(Debug, Serialize)]
struct SpawnResult {
    name: String,
    model: String,
    status: &'static str,
    output: String,
}

#[derive(Clone)]
struct SubagentServer {
    #[expect(dead_code, reason = "tool_handler macro accesses this router field")]
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl SubagentServer {
    #[tool(
        name = "loom_list_subagent_models",
        description = "List every provider/model currently enabled in LoomRouter and accepted by loom_spawn_agents."
    )]
    async fn list_models(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        let text = serde_json::to_string_pretty(&enabled_model_slugs(&AppConfig::load())).map_err(
            |error| {
                rmcp::ErrorData::internal_error(
                    format!("serializing enabled models failed: {error}"),
                    None,
                )
            },
        )?;
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }
    #[tool(
        name = "loom_spawn_agents",
        description = "Run independent Codex subagents with any provider/model currently enabled in LoomRouter. Use this when native spawn_agent rejects a model slug. Results include an explicit completed or failed status for every worker."
    )]
    async fn spawn_agents(
        &self,
        Parameters(request): Parameters<SpawnRequest>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        if request.tasks.is_empty() || request.tasks.len() > MAX_TASKS {
            return Err(rmcp::ErrorData::invalid_params(
                format!("tasks must contain between 1 and {MAX_TASKS} items"),
                None,
            ));
        }
        let depth = current_depth();
        if depth >= MAX_DEPTH {
            return Err(rmcp::ErrorData::invalid_params(
                format!("subagent depth limit reached ({MAX_DEPTH})"),
                None,
            ));
        }
        let config = AppConfig::load();
        for task in &request.tasks {
            if !model_is_enabled(&config, &task.model) {
                return Err(rmcp::ErrorData::invalid_params(
                    format!("model '{}' is not enabled in LoomRouter", task.model),
                    None,
                ));
            }
            validate_sandbox(task.sandbox.as_deref())?;
        }

        let cwd = request
            .cwd
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .ok_or_else(|| rmcp::ErrorData::invalid_params("cwd is unavailable", None))?;
        // Structured concurrency matters here: dropping a cancelled MCP call
        // must drop the in-flight Command futures instead of detaching workers.
        let results: Vec<SpawnResult> = stream::iter(request.tasks)
            .map(|task| run_task(task, cwd.clone(), depth + 1))
            .buffered(MAX_CONCURRENCY)
            .collect()
            .await;
        let text = serde_json::to_string_pretty(&results).map_err(|error| {
            rmcp::ErrorData::internal_error(format!("serializing results failed: {error}"), None)
        })?;
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }
}

#[tool_handler]
impl ServerHandler for SubagentServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Use loom_spawn_agents for enabled LoomRouter models that the native spawn_agent model enum cannot represent. Report each returned worker status in the parent chat.")
    }
}

fn current_depth() -> u8 {
    std::env::var(DEPTH_ENV)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

fn validate_sandbox(sandbox: Option<&str>) -> Result<(), rmcp::ErrorData> {
    match sandbox.unwrap_or("workspace-write") {
        "read-only" | "workspace-write" | "danger-full-access" => Ok(()),
        value => Err(rmcp::ErrorData::invalid_params(
            format!("unsupported sandbox '{value}'"),
            None,
        )),
    }
}

async fn run_task(task: SpawnTask, cwd: PathBuf, depth: u8) -> SpawnResult {
    let name = task.name;
    let model = task.model;
    let sandbox = task.sandbox.unwrap_or_else(|| "workspace-write".into());
    let Some(binary) = super::codex_bin() else {
        return SpawnResult {
            name,
            model,
            status: "failed",
            output: "Codex CLI not found".into(),
        };
    };
    let mut command = tokio::process::Command::new(binary);
    command.args(codex_task_args(&model, &sandbox, &cwd, &task.prompt));
    command
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .env(DEPTH_ENV, depth.to_string());
    crate::cli_locator::hide_console_window(command.as_std_mut());
    let output = command.output().await;

    match output {
        Ok(output) => SpawnResult {
            name,
            model,
            status: if output.status.success() {
                "completed"
            } else {
                "failed"
            },
            output: extract_final_message(&output.stdout).unwrap_or_else(|| {
                String::from_utf8_lossy(if output.stderr.is_empty() {
                    &output.stdout
                } else {
                    &output.stderr
                })
                .trim()
                .to_string()
            }),
        },
        Err(error) => SpawnResult {
            name,
            model,
            status: "failed",
            output: error.to_string(),
        },
    }
}

fn codex_task_args(model: &str, sandbox: &str, cwd: &std::path::Path, prompt: &str) -> Vec<String> {
    vec![
        "--ask-for-approval".into(),
        "never".into(),
        "--sandbox".into(),
        sandbox.into(),
        "--model".into(),
        model.into(),
        "--cd".into(),
        cwd.display().to_string(),
        "exec".into(),
        "--json".into(),
        "--ephemeral".into(),
        prompt.into(),
    ]
}

fn extract_final_message(stdout: &[u8]) -> Option<String> {
    String::from_utf8_lossy(stdout)
        .lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|event| {
            (event.get("type")?.as_str()? == "item.completed")
                .then(|| event.get("item").cloned())
                .flatten()
        })
        .filter(|item| item.get("type").and_then(|v| v.as_str()) == Some("agent_message"))
        .filter_map(|item| {
            item.get("text")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .next()
}

pub async fn serve_subagent_mcp() -> anyhow::Result<()> {
    let service = SubagentServer::new()
        .serve(rmcp::transport::stdio())
        .await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Provider, ProviderModel, ProviderProtocol};
    use rmcp::ServiceExt;

    fn config_with_models() -> AppConfig {
        let mut config = AppConfig::default();
        config.providers.insert(
            "opencode-go".into(),
            Provider {
                id: "opencode-go".into(),
                name: "OpenCode Go".into(),
                protocol: ProviderProtocol::Responses,
                base_url: "https://example.test/v1".into(),
                api_key: None,
                keys: Vec::new(),
                rotation_enabled: false,
                has_key: true,
                context_window: None,
                user_agent: None,
                models: vec![
                    ProviderModel {
                        id: "deepseek-v4-flash".into(),
                        label: None,
                        context_window: Some(1_000_000),
                        protocol: Some(ProviderProtocol::Responses),
                        fast_mode: false,
                        enabled: true,
                        supports_vision: false,
                    },
                    ProviderModel {
                        id: "disabled-model".into(),
                        label: None,
                        context_window: None,
                        protocol: None,
                        fast_mode: false,
                        enabled: false,
                        supports_vision: false,
                    },
                ],
                enabled: true,
            },
        );
        config
    }

    #[test]
    fn only_enabled_provider_models_can_spawn() {
        let config = config_with_models();
        assert!(model_is_enabled(&config, "opencode-go/deepseek-v4-flash"));
        assert!(!model_is_enabled(&config, "opencode-go/disabled-model"));
        assert!(!model_is_enabled(&config, "missing/model"));
    }

    #[test]
    fn enabled_model_list_tracks_provider_and_native_slug_mode() {
        let mut config = config_with_models();
        assert_eq!(
            enabled_model_slugs(&config),
            vec!["opencode-go/deepseek-v4-flash"]
        );

        config.native_slug_mode = true;
        assert_eq!(enabled_model_slugs(&config), vec!["deepseek-v4-flash"]);

        config.providers.get_mut("opencode-go").unwrap().enabled = false;
        assert!(enabled_model_slugs(&config).is_empty());
    }

    #[test]
    fn final_agent_message_is_extracted_from_codex_jsonl() {
        let output = br#"{"type":"thread.started","thread_id":"x"}
{"type":"item.completed","item":{"id":"1","type":"agent_message","text":"done"}}
"#;
        assert_eq!(extract_final_message(output).as_deref(), Some("done"));
    }

    #[test]
    fn codex_global_options_precede_exec_subcommand() {
        let args = codex_task_args(
            "opencode-go/deepseek-v4-flash",
            "read-only",
            std::path::Path::new("/tmp/work"),
            "Review",
        );
        let exec = args.iter().position(|arg| arg == "exec").unwrap();
        let approval = args
            .iter()
            .position(|arg| arg == "--ask-for-approval")
            .unwrap();
        assert!(approval < exec);
        assert_eq!(&args[exec..], ["exec", "--json", "--ephemeral", "Review"]);
    }

    #[tokio::test]
    async fn mcp_handshake_advertises_model_list_and_spawn_tools() {
        let (server_transport, client_transport) = tokio::io::duplex(16 * 1024);
        let server = tokio::spawn(async move {
            SubagentServer::new()
                .serve(server_transport)
                .await?
                .waiting()
                .await?;
            anyhow::Ok(())
        });
        let client = ().serve(client_transport).await.unwrap();

        let tools = client.list_tools(None).await.unwrap();
        let names: Vec<&str> = tools.tools.iter().map(|tool| tool.name.as_ref()).collect();
        assert!(names.contains(&"loom_list_subagent_models"));
        assert!(names.contains(&"loom_spawn_agents"));

        client.cancel().await.unwrap();
        server.await.unwrap().unwrap();
    }
}
