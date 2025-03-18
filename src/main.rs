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
};

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
use tracing_subscriber::EnvFilter;

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
}

impl ProxyRouter {
    /// Creates a new ProxyRouter, connecting to multiple upstream servers.
    ///
    /// In this example, we connect to two SSE endpoints and one stdio process,
    /// just to illustrate how you might combine them. Adjust as you like.
    pub async fn new() -> anyhow::Result<Self> {
        // 1) Build a list of "transports" that connect to your upstream servers
        //    e.g. SseTransport, StdioTransport, etc.
        //
        // For demonstration, here's a hypothetical set of endpoints/processes:
        // SSE to server A
        let transport_a = SseTransport::new("http://localhost:8009/sse", HashMap::new());
        // A stdio-based server (for example, could be `cargo run -p mcp-server` or some other command)
        let transport_c = StdioTransport::new(
            "npx".to_string(),
            vec!["-y".to_string(), "@modelcontextprotocol/server-everything".to_string()],
            HashMap::new(),
        );

        // 2) Start each transport (async), producing a handle
        let handle_a = transport_a.start().await?;
        let handle_c = transport_c.start().await?;

        // 3) Wrap each transport handle in a Tower service with optional timeouts
        let service_a = McpService::with_timeout(handle_a, std::time::Duration::from_secs(10));
        let service_c = McpService::with_timeout(handle_c, std::time::Duration::from_secs(10));

        // 4) Create an McpClient for each
        let mut client_a = McpClient::new(service_a);
        let mut client_c = McpClient::new(service_c);

        // 5) Initialize each upstream server connection. In a real system, you might:
        //    - pass different ClientInfo or capabilities to each
        //    - handle errors individually
        let init_a = client_a.initialize(
            ClientInfo { name: "ProxyUpstream".into(), version: "1.0.0".into() },
            ClientCapabilities::default(),
        ).await?;
        let init_c = client_c.initialize(
            ClientInfo { name: "ProxyUpstream".into(), version: "1.0.0".into() },
            ClientCapabilities::default(),
        ).await?;

        // 6) Aggregate server capabilities (union) from all upstreams
        let aggregated_caps = merge_server_capabilities(&[
            init_a.capabilities.clone(),
            init_c.capabilities.clone(),
        ]);

        // For instructions, just combine them in some naive way:
        let instructions = format!(
            "Proxy aggregator of 3 upstream servers.\nServer A instructions:\n{}\n\nServer C instructions:\n{}\n",
            init_a.instructions.unwrap_or_default(),
            init_c.instructions.unwrap_or_default()
        );

        let name = "mcp-proxy-server".to_string();

        // 7) Return a ProxyRouter that holds them
        Ok(Self {
            upstream_clients: Arc::new(vec![
                Box::new(client_a),
                Box::new(client_c),
            ]),
            name,
            instructions,
            capabilities: aggregated_caps,
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

    /// Union of all upstream "tools"
    fn list_tools(&self) -> Vec<Tool> {
        let mut all_tools = vec![];
        for client in self.upstream_clients.iter() {
            // For each client, call list_tools synchronously (blocking on the future).
            // This is a toy example, you'd probably do something more advanced in production.
            let rt = tokio::runtime::Runtime::new().unwrap();
            match rt.block_on(client.list_tools(None)) {
                Ok(result) => {
                    for t in result.tools {
                        // Add any new ones that aren't duplicates
                        if !all_tools.iter().any(|existing: &Tool| existing.name == t.name) {
                            all_tools.push(t);
                        }
                    }
                }
                Err(e) => {
                    // If an upstream is unreachable, we skip it
                    eprintln!("list_tools: ignoring upstream error: {}", e);
                }
            }
        }
        all_tools
    }

    /// When a tool is called, search each upstream to see if it has that tool. If found, call it.
    fn call_tool(
        &self,
        tool_name: &str,
        arguments: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Content>, ToolError>> + Send>> {
        let tool_name = tool_name.to_string();
        let arguments = arguments.clone();
        let upstream_clients = self.upstream_clients.clone();
        Box::pin(async move {
            for client in upstream_clients.iter() {
                // Check if this server has the tool
                let tools_res = client.list_tools(None).await;
                let Ok(tools) = tools_res else { 
                    continue; 
                };
                let found = tools.tools.iter().any(|t| t.name == tool_name);
                if !found {
                    continue;
                }
                // If found, call the tool
                let call_res = client.call_tool(&tool_name, arguments.clone()).await;
                match call_res {
                    Ok(result) => {
                        // Return it as soon as the first server succeeds
                        let content = result.content;
                        let is_err = result.is_error.unwrap_or(false);
                        if !is_err {
                            return Ok(content);
                        } else {
                            // If server indicated error in the tool call
                            return Err(ToolError::ExecutionError(
                                "Upstream server indicated error".to_string(),
                            ));
                        }
                    }
                    Err(e) => {
                        // Upstream call error -> try next server
                        eprintln!("call_tool error from one server: {}", e);
                        // keep going
                    }
                }
            }
            // If none of the upstreams had the tool or returned success
            Err(ToolError::NotFound(format!("Tool {} not found upstream", tool_name)))
        })
    }

    /// Union of all upstream "resources"
    fn list_resources(&self) -> Vec<Resource> {
        let mut all_resources = vec![];
        for client in self.upstream_clients.iter() {
            let rt = tokio::runtime::Runtime::new().unwrap();
            match rt.block_on(client.list_resources(None)) {
                Ok(result) => {
                    for r in result.resources {
                        if !all_resources.iter().any(|existing: &Resource| existing.uri == r.uri) {
                            all_resources.push(r);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("list_resources: ignoring upstream error: {}", e);
                }
            }
        }
        all_resources
    }

    /// For reading a resource, pick the first server that says it has that resource
    fn read_resource(
        &self,
        uri: &str,
    ) -> Pin<Box<dyn Future<Output = Result<String, ResourceError>> + Send>> {
        let uri = uri.to_string();
        let upstream_clients = self.upstream_clients.clone();
        Box::pin(async move {
            for client in upstream_clients.iter() {
                match client.read_resource(&uri).await {
                    Ok(res) => {
                        // If found, return
                        // 简化处理方式，避免复杂的枚举模式匹配
                        return Ok(res.contents
                            .iter()
                            .map(|c| format!("{:?}", c))  // 简单使用Debug输出
                            .collect::<Vec<_>>()
                            .join("\n"));
                    }
                    Err(e) => {
                        eprintln!("read_resource ignoring upstream error: {}", e);
                        // keep going
                    }
                }
            }
            Err(ResourceError::NotFound(format!("Resource {} not found in any upstream", uri)))
        })
    }

    /// Union of all upstream prompts
    fn list_prompts(&self) -> Vec<Prompt> {
        let mut all_prompts = vec![];
        for client in self.upstream_clients.iter() {
            let rt = tokio::runtime::Runtime::new().unwrap();
            match rt.block_on(client.list_prompts(None)) {
                Ok(result) => {
                    for p in result.prompts {
                        if !all_prompts.iter().any(|existing: &Prompt| existing.name == p.name) {
                            all_prompts.push(p);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("list_prompts: ignoring upstream error: {}", e);
                }
            }
        }
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
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env().add_directive("mcp_client=debug".parse().unwrap()),
        )
        .init();

    // Build the aggregator router (connect to upstream servers, gather capabilities).
    let proxy_router = ProxyRouter::new().await?;
    let service = RouterService(proxy_router);

    // Wrap in the mcp_server's ByteTransport (reading from stdin, writing to stdout).
    let transport = ByteTransport::new(stdin(), stdout());
    let server = Server::new(service);

    // Run until EOF or other I/O break. This will block the current task.
    println!("Proxy MCP Server: starting main loop on stdin/stdout...");
    server.run(transport).await?;
    println!("Proxy MCP Server: shutting down.");

    Ok(())
}