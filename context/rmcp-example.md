下面展示 RMCP 在 Client/Server 两端最核心的几个 API 使用方式，直接选取示例里最常见、最具代表性的片段来说明。大体流程分为三步：定义服务或客户端 → 构建 Transport → 调用 serve 进行通讯并发请求。

⸻

Server 端

1. 定义服务

Server 端通常需要实现 ServerHandler Trait，并用宏 #[tool] 定义要暴露的工具。

use rmcp::{ServerHandler, model::ServerInfo, schemars, tool, tool_box};

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SumRequest {
    pub a: i32,
    pub b: i32,
}

#[derive(Debug, Clone)]
pub struct Calculator;

impl Calculator {
    #[tool(description = "Calculate the sum of two numbers")]
    fn sum(&self, #[tool(aggr)] SumRequest { a, b }: SumRequest) -> String {
        (a + b).to_string()
    }

    #[tool(description = "Calculate the difference of two numbers")]
    fn sub(&self, #[tool(param)] a: i32, #[tool(param)] b: i32) -> String {
        (a - b).to_string()
    }

    // 将这些方法注册到一个 "tool_box"
    tool_box!(Calculator { sum, sub });
}

impl ServerHandler for Calculator {
    // 自动派生 call_tool / list_tools 等功能
    tool_box!(@derive);

    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some("A simple calculator".to_string()),
            ..Default::default()
        }
    }
}

	•	#[tool] 宏用来声明一个可被远端调用的方法（Tool）。
	•	tool_box!() 宏会把定义的 Tool 收集起来，以便后面自动实现 call_tool 等操作。

2. 构建 Transport 并调用 serve

Server 只需要为输入/输出数据建立传输层，然后调用 serve。在以下示例里，使用 TCP 作为传输：

use rmcp::{serve_server};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:8001").await?;

    // 不断接收新的连接，给每个连接创建一个 Server
    while let Ok((stream, _addr)) = listener.accept().await {
        tokio::spawn(async move {
            let server = serve_server(Calculator, stream).await?;
            // 完成初始化后可选择等待结束
            server.waiting().await?;
            Ok::<_, anyhow::Error>(())
        });
    }
    Ok(())
}

	•	serve_server(Calculator, stream) 会返回一个 RunningService<RoleServer, _>，然后可调用 waiting().await 等方法进入事件循环。

⸻

Client 端

1. 客户端结构：调用 ServiceExt 的 serve 方法

Client 端使用 ().serve(...) 或者带有元信息的 ClientInfo::default().serve(...) 等方式启动，与 Server 端用法类似，只是角色是 RoleClient。下例中通过子进程作为传输层：

use rmcp::{ServiceExt, model::CallToolRequestParam, transport::TokioChildProcess};
use tokio::process::Command;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 准备传输：这里以子进程为例
    let transport = TokioChildProcess::new(Command::new("npx")
        .arg("-y")
        .arg("@modelcontextprotocol/server-everything"))?;

    // 启动客户端
    let client_service = ().serve(transport).await?; 
    // client_service 类型是 RunningService<RoleClient, ()>

    // 调用 list_tools 等核心 API
    let tools = client_service.list_tools(Default::default()).await?;
    println!("Tools: {:?}", tools);

    // 调用某个 tool
    let result = client_service.call_tool(CallToolRequestParam {
        name: "echo".into(),
        arguments: Some(serde_json::json!({
            "message": "hello from rmcp"
        }).as_object().unwrap().to_owned()),
    })
    .await?;
    println!("Echo result: {:?}", result);

    // 结束通讯
    client_service.cancel().await?;
    Ok(())
}

这里最核心的几个方法都来自 RunningService<RoleClient, T>：
	1.	list_tools / list_all_tools：获取远端所有 Tool 列表。
	2.	call_tool：调用指定 Tool 并传递参数。
	3.	list_resources, read_resource, list_prompts, get_prompt：如服务端实现了相应的资源/提示 API，这里可直接调用。
	4.	cancel / waiting：结束连接或等待服务退出。

⸻

总结
	•	Server 端实现 ServerHandler Trait（含工具声明），然后 serve_server(YourServerHandler, transport) 或者 yourServerHandler.serve(transport).await? 即可。
	•	Client 端只需要 (你的状态).serve(transport)，就能拿到 RunningService<RoleClient, T> 对象，再直接调用 list_tools(), call_tool() 等方法与 Server 交互。
	•	Transport 既可以是子进程的 I/O，也可以是 TCP/Unix Socket、WebSocket、HTTP 升级、SSE 等。RMCP 只要求能将 JsonRPC 消息在两端传输。
	•	核心 API 多定义在 RunningService<RoleClient, _> 与 RunningService<RoleServer, _>，比如：
	•	Client 端常见方法：
	•	list_tools, call_tool, list_resources, read_resource, list_prompts, get_prompt, cancel, waiting。
	•	Server 端内部只需实现 ServerHandler 接口提供 call_tool / list_tools / get_info 即可（可借助 #[tool] 宏来简化）。

以上即为示例项目里常用的核心 Client/Server API 用法。