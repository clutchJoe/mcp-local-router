use std::{
    borrow::Cow,
    collections::HashMap,
    io,
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{
        sse::{Event, Sse},
        Response,
    },
    routing::{get, post},
    Json, Router,
};
use clap::Parser;
use futures::{Sink, SinkExt, Stream, StreamExt};
use log::{debug, error, info};
#[cfg(windows)]
use process_wrap::tokio::JobObject;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
use process_wrap::tokio::TokioCommandWrap;
use rand::random;
use rmcp::{
    model::{
        CallToolRequestParam, CallToolResult, ClientJsonRpcMessage, ErrorData, Implementation,
        InitializeResult, ListToolsResult, PaginatedRequestParam, ServerCapabilities, Tool,
        ToolsCapability,
    },
    service::{
        serve_server_with_ct, RequestContext, RoleClient, RunningService, RxJsonRpcMessage,
        ServerInitializeError, ServiceError, ServiceExt, TxJsonRpcMessage,
    },
    transport::TokioChildProcess,
    RoleServer, ServerHandler,
};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, RwLock};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::{CancellationToken, PollSender};
use tracing_log;
use tracing_subscriber::{self, EnvFilter};

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
    #[serde(default)]
    env: HashMap<String, String>,
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long, value_name = "FILE")]
    config: PathBuf,

    #[arg(long, default_value = "stdio")]
    transport: String,

    #[arg(long, default_value = "127.0.0.1:8080")]
    address: String,
}

type ClientHandle = Arc<tokio::sync::Mutex<Option<RunningService<RoleClient, ()>>>>;
type ClientMap = Arc<RwLock<HashMap<String, ClientHandle>>>;
type ToolStore = Arc<RwLock<HashMap<Cow<'static, str>, (Tool, String)>>>;
type HealthStore = Arc<RwLock<HashMap<String, UpstreamStatus>>>;

#[derive(Clone, Debug, Default, Serialize)]
struct UpstreamStatus {
    is_online: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_success_epoch_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_error: Option<String>,
}

fn now_epoch_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

// ----------------------------------------------------------------
// AggregatorService: store the discovered tools in a HashMap<String, (Tool, String)>
// rather than Cow<'static, str> to avoid lifetime headaches:
#[derive(Clone)]
struct AggregatorService {
    // Key = tool name (Cow<'static, str>),
    // Value = (Tool object, which upstream server it came from)
    clients: ClientMap,
    tools: ToolStore,
    health: HealthStore,
}

impl AggregatorService {
    async fn replace_upstream_tools(tools_store: &ToolStore, upstream: &str, new_tools: Vec<Tool>) {
        let mut tools_guard = tools_store.write().await;
        tools_guard.retain(|_, (_, owner)| owner != upstream);
        for tool in new_tools {
            tools_guard.insert(
                Cow::Owned(tool.name.clone().into_owned()),
                (tool, upstream.to_string()),
            );
        }
    }

    async fn clear_upstream_tools(tools_store: &ToolStore, upstream: &str) {
        let mut tools_guard = tools_store.write().await;
        tools_guard.retain(|_, (_, owner)| owner != upstream);
    }

    async fn remove_client(
        clients_store: &ClientMap,
        health_store: &HealthStore,
        upstream: &str,
        reason: Option<String>,
    ) {
        let handle = clients_store.write().await.remove(upstream);
        if let Some(client_handle) = handle {
            info!("Removing upstream client '{}'", upstream);
            let mut guard = client_handle.lock().await;
            if let Some(client) = guard.take() {
                if let Err(e) = client.cancel().await {
                    error!("Failed to cancel client {}: {}", upstream, e);
                }
            }
        }
        AggregatorService::mark_status_offline(health_store, upstream, reason).await;
    }

    async fn mark_status_online(health_store: &HealthStore, upstream: &str) {
        let mut guard = health_store.write().await;
        let status = guard.entry(upstream.to_string()).or_default();
        status.is_online = true;
        status.last_success_epoch_ms = Some(now_epoch_millis());
        status.last_error = None;
    }

