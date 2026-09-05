// Link release builds against the Windows GUI subsystem. Without this the
// binary is a console app, so Windows allocates a console beside the window
// and the tracing output lands in it. Debug builds keep the console: that is
// where `tauri dev` prints the log.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if std::env::args().nth(1).as_deref() == Some("provider-auth") {
        loom_router_lib::codex::print_provider_auth_token();
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("subagent-mcp") {
        let runtime = tokio::runtime::Runtime::new().expect("subagent MCP runtime");
        if let Err(error) = runtime.block_on(loom_router_lib::codex::serve_subagent_mcp()) {
            eprintln!("LoomRouter subagent MCP failed: {error}");
            std::process::exit(1);
        }
        return;
    }
    loom_router_lib::run()
}
