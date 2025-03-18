//! A single-file example of an MCP "proxy server" that aggregates multiple upstream MCP Servers
//! as MCP clients, and itself exposes an MCP Server interface to downstream clients via stdin/stdout.
//!
//! Usage:
//! 1) Make sure you have the `mcp-client`, `mcp-core`, and `mcp-server` crates (or their code) available.
//! 2) Adjust the upstream server endpoints in `ProxyRouter::new(...)` as desired.
//! 3) `cargo run --bin proxy_server` (or however you choose to integrate into your project).
//!
//! This example connects (as a client) to multiple upstream MCP Servers (each via SSE, stdio, etc.),
//! merges or proxies their capabilities, and then starts its own MCP Server on stdin/stdout.
//! Downstream clients can connect to this process and see a union of the upstream servers' tools/resources/prompts.

use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::Arc,
    path::PathBuf,
    fs,
};

use clap::Parser;
use serde::{Deserialize, Serialize};
use mcp_client::{
    client::{ClientCapabilities, ClientInfo, McpClient, McpClientTrait},
    service::McpService,
    transport::{SseTransport, StdioTransport, Transport},
};
use mcp_spec::{
    content::Content,
    handler::{PromptError, ResourceError, ToolError},
    prompt::Prompt,
    protocol::{
        PromptsCapability,
        ResourcesCapability,
        ServerCapabilities,
        ToolsCapability,
    },
    resource::Resource,
    tool::Tool,
};
use mcp_server::{
    router::{Router, RouterService},
    ByteTransport, Server,
};
use serde_json::Value;
use tokio::io::{stdin, stdout};
use tracing::{info, debug, warn, error};
use tracing_subscriber::EnvFilter;
use tracing_appender::rolling::{RollingFileAppender, Rotation};

/// Command line arguments
#[derive(Parser, Debug)]
#[clap(author, version, about)]
struct Args {
    /// Path to the MCP server configuration file
    #[clap(short, long, required = true, value_parser = clap::value_parser!(PathBuf))]
    config: PathBuf,
}

/// MCP Server configuration file structure
#[derive(Debug, Serialize, Deserialize)]
struct Config {
    /// Map of server name to server configuration
    #[serde(default)]
    mcpServers: HashMap<String, ServerConfig>,
}

/// Configuration for a specific MCP server
#[derive(Debug, Serialize, Deserialize)]
struct ServerConfig {
    /// Command to execute
    command: String,
    /// Command arguments
    #[serde(default)]
    args: Vec<String>,
    /// Environment variables to pass to the command
    #[serde(default)]
    env: HashMap<String, String>,
}

/// A struct that holds multiple MCP clients (each connected to an upstream MCP server),
/// and implements the MCP Server `Router` trait by aggregating or proxying calls to them.
#[derive(Clone)]
struct ProxyRouter {
    /// Each upstream server is represented by a fully-initialized `McpClient`.
    /// For simplicity, we keep them all in a vector.
    upstream_clients: Arc<Vec<Box<dyn McpClientTrait>>>,

    /// Aggregated or custom name for this "proxy" server
    name: String,

    /// Merged or custom instructions for how to use this server
    instructions: String,

    /// An aggregated set of capabilities (union) from all upstream servers.
    capabilities: ServerCapabilities,

    /// Shared runtime for blocking calls
    rt: Arc<tokio::runtime::Runtime>,

    /// Cached tools from all upstream clients
    cached_tools: Arc<Vec<Tool>>,
}

