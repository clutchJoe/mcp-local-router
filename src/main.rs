use std::{collections::HashMap, path::PathBuf, sync::Arc, borrow::Cow};

use anyhow::{Context, Result};
use clap::Parser;
use env_logger;
use log::{error, info};
use rmcp::{
    model::{
        CallToolRequestParam, CallToolResult, ErrorData, InitializeResult, ListToolsResult,
        PaginatedRequestParam, Tool,
    },
    service::{serve_server, RequestContext, RoleClient, RunningService, ServiceExt},
    transport::TokioChildProcess,
    RoleServer, ServerHandler,
};
use serde::Deserialize;
use tokio::sync::RwLock;

#[derive(Deserialize, Debug)]
struct AppConfig {
    // Renaming to match how we read it in main:
    #[serde(rename = "mcpServers")]
    mcp_servers: HashMap<String, ServerConfig>,
}

#[derive(Deserialize, Debug)]
struct ServerConfig {
    command: String,
    args: Vec<String>,
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long, value_name = "FILE")]
    config: PathBuf,
}

// ----------------------------------------------------------------
// AggregatorService: store the discovered tools in a HashMap<String, (Tool, String)>
// rather than Cow<'static, str> to avoid lifetime headaches:
#[derive(Clone)]
struct AggregatorService {
    // Key = tool name (Cow<'static, str>),
    // Value = (Tool object, which upstream server it came from)
    clients: Arc<RwLock<HashMap<String, Arc<RunningService<RoleClient, ()>>>>>,
    tools: Arc<RwLock<HashMap<Cow<'static, str>, (Tool, String)>>>,
}

impl AggregatorService {
    async fn new(config: AppConfig) -> Result<Self> {
        let clients = Arc::new(RwLock::new(HashMap::new()));
        let tools = Arc::new(RwLock::new(HashMap::new()));

        // For each "upstream":
        for (name, server_config) in config.mcp_servers {
            info!(
                "Setting up upstream server '{}' with command: {} {:?}",
                name, server_config.command, server_config.args
            );

            let mut cmd = tokio::process::Command::new(&server_config.command);
            cmd.args(&server_config.args);

            let transport = TokioChildProcess::new(&mut cmd).context(format!(
                "Failed to create child process transport for {name}"
            ))?;

            let client_service = ()
                .serve(transport)
                .await
                .context(format!("Failed to create client service for {name}"))?;
            let client_arc = Arc::new(client_service);

            // Insert into "clients" map:
            clients
                .write()
                .await
                .insert(name.clone(), client_arc.clone());

            // Fetch its tools:
            match client_arc.list_tools(None).await {
                Ok(list_result) => {
                    let mut tools_guard = tools.write().await;
                    for tool in list_result.tools {
                        info!("Discovered tool '{}' from upstream '{}'", tool.name, name);
                        tools_guard.insert(Cow::Owned(tool.name.clone().into_owned()), (tool, name.clone()));
                    }
                }
                Err(e) => {
                    error!("Failed to list tools from upstream '{}': {}", name, e);
                }
            }
        }

        Ok(Self { clients, tools })
    }
}

// ----------------------------------------------------------------
// Implement the trait using the async_trait macro to match
// the signature in rmcp::ServerHandler:
impl ServerHandler for AggregatorService {
    fn get_info(&self) -> InitializeResult {
        InitializeResult::default()
    }

    async fn list_tools(
        &self,
        _params: PaginatedRequestParam,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools_guard = self.tools.read().await;
        let tools_list: Vec<Tool> = tools_guard.values().map(|(tool, _)| tool.clone()).collect();
        Ok(ListToolsResult {
            tools: tools_list,
            next_cursor: None,
        })
    }

    async fn call_tool(
        &self,
        params: CallToolRequestParam,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let tool_name = params.name;
        let arguments = params.arguments;
        let clients_guard = self.clients.read().await;
        let tools_guard = self.tools.read().await;
        if let Some((_tool, client_name)) = tools_guard.get(&*tool_name) {
            if let Some(client) = clients_guard.get(client_name) {
                info!(
                    "Routing call to tool '{}' -> upstream '{}'",
                    tool_name, client_name
                );
                let client_params = CallToolRequestParam {
                    name: tool_name.clone(),
                    arguments,
                };
                match client.call_tool(client_params).await {
                    Ok(result) => Ok(result),
                    Err(e) => {
                        error!(
                            "Error calling tool '{}' on upstream '{}': {}",
                            tool_name, client_name, e
                        );
                        Err(ErrorData::internal_error(format!("{}", e), None))
                    }
                }
            } else {
                error!(
                    "Client '{}' associated with tool '{}' not found.",
                    client_name, tool_name
                );
                Err(ErrorData::internal_error(
                    format!("Client '{}' not found", client_name),
                    None,
                ))
            }
        } else {
            error!("Tool '{}' not found.", tool_name);
            Err(ErrorData::resource_not_found(
                format!("Tool '{}' not found", tool_name),
                None,
            ))
        }
    }
}

// ----------------------------------------------------------------
// Main function:
#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    let args = Args::parse();

    info!("Loading configuration from: {:?}", args.config);
    let config_content = std::fs::read_to_string(&args.config)
        .with_context(|| format!("Failed to read config file: {:?}", args.config))?;

    let parsed: AppConfig =
        serde_json::from_str(&config_content).context("Config is not valid JSON")?;
    let aggregator_service = AggregatorService::new(parsed).await?;

    info!("Starting MCP local router server...");

    // For demonstration, we serve over stdio, but you could do something else:
    use tokio::io::{stdin, stdout};
    let transport = (stdin(), stdout());
    serve_server(aggregator_service, transport).await?;

    info!("Server stopped.");
    Ok(())
}