    async fn mark_status_offline(
        health_store: &HealthStore,
        upstream: &str,
        reason: Option<String>,
    ) {
        let mut guard = health_store.write().await;
        let status = guard.entry(upstream.to_string()).or_default();
        status.is_online = false;
        if let Some(reason) = reason {
            status.last_error = Some(reason);
        }
    }

    fn is_fatal_service_error(err: &ServiceError) -> bool {
        matches!(
            err,
            ServiceError::TransportClosed
                | ServiceError::TransportSend(_)
                | ServiceError::Cancelled { .. }
        )
    }

    async fn new(config: AppConfig) -> Result<Self> {
        let clients = Arc::new(RwLock::new(HashMap::new()));
        let tools = Arc::new(RwLock::new(HashMap::new()));
        let health = Arc::new(RwLock::new(HashMap::new()));

        // For each "upstream":
        for (name, server_config) in config.mcp_servers {
            info!(
                "Setting up upstream server '{}' with command: {} {:?}",
                name, server_config.command, server_config.args
            );

            health.write().await.entry(name.clone()).or_default();

            let mut command_wrap = TokioCommandWrap::with_new(&server_config.command, |command| {
                command.args(&server_config.args);
                command.envs(&server_config.env);
            });
            #[cfg(unix)]
            {
                command_wrap.wrap(ProcessGroup::leader());
            }
            #[cfg(windows)]
            {
                command_wrap.wrap(JobObject);
            }

            let transport = TokioChildProcess::new(command_wrap).context(format!(
                "Failed to create child process transport for {name}"
            ))?;

            let client_service = ()
                .serve(transport)
                .await
                .context(format!("Failed to create client service for {name}"))?;
            let client_arc = Arc::new(tokio::sync::Mutex::new(Some(client_service)));

            // Insert into "clients" map:
            clients
                .write()
                .await
                .insert(name.clone(), client_arc.clone());

            // Asynchronize tool discovery to avoid blocking aggregator initialization
            let name_for_discovery = name.clone();
            let client_arc_clone = client_arc.clone();
            let tools_clone = tools.clone();
            let clients_clone = clients.clone();
            let health_clone = health.clone();
            tokio::spawn(async move {
                let discovery_timeout = std::time::Duration::from_secs(30);
                let retry_delay = std::time::Duration::from_secs(5);

                loop {
                    let result = {
                        let guard = client_arc_clone.lock().await;
                        if let Some(ref client) = *guard {
                            tokio::time::timeout(discovery_timeout, client.list_all_tools()).await
                        } else {
                            debug!(
                                "Client for '{}' dropped before discovery completed",
                                name_for_discovery
                            );
                            return;
                        }
                    };

                    match result {
                        Ok(Ok(tools_list)) => {
                            for tool in &tools_list {
                                info!(
                                    "Discovered tool '{}' from upstream '{}'",
                                    tool.name, name_for_discovery
                                );
                            }
                            AggregatorService::replace_upstream_tools(
                                &tools_clone,
                                &name_for_discovery,
                                tools_list,
                            )
                            .await;
                            AggregatorService::mark_status_online(
                                &health_clone,
                                &name_for_discovery,
                            )
                            .await;
                            break;
                        }
                        Ok(Err(e)) => {
                            let error_text = e.to_string();
                            error!(
                                "Failed to list tools from upstream '{}': {}",
                                name_for_discovery, error_text
                            );
                            AggregatorService::clear_upstream_tools(
                                &tools_clone,
                                &name_for_discovery,
                            )
                            .await;
                            AggregatorService::mark_status_offline(
                                &health_clone,
                                &name_for_discovery,
                                Some(error_text.clone()),
                            )
                            .await;
                            if AggregatorService::is_fatal_service_error(&e) {
                                AggregatorService::remove_client(
                                    &clients_clone,
                                    &health_clone,
                                    &name_for_discovery,
                                    Some(error_text),
                                )
                                .await;
                                break;
                            }
                        }
                        Err(_) => {
                            error!(
                                "Tool discovery timed out for upstream '{}'",
                                name_for_discovery
                            );
                            AggregatorService::mark_status_offline(
                                &health_clone,
                                &name_for_discovery,
                                Some("Tool discovery timed out".to_string()),
                            )
                            .await;
                        }
                    }

                    tokio::time::sleep(retry_delay).await;
                }
            });
        }

        Ok(Self {
            clients,
            tools,
            health,
        })
    }