impl ProxyRouter {
    /// Creates a new ProxyRouter, connecting to multiple upstream servers.
    ///
    /// In this example, we connect to two SSE endpoints and one stdio process,
    /// just to illustrate how you might combine them. Adjust as you like.
    pub async fn new(config_path: Option<PathBuf>) -> anyhow::Result<Self> {
        info!("Creating new ProxyRouter...");
        // 创建共享的 Runtime
        let rt = Arc::new(tokio::runtime::Runtime::new().unwrap());

        // 记录客户端列表
        let mut upstream_clients: Vec<Box<dyn McpClientTrait>> = Vec::new();
        let mut cached_tools = Vec::new();
        
        // 从配置文件加载 transports
        let path = config_path.ok_or_else(|| anyhow::anyhow!("Config file path is required"))?;
        info!("Loading server configuration from: {:?}", path);
        let contents = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(e) => {
                error!("Failed to read config file '{}': {}", path.display(), e);
                std::process::exit(1);
            }
        };
        
        let config: Config = match serde_json::from_str(&contents) {
            Ok(config) => config,
            Err(e) => {
                error!("Failed to parse JSON config file '{}': {}", path.display(), e);
                error!("Please ensure the file is valid JSON format with the structure as shown in mcp.example.json");
                std::process::exit(1);
            }
        };
        
        if config.mcpServers.is_empty() {
            error!("No MCP servers defined in config file '{}'", path.display());
            std::process::exit(1);
        }
        
        // 从配置中创建 transports
        for (server_name, server_config) in config.mcpServers.iter() {
            info!("Creating transport for server: {}", server_name);
            
            // 创建 StdioTransport
            let transport = StdioTransport::new(
                server_config.command.clone(),
                server_config.args.clone(),
                server_config.env.clone(),
            );
            debug!("Created StdioTransport for server {}: {} {:?}", 
                  server_name, server_config.command, server_config.args);
            
            // 启动 transport
            let handle = match transport.start().await {
                Ok(handle) => {
                    info!("Successfully started StdioTransport for server: {}", server_name);
                    handle
                },
                Err(e) => {
                    error!("Failed to start StdioTransport for server {}: {}", server_name, e);
                    continue;
                }
            };
            
            // 创建 McpService
            let service = McpService::with_timeout(handle, std::time::Duration::from_secs(30));
            
            // 创建 McpClient
            let mut client = McpClient::new(service);
            
            // 初始化连接
            let client_info = ClientInfo { 
                name: "ProxyUpstream".into(), 
                version: "1.0.0".into() 
            };
            
            match client.initialize(
                client_info,
                ClientCapabilities::default(),
            ).await {
                Ok(init) => {
                    info!("Successfully initialized client for server: {}", server_name);
                    debug!("Client {} capabilities: {:?}", server_name, init.capabilities);
                    
                    // 添加到客户端列表
                    upstream_clients.push(Box::new(client) as Box<dyn McpClientTrait>);
                },
                Err(e) => {
                    error!("Failed to initialize client for server {}: {}", server_name, e);
                    continue;
                }
            };
        }
        
        // 如果没有成功连接的客户端，返回错误
        if upstream_clients.is_empty() {
            return Err(anyhow::anyhow!("Failed to connect to any MCP servers"));
        }

        // 3) 聚合 server capabilities
        debug!("Merging server capabilities...");
        
        // 获取每个客户端的 capabilities
        let mut all_caps = Vec::new();
        for (idx, client) in upstream_clients.iter().enumerate() {
            // 尝试获取每个客户端的 capabilities
            if let Ok(info) = client.list_tools(None).await {
                if !info.tools.is_empty() {
                    debug!("Client {}: found {} tools", idx, info.tools.len());
                    // 如果客户端有工具，我们假设它支持 tools capability
                    all_caps.push(ServerCapabilities {
                        tools: Some(ToolsCapability { list_changed: Some(true) }),
                        prompts: None,
                        resources: None,
                    });
                }
            }
            
            if let Ok(info) = client.list_prompts(None).await {
                if !info.prompts.is_empty() {
                    debug!("Client {}: found {} prompts", idx, info.prompts.len());
                    // 如果客户端有提示，我们假设它支持 prompts capability
                    all_caps.push(ServerCapabilities {
                        tools: None,
                        prompts: Some(PromptsCapability { list_changed: Some(true) }),
                        resources: None,
                    });
                }
            }
            
            if let Ok(info) = client.list_resources(None).await {
                if !info.resources.is_empty() {
                    debug!("Client {}: found {} resources", idx, info.resources.len());
                    // 如果客户端有资源，我们假设它支持 resources capability
                    all_caps.push(ServerCapabilities {
                        tools: None,
                        prompts: None,
                        resources: Some(ResourcesCapability { list_changed: Some(true), subscribe: Some(true) }),
                    });
                }
            }
        }
        
