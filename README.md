# MCP Local Router

This project is an MCP (Model Context Protocol) local router that serves as an aggregation proxy for MCP servers. It can connect to multiple upstream MCP servers and aggregate their functionalities into a single interface for downstream clients.

## Features

- Supports specifying a configuration file via command line arguments
- Supports configuring multiple upstream MCP servers
- Supports stdio transport
- Supports injecting environment variables into stdio transport

## Usage

### Running

It must be run with a configuration file:

```bash
cargo run -- --config mcp-config.json --transport sse
```

### Configuration File Format

The configuration file is in JSON format; an example is shown below:

```json
{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": [
        "-y",
        "@modelcontextprotocol/server-filesystem",
        "/Users/username/Desktop"
      ],
      "env": {
        "LINEAR_ACCESS_TOKEN": "your_personal_access_token"
      }
    },
    "everything": {
      "command": "npx",
      "args": [
        "-y",
        "@modelcontextprotocol/server-everything"
      ],
      "env": {}
    }
  }
}
```

Description:

- mcpServers: a mapping of multiple server configurations
  - Each key is the server's name (used for logging)
  - Each value is an object containing the following fields:
    - command: the command to execute
    - args: an array of command arguments
    - env: a mapping of environment variables to inject

## Build and Installation

```bash
# Build the project
cargo build --release

# Run
cargo run --release -- --config mcp-config.json --transport stdio
```

## Dependencies

- Rust 2021 Edition
- tokio async runtime
- MCP-related libraries: [rmcp](https://github.com/modelcontextprotocol/rust-sdk)