    pub async fn shutdown(&self) {
        let clients = self.clients.read().await;
        for (name, client_arc) in clients.iter() {
            info!("Shutting down client {}", name);
            let mut guard = client_arc.lock().await;
            if let Some(client) = guard.take() {
                // Add timeout to prevent hanging on unresponsive clients
                match tokio::time::timeout(std::time::Duration::from_secs(5), client.cancel()).await
                {
                    Ok(Ok(_)) => {
                        info!("Client {} shut down successfully", name);
                    }
                    Ok(Err(e)) => {
                        error!("Failed to cancel client {}: {}", name, e);
                    }
                    Err(_) => {
                        error!(
                            "Timeout shutting down client {} - forcefully terminating",
                            name
                        );
                        // The client didn't respond within timeout, but we continue
                        // with shutdown rather than hanging indefinitely
                    }
                }
            } else {
                info!("Client {} already shut down or taken.", name);
            }
            AggregatorService::mark_status_offline(&self.health, name, None).await;
        }
        info!("All upstream clients have been shut down.");
    }

    async fn get_client_handle(&self, name: &str) -> Option<ClientHandle> {
        let clients = self.clients.read().await;
        clients.get(name).cloned()
    }

    async fn upstream_names(&self) -> Vec<String> {
        let clients = self.clients.read().await;
        clients.keys().cloned().collect()
    }

    async fn health_snapshot(&self) -> HashMap<String, UpstreamStatus> {
        self.health.read().await.clone()
    }
}

#[derive(Clone)]
struct UpstreamProxyService {
    client_name: String,
    client: ClientHandle,
}

impl UpstreamProxyService {
    fn new(client_name: String, client: ClientHandle) -> Self {
        Self {
            client_name,
            client,
        }
    }
}

impl ServerHandler for UpstreamProxyService {
    fn get_info(&self) -> InitializeResult {
        InitializeResult {
            // 使用默认协议版本；rmcp 0.2.0 的 Default 会填入当前支持的 protocolVersion
            ..InitializeResult {
                capabilities: ServerCapabilities {
                    tools: Some(ToolsCapability::default()),
                    ..ServerCapabilities::default()
                },
                server_info: Implementation {
                    name: "local-router-upstream-proxy".to_string(),
                    version: env!("CARGO_PKG_VERSION").to_string(),
                },
                instructions: Some(format!(
                    "This endpoint proxies tools/* to the '{}' upstream server.",
                    self.client_name
                )),
                ..InitializeResult::default()
            }
        }
    }

    async fn list_tools(
        &self,
        params: Option<PaginatedRequestParam>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let guard = self.client.lock().await;
        if let Some(client) = guard.as_ref() {
            client
                .list_tools(params)
                .await
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))
        } else {
            Err(ErrorData::internal_error(
                format!("Client '{}' already shut down", self.client_name),
                None,
            ))
        }
    }

    async fn call_tool(
        &self,
        params: CallToolRequestParam,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let guard = self.client.lock().await;
        if let Some(client) = guard.as_ref() {
            info!(
                "Routing direct tool call '{}' to upstream '{}'",
                params.name, self.client_name
            );
            client
                .call_tool(params)
                .await
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))
        } else {
            Err(ErrorData::internal_error(
                format!("Client '{}' already shut down", self.client_name),
                None,
            ))
        }
    }
}