        let aggregated_caps = merge_server_capabilities(&all_caps);
        debug!("Aggregated capabilities: {:?}", aggregated_caps);

        // 简单组合所有服务器的 instructions
        let instructions = String::from("Proxy aggregator of upstream servers.\n");
        // 由于没法获取每个服务器的指令，我们使用默认指令

        let name = "mcp-proxy-server".to_string();
        info!("ProxyRouter created successfully with name: {}", name);

        // 缓存所有客户端的工具
        info!("Caching tools from all upstream clients...");
        for (idx, client) in upstream_clients.iter().enumerate() {
            match client.list_tools(None).await {
                Ok(result) => {
                    debug!("Client {}: cached {} tools", idx, result.tools.len());
                    for t in result.tools {
                        // Add any new ones that aren't duplicates
                        if !cached_tools.iter().any(|existing: &Tool| existing.name == t.name) {
                            debug!("Caching tool: {}", t.name);
                            cached_tools.push(t);
                        }
                    }
                }
                Err(e) => {
                    // If an upstream is unreachable, we skip it
                    error!("Client {}: initial list_tools error: {}", idx, e);
                }
            }
        }
        info!("Cached {} tools from all upstream clients", cached_tools.len());

        // 7) Return a ProxyRouter that holds them
        Ok(Self {
            upstream_clients: Arc::new(upstream_clients),
            name,
            instructions,
            capabilities: aggregated_caps,
            rt: Arc::clone(&rt),
            cached_tools: Arc::new(cached_tools),
        })
    }
}

/// Merge multiple `ServerCapabilities` into a single union for demonstration.
fn merge_server_capabilities(caps_list: &[ServerCapabilities]) -> ServerCapabilities {
    // We'll union each sub-field naively, OR-ing any booleans we find.
    // Real logic may be more nuanced (like deciding how to unify different "subscribe" options).
    let mut prompts_enabled = false;
    let mut resources_enabled = false;
    let mut tools_enabled = false;
    for caps in caps_list {
        if caps.prompts.is_some() {
            prompts_enabled = true;
        }
        if caps.resources.is_some() {
            resources_enabled = true;
        }
        if caps.tools.is_some() {
            tools_enabled = true;
        }
    }
    let prompts = prompts_enabled.then(|| PromptsCapability { list_changed: Some(true) });
    let resources = resources_enabled.then(|| ResourcesCapability {
        subscribe: Some(true),
        list_changed: Some(true),
    });
    let tools = tools_enabled.then(|| ToolsCapability {
        list_changed: Some(true),
    });

    ServerCapabilities {
        prompts,
        resources,
        tools,
    }
}

/// Implementation of Router trait for ProxyRouter, effectively making this a local "server."
///
/// We delegate (or aggregate) calls to each upstream client as needed.
impl Router for ProxyRouter {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn instructions(&self) -> String {
        self.instructions.clone()
    }

    fn capabilities(&self) -> ServerCapabilities {
        self.capabilities.clone()
    }

    /// Return cached tools instead of querying upstream clients each time
    fn list_tools(&self) -> Vec<Tool> {
        debug!("ProxyRouter: returning {} cached tools", self.cached_tools.len());
        self.cached_tools.to_vec()
    }