// ----------------------------------------------------------------
// Implement the trait using the async_trait macro to match
// the signature in rmcp::ServerHandler:
impl ServerHandler for AggregatorService {
    fn get_info(&self) -> InitializeResult {
        InitializeResult {
            capabilities: ServerCapabilities {
                tools: Some(ToolsCapability::default()),
                ..ServerCapabilities::default()
            },
            server_info: Implementation {
                name: "local-router".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
            instructions: Some("This server aggregates tools from multiple upstream MCP servers. Use tools/list then tools/call.".to_string()),
            ..InitializeResult::default()
        }
    }

    async fn list_tools(
        &self,
        _params: Option<PaginatedRequestParam>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        // 如果缓存为空，主动向所有上游拉取一次，避免初始化竞态导致空列表
        let need_refresh = {
            let g = self.tools.read().await;
            g.is_empty()
        };
        if need_refresh {
            let clients = self.clients.read().await;
            for (name, client_mutex) in clients.iter() {
                let guard = client_mutex.lock().await;
                if let Some(client) = guard.as_ref() {
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        client.list_all_tools(),
                    )
                    .await
                    {
                        Ok(Ok(tools_list)) => {
                            AggregatorService::replace_upstream_tools(
                                &self.tools,
                                name,
                                tools_list,
                            )
                            .await;
                            AggregatorService::mark_status_online(&self.health, name).await;
                        }
                        Ok(Err(e)) => {
                            let error_text = e.to_string();
                            error!(
                                "list_tools refresh failed for upstream '{}': {}",
                                name, error_text
                            );
                            AggregatorService::clear_upstream_tools(&self.tools, name).await;
                            AggregatorService::mark_status_offline(
                                &self.health,
                                name,
                                Some(error_text.clone()),
                            )
                            .await;
                            if AggregatorService::is_fatal_service_error(&e) {
                                AggregatorService::remove_client(
                                    &self.clients,
                                    &self.health,
                                    name,
                                    Some(error_text),
                                )
                                .await;
                            }
                        }
                        Err(_) => {
                            error!("list_tools refresh timeout for upstream '{}'", name);
                            AggregatorService::clear_upstream_tools(&self.tools, name).await;
                            AggregatorService::mark_status_offline(
                                &self.health,
                                name,
                                Some("list_tools refresh timeout".to_string()),
                            )
                            .await;
                        }
                    }
                }
            }
        }

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
            if let Some(client_mutex) = clients_guard.get(client_name) {
                info!(
                    "Routing call to tool '{}' -> upstream '{}'",
                    tool_name, client_name
                );
                let client_params = CallToolRequestParam {
                    name: tool_name.clone(),
                    arguments,
                };
                let guard = client_mutex.lock().await;
                if let Some(client) = guard.as_ref() {
                    match client.call_tool(client_params).await {
                        Ok(result) => Ok(result),
                        Err(e) => {
                            error!(
                                "Error calling tool '{}' on upstream '{}': {}",
                                tool_name, client_name, e
                            );
                            Err(ErrorData::internal_error(format!("{e}"), None))
                        }
                    }
                } else {
                    error!("Client '{}' already shut down or taken.", client_name);
                    Err(ErrorData::internal_error(
                        format!("Client '{client_name}' already shut down"),
                        None,
                    ))
                }
            } else {
                error!(
                    "Client '{}' associated with tool '{}' not found.",
                    client_name, tool_name
                );
                Err(ErrorData::internal_error(
                    format!("Client '{client_name}' not found"),
                    None,
                ))
            }
        } else {
            error!("Tool '{}' not found.", tool_name);
            Err(ErrorData::resource_not_found(
                format!("Tool '{tool_name}' not found"),
                None,
            ))
        }
    }
}

type SessionId = Arc<str>;
type TxStore = Arc<RwLock<HashMap<SessionId, mpsc::Sender<ClientJsonRpcMessage>>>>;

#[derive(Clone)]
struct RouterState {
    txs: TxStore,
    transport_tx: mpsc::UnboundedSender<TransportAssignment>,
    post_path: Arc<str>,
    aggregator: Arc<AggregatorService>,
}

struct TransportAssignment {
    target: TargetService,
    transport: RouterSseTransport,
}

#[derive(Clone)]
enum TargetService {
    Aggregator,
    Upstream { name: String, client: ClientHandle },
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PostEventQuery {
    session_id: String,
}

async fn post_event_handler(
    State(state): State<RouterState>,
    Query(PostEventQuery { session_id }): Query<PostEventQuery>,
    Json(message): Json<ClientJsonRpcMessage>,
) -> Result<StatusCode, StatusCode> {
    tracing::debug!(session_id, ?message, "new client message");
    let tx = {
        let guard = state.txs.read().await;
        guard
            .get(session_id.as_str())
            .cloned()
            .ok_or(StatusCode::NOT_FOUND)?
    };
    tx.send(message).await.map_err(|_| StatusCode::GONE)?;
    Ok(StatusCode::ACCEPTED)
}

async fn aggregator_sse_handler(
    State(state): State<RouterState>,
) -> Result<Sse<impl Stream<Item = Result<Event, io::Error>>>, Response<String>> {
    handle_sse_connection(state, TargetService::Aggregator).await
}

async fn upstream_sse_handler(
    State(state): State<RouterState>,
    Path(endpoint): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, io::Error>>>, Response<String>> {
    if let Some(client) = state.aggregator.get_client_handle(&endpoint).await {
        handle_sse_connection(
            state,
            TargetService::Upstream {
                name: endpoint,
                client,
            },
        )
        .await
    } else {
        let available = state.aggregator.upstream_names().await;
        let detail = if available.is_empty() {
            format!("Unknown upstream service '{endpoint}'")
        } else {
            format!(
                "Unknown upstream service '{endpoint}'. Available: {}",
                available.join(", ")
            )
        };
        Err(not_found_response(&detail))
    }
}

async fn health_handler(State(state): State<RouterState>) -> Json<HashMap<String, UpstreamStatus>> {
    let snapshot = state.aggregator.health_snapshot().await;
    Json(snapshot)
}

fn session_id() -> SessionId {
    let id = format!("{:016x}", random::<u128>());
    Arc::from(id)
}

async fn handle_sse_connection(
    state: RouterState,
    target: TargetService,
) -> Result<Sse<impl Stream<Item = Result<Event, io::Error>>>, Response<String>> {
    let session = session_id();
    tracing::info!(%session, "sse connection");
    let (from_client_tx, from_client_rx) = tokio::sync::mpsc::channel(64);
    let (to_client_tx, to_client_rx) = tokio::sync::mpsc::channel(64);
    state
        .txs
        .write()
        .await
        .insert(session.clone(), from_client_tx);
    let stream = ReceiverStream::new(from_client_rx);
    let sink = PollSender::new(to_client_tx);
    let transport = RouterSseTransport {
        stream,
        sink,
        session_id: session.clone(),
        tx_store: state.txs.clone(),
    };
    if state
        .transport_tx
        .send(TransportAssignment { target, transport })
        .is_err()
    {
        state.txs.write().await.remove(&session);
        return Err(internal_server_response("Transport channel closed"));
    }
    let post_path = state.post_path.clone();
    let stream = futures::stream::once(futures::future::ok(
        Event::default()
            .event("endpoint")
            .data(format!("{post_path}?sessionId={session}")),
    ))
    .chain(ReceiverStream::new(to_client_rx).map(|message| {
        match serde_json::to_string(&message) {
            Ok(payload) => Ok(Event::default().event("message").data(payload)),
            Err(e) => Err(io::Error::new(io::ErrorKind::InvalidData, e)),
        }
    }));
    Ok(Sse::new(stream))
}

fn internal_server_response(message: &str) -> Response<String> {
    let mut response = Response::new(message.to_string());
    *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
    response
}

fn not_found_response(message: &str) -> Response<String> {
    let mut response = Response::new(message.to_string());
    *response.status_mut() = StatusCode::NOT_FOUND;
    response
}

struct RouterSseTransport {
    stream: ReceiverStream<RxJsonRpcMessage<RoleServer>>,
    sink: PollSender<TxJsonRpcMessage<RoleServer>>,
    session_id: SessionId,
    tx_store: TxStore,
}

impl Sink<TxJsonRpcMessage<RoleServer>> for RouterSseTransport {
    type Error = io::Error;

    fn poll_ready(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.sink
            .poll_ready_unpin(cx)
            .map_err(std::io::Error::other)
    }

    fn start_send(
        mut self: std::pin::Pin<&mut Self>,
        item: TxJsonRpcMessage<RoleServer>,
    ) -> Result<(), Self::Error> {
        self.sink
            .start_send_unpin(item)
            .map_err(std::io::Error::other)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.sink
            .poll_flush_unpin(cx)
            .map_err(std::io::Error::other)
    }

    fn poll_close(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        let inner_close_result = self
            .sink
            .poll_close_unpin(cx)
            .map_err(std::io::Error::other);
        if inner_close_result.is_ready() {
            let session_id = self.session_id.clone();
            let tx_store = self.tx_store.clone();
            tokio::spawn(async move {
                tx_store.write().await.remove(&session_id);
            });
        }
        inner_close_result
    }
}

impl Stream for RouterSseTransport {
    type Item = RxJsonRpcMessage<RoleServer>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.stream.poll_next_unpin(cx)
    }
}

// ----------------------------------------------------------------
// Main function:
#[tokio::main]
async fn main() -> Result<()> {
    if let Err(e) = async_main().await {
        error!("Fatal error: {:?}", e);
        std::process::exit(1);
    }
    Ok(())
}

async fn async_main() -> Result<()> {
    tracing_log::LogTracer::init().ok();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive(tracing::Level::DEBUG.into()))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init()
        .ok();
    let pid = std::process::id();
    info!("Starting MCP Local Router with multi-transport (pid: {pid})");