    /// When a tool is called, search each upstream to see if it has that tool. If found, call it.
    fn call_tool(
        &self,
        tool_name: &str,
        arguments: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Content>, ToolError>> + Send>> {
        info!("ProxyRouter: call_tool '{}' with arguments: {}", tool_name, arguments);
        let tool_name = tool_name.to_string();
        let arguments = arguments.clone();
        let upstream_clients = self.upstream_clients.clone();
        Box::pin(async move {
            for (idx, client) in upstream_clients.iter().enumerate() {
                debug!("Checking client {} for tool '{}'", idx, tool_name);
                // Check if this server has the tool
                let tools_res = client.list_tools(None).await;
                let Ok(tools) = tools_res else { 
                    warn!("Client {}: failed to list tools", idx);
                    continue; 
                };
                let found = tools.tools.iter().any(|t| t.name == tool_name);
                if !found {
                    debug!("Client {}: tool '{}' not found", idx, tool_name);
                    continue;
                }
                
                debug!("Client {}: found tool '{}', calling it", idx, tool_name);
                // If found, call the tool
                let call_res = client.call_tool(&tool_name, arguments.clone()).await;
                match call_res {
                    Ok(result) => {
                        // Return it as soon as the first server succeeds
                        let content = result.content;
                        let is_err = result.is_error.unwrap_or(false);
                        if !is_err {
                            info!("Client {}: tool '{}' call succeeded", idx, tool_name);
                            return Ok(content);
                        } else {
                            // If server indicated error in the tool call
                            warn!("Client {}: tool '{}' call returned error status", idx, tool_name);
                            return Err(ToolError::ExecutionError(
                                "Upstream server indicated error".to_string(),
                            ));
                        }
                    }
                    Err(e) => {
                        // Upstream call error -> try next server
                        error!("Client {}: tool '{}' call error: {}", idx, tool_name, e);
                        // keep going
                    }
                }
            }
            // If none of the upstreams had the tool or returned success
            error!("Tool '{}' not found in any upstream server", tool_name);
            Err(ToolError::NotFound(format!("Tool {} not found upstream", tool_name)))
        })
    }

    /// Union of all upstream "resources"
    fn list_resources(&self) -> Vec<Resource> {
        let mut all_resources = vec![];
        for (idx, client) in self.upstream_clients.iter().enumerate() {
            match self.rt.block_on(client.list_resources(None)) {
                Ok(result) => {
                    debug!("Client {}: retrieved {} resources", idx, result.resources.len());
                    for r in result.resources {
                        if !all_resources.iter().any(|existing: &Resource| existing.uri == r.uri) {
                            debug!("Adding resource: {}", r.uri);
                            all_resources.push(r);
                        }
                    }
                }
                Err(e) => {
                    error!("Client {}: list_resources error: {}", idx, e);
                }
            }
        }
        debug!("ProxyRouter: returning {} aggregated resources", all_resources.len());
        all_resources
    }

    /// For reading a resource, pick the first server that says it has that resource
    fn read_resource(
        &self,
        uri: &str,
    ) -> Pin<Box<dyn Future<Output = Result<String, ResourceError>> + Send>> {
        info!("ProxyRouter: read_resource '{}'", uri);
        let uri = uri.to_string();
        let upstream_clients = self.upstream_clients.clone();
        Box::pin(async move {
            for (idx, client) in upstream_clients.iter().enumerate() {
                debug!("Checking client {} for resource '{}'", idx, uri);
                match client.read_resource(&uri).await {
                    Ok(res) => {
                        info!("Client {}: found resource '{}'", idx, uri);
                        return Ok(res.contents
                            .iter()
                            .map(|c| format!("{:?}", c))  // 简单使用Debug输出
                            .collect::<Vec<_>>()
                            .join("\n"));
                    }
                    Err(e) => {
                        warn!("Client {}: read_resource '{}' error: {}", idx, uri, e);
                        // keep going
                    }
                }
            }
            error!("Resource '{}' not found in any upstream server", uri);
            Err(ResourceError::NotFound(format!("Resource {} not found in any upstream", uri)))
        })
    }

    /// Union of all upstream prompts
    fn list_prompts(&self) -> Vec<Prompt> {
        let mut all_prompts = vec![];
        for (idx, client) in self.upstream_clients.iter().enumerate() {
            match self.rt.block_on(client.list_prompts(None)) {
                Ok(result) => {
                    debug!("Client {}: retrieved {} prompts", idx, result.prompts.len());
                    for p in result.prompts {
                        if !all_prompts.iter().any(|existing: &Prompt| existing.name == p.name) {
                            debug!("Adding prompt: {}", p.name);
                            all_prompts.push(p);
                        }
                    }
                }
                Err(e) => {
                    error!("Client {}: list_prompts error: {}", idx, e);
                }
            }
        }
        debug!("ProxyRouter: returning {} aggregated prompts", all_prompts.len());
        all_prompts
    }

    /// For retrieving a prompt, pick the first server that actually has it
    fn get_prompt(
        &self,
        prompt_name: &str,
    ) -> Pin<Box<dyn Future<Output = Result<String, PromptError>> + Send>> {
        let prompt_name = prompt_name.to_string();
        let upstream_clients = self.upstream_clients.clone();
        Box::pin(async move {
            for client in upstream_clients.iter() {
                // Try listing prompts
                let prompts_res = client.list_prompts(None).await;
                let Ok(prompts) = prompts_res else {
                    // keep going
                    continue;
                };
                let found = prompts.prompts.iter().any(|p| p.name == prompt_name);
                if !found {
                    continue;
                }
                // If it has the prompt, call get_prompt
                let prompt_res = client.get_prompt(&prompt_name, serde_json::json!({})).await;
                match prompt_res {
                    Ok(p) => {
                        // Return the entire "description" we got
                        let desc = p.description.unwrap_or_default();
                        return Ok(desc);
                    }
                    Err(e) => {
                        eprintln!("get_prompt ignoring upstream error: {}", e);
                    }
                }
            }
            Err(PromptError::NotFound(format!(
                "Prompt {} not found in any upstream",
                prompt_name
            )))
        })
    }
}

/// A main function that starts the "proxy server" on stdin/stdout
/// and listens for incoming JSON-RPC from downstream clients.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let file_appender = RollingFileAppender::new(Rotation::DAILY, "logs", "mcp-server.log");