    let args = Args::parse();
    info!("Loading configuration from: {:?}", args.config);
    let config_content = std::fs::read_to_string(&args.config)
        .with_context(|| format!("Failed to read config file: {:?}", args.config))?;
    let parsed: AppConfig =
        serde_json::from_str(&config_content).context("Config is not valid JSON")?;

    let aggregator_service = AggregatorService::new(parsed).await?;
    info!("AggregatorService successfully created.");

    match args.transport.as_str() {
        "stdio" => {
            info!("Using stdio transport");
            use tokio::io::{stdin, stdout};
            let aggregator_service = Arc::new(aggregator_service);
            let aggregator_for_shutdown = aggregator_service.clone();
            let shutdown_token = CancellationToken::new();

            let running_service = tokio::select! {
                result = serve_server_with_ct(
                    aggregator_service.as_ref().clone(),
                    (stdin(), stdout()),
                    shutdown_token.clone(),
                ) => {
                    match result {
                        Ok(running) => running,
                        Err(ServerInitializeError::Cancelled) => {
                            info!("Server initialization cancelled.");
                            aggregator_for_shutdown.shutdown().await;
                            return Ok(());
                        }
                        Err(err) => {
                            aggregator_for_shutdown.shutdown().await;
                            return Err(err.into());
                        }
                    }
                }
                ctrl = tokio::signal::ctrl_c() => {
                    match ctrl {
                        Ok(()) => info!(
                            "Ctrl+C received before initialization, shutting down aggregator service..."
                        ),
                        Err(err) => error!("Failed to listen for Ctrl+C: {}", err),
                    }
                    shutdown_token.cancel();
                    aggregator_for_shutdown.shutdown().await;
                    // Hard-exit to avoid hanging on blocking stdin read in stdio transport.
                    info!("Force-exiting process after Ctrl+C (stdio, early)");
                    std::process::exit(130);
                }
            };

            let (server_done_tx, server_done_rx) = tokio::sync::oneshot::channel();
            let server_handle = tokio::spawn(async move {
                let result = running_service.waiting().await;
                let _ = server_done_tx.send(());
                result
            });

            let server_done = async {
                let _ = server_done_rx.await;
            };

            let mut got_sigint = false;

            tokio::select! {
                ctrl = tokio::signal::ctrl_c() => {
                    match ctrl {
                        Ok(()) => info!("Ctrl+C received, shutting down aggregator service..."),
                        Err(err) => error!("Failed to listen for Ctrl+C: {}", err),
                    }
                    got_sigint = true;
                }
                _ = server_done => {
                    info!("Stdio transport finished");
                }
            }

            shutdown_token.cancel();
            aggregator_for_shutdown.shutdown().await;
            if got_sigint {
                // Hard-exit to avoid hanging on blocking stdin read in stdio transport.
                info!("Force-exiting process after Ctrl+C (stdio)");
                std::process::exit(130);
            }

            match server_handle.await {
                Ok(Ok(reason)) => {
                    info!("Server task finished: {:?}", reason);
                }
                Ok(Err(join_err)) => {
                    error!("Server task join error: {}", join_err);
                }
                Err(join_err) => {
                    error!("Server task panicked: {}", join_err);
                }
            }

            info!("Server stopped (stdio).");
        }
        "sse" => {
            info!("Using SSE transport on {}", args.address);
            let addr: SocketAddr = args.address.parse()?;
            let aggregator_service = Arc::new(aggregator_service);
            let aggregator_service_for_shutdown = aggregator_service.clone();

            let (transport_tx, transport_rx) = mpsc::unbounded_channel::<TransportAssignment>();
            let state = RouterState {
                txs: Arc::new(RwLock::new(HashMap::new())),
                transport_tx: transport_tx.clone(),
                post_path: Arc::<str>::from("/message"),
                aggregator: aggregator_service.clone(),
            };

            let cancellation = CancellationToken::new();
            let listener = TcpListener::bind(addr).await?;
            let cancel_for_server = cancellation.clone();
            let router_state = state.clone();
            let router = Router::new()
                .route("/sse", get(aggregator_sse_handler))
                .route("/sse/{endpoint}", get(upstream_sse_handler))
                .route("/health", get(health_handler))
                .route("/message", post(post_event_handler))
                .with_state(router_state);
            let server = axum::serve(listener, router).with_graceful_shutdown(async move {
                cancel_for_server.cancelled().await;
                info!("SSE router cancelled");
            });
            let server_handle = tokio::spawn(async move {
                if let Err(e) = server.await {
                    tracing::error!(error = %e, "SSE router error");
                }
            });

            let cancel_for_processor = cancellation.clone();
            let processor_aggregator = aggregator_service.clone();
            let processor_handle = tokio::spawn(async move {
                let mut transport_rx = transport_rx;
                while let Some(TransportAssignment { target, transport }) =
                    transport_rx.recv().await
                {
                    match target {
                        TargetService::Aggregator => {
                            let service = processor_aggregator.as_ref().clone();
                            let connection_ct = cancel_for_processor.child_token();
                            tokio::spawn(async move {
                                if let Err(e) = async {
                                    let running = service
                                        .serve_with_ct(transport, connection_ct.clone())
                                        .await
                                        .map_err(io::Error::other)?;
                                    running.waiting().await?;
                                    Ok::<(), io::Error>(())
                                }
                                .await
                                {
                                    // Check if this is a cancellation error (normal during shutdown)
                                    let error_str = e.to_string();
                                    if error_str.contains("Cancelled")
                                        || error_str.contains("cancelled")
                                    {
                                        tracing::info!(
                                            "Aggregator SSE connection closed during shutdown"
                                        );
                                    } else {
                                        tracing::error!(error = %e, "Aggregator SSE connection error");
                                    }
                                }
                            });
                        }
                        TargetService::Upstream { name, client } => {
                            let service = UpstreamProxyService::new(name.clone(), client);
                            let connection_ct = cancel_for_processor.child_token();
                            tokio::spawn(async move {
                                if let Err(e) = async {
                                    info!("Serving upstream '{}' over dedicated SSE", name);
                                    let running = service
                                        .serve_with_ct(transport, connection_ct.clone())
                                        .await
                                        .map_err(io::Error::other)?;
                                    running.waiting().await?;
                                    Ok::<(), io::Error>(())
                                }
                                .await
                                {
                                    // Check if this is a cancellation error (normal during shutdown)
                                    let error_str = e.to_string();
                                    if error_str.contains("Cancelled")
                                        || error_str.contains("cancelled")
                                    {
                                        tracing::info!(
                                            "Upstream '{}' SSE connection closed during shutdown",
                                            name
                                        );
                                    } else {
                                        tracing::error!(error = %e, "Upstream SSE connection error");
                                    }
                                }
                            });
                        }
                    }
                }
            });

            tokio::signal::ctrl_c().await?;
            info!("Ctrl+C received, shutting down aggregator service...");
            cancellation.cancel();
            drop(transport_tx);
            server_handle.await.ok();
            processor_handle.await.ok();
            aggregator_service_for_shutdown.shutdown().await;
            info!("Server stopped (SSE).");
        }
        _ => {
            error!("Invalid transport type: {}", args.transport);
            return Err(anyhow::anyhow!(
                "Invalid transport type: {}",
                args.transport
            ));
        }
    }
    info!("async_main completed, returning Ok(())");
    Ok(())
}