    // Initialize logging with more detailed settings
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive("mcp_client=debug".parse().unwrap())
                .add_directive("mcp_server=debug".parse().unwrap())
                .add_directive("mcp_spec=debug".parse().unwrap())
        )
        .with_writer(file_appender)
        .init();

    info!("Starting MCP Proxy Server...");
    
    // 解析命令行参数
    let args = Args::parse();
    info!("Command line arguments: {:?}", args);

    // Build the aggregator router (connect to upstream servers, gather capabilities).
    info!("Initializing ProxyRouter...");
    let proxy_router = match ProxyRouter::new(Some(args.config)).await {
        Ok(router) => {
            info!("ProxyRouter initialized successfully");
            router
        },
        Err(e) => {
            error!("Failed to initialize ProxyRouter: {}", e);
            std::process::exit(1);
        }
    };
    
    let service = RouterService(proxy_router);
    info!("Created RouterService");

    // Wrap in the mcp_server's ByteTransport (reading from stdin, writing to stdout).
    let stdin = stdin();
    let stdout = stdout();
    debug!("Creating ByteTransport with stdin/stdout...");
    let transport = ByteTransport::new(stdin, stdout);
    
    debug!("Creating Server with RouterService...");
    let server = Server::new(service);

    // Run until EOF or other I/O break. This will block the current task.
    info!("Proxy MCP Server: starting main loop on stdin/stdout...");

    match server.run(transport).await {
        Ok(_) => {
            info!("Proxy MCP Server: main loop completed normally");
        },
        Err(e) => {
            error!("Proxy MCP Server: error in main loop: {}", e);
            error!("Error details: {:?}", e);
            std::process::exit(1);
        }
    }
    
    info!("Proxy MCP Server: shutting down.");

    Ok(())
}