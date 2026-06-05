//! Minimal MCP (Model Context Protocol) client — stdio transport only.
//!
//! Servers are declared in ~/.aish/.mcp.json using the same shape Claude Code
//! uses:
//!
//! ```json
//! { "mcpServers": { "name": { "command": "npx", "args": ["-y", "pkg"], "env": {} } } }
//! ```
//!
//! Each server is spawned once at startup; its tools join the model's tool set
//! as `mcp__<server>__<tool>`. The protocol is newline-delimited JSON-RPC 2.0:
//! initialize → notifications/initialized → tools/list, then tools/call per use.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};

const PROTOCOL_VERSION: &str = "2024-11-05";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const CALL_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Default)]
pub struct McpHost {
    servers: Vec<McpServer>,
}

struct McpTool {
    name: String,
    description: String,
    schema: Value,
    /// The server's `readOnlyHint` annotation — read-only tools skip the
    /// confirmation gate in careful/normal mode.
    read_only: bool,
}

struct McpServer {
    name: String,
    _child: Child, // held for lifetime; killed on drop
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    next_id: u64,
    tools: Vec<McpTool>,
}

impl McpHost {
    /// Spawn every server in `.mcp.json`. A server that fails to start or
    /// handshake is skipped with a warning — one bad entry must not take the
    /// shell down.
    pub async fn start(config_path: &Path) -> Self {
        let mut host = Self::default();
        let Ok(text) = std::fs::read_to_string(config_path) else {
            return host;
        };
        let config: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("\x1b[33maish:\x1b[0m {} is not valid JSON: {e}", config_path.display());
                return host;
            }
        };
        let Some(servers) = config["mcpServers"].as_object() else {
            return host;
        };
        for (name, spec) in servers {
            match McpServer::start(name, spec).await {
                Ok(s) => {
                    eprintln!(
                        "\x1b[2mmcp: {} up ({} tool{})\x1b[0m",
                        name,
                        s.tools.len(),
                        if s.tools.len() == 1 { "" } else { "s" }
                    );
                    host.servers.push(s);
                }
                Err(e) => eprintln!("\x1b[33maish:\x1b[0m mcp server {name} skipped: {e:#}"),
            }
        }
        host
    }

    /// Tool definitions for every connected server, namespaced.
    pub fn tool_defs(&self) -> Vec<crate::backend::ToolDef> {
        self.servers
            .iter()
            .flat_map(|s| {
                s.tools.iter().map(|t| crate::backend::ToolDef {
                    name: format!("mcp__{}__{}", s.name, t.name),
                    description: t.description.clone(),
                    schema: t.schema.clone(),
                })
            })
            .collect()
    }

    /// True when the server annotated this tool read-only (`readOnlyHint`).
    pub fn is_read_only(&self, qualified: &str) -> bool {
        let Some((server_name, tool)) =
            qualified.strip_prefix("mcp__").and_then(|r| r.split_once("__"))
        else {
            return false;
        };
        self.servers
            .iter()
            .find(|s| s.name == server_name)
            .and_then(|s| s.tools.iter().find(|t| t.name == tool))
            .is_some_and(|t| t.read_only)
    }

    /// Route an `mcp__server__tool` call to its server.
    pub async fn call(&mut self, qualified: &str, args: &Value) -> Result<String> {
        let rest = qualified
            .strip_prefix("mcp__")
            .ok_or_else(|| anyhow::anyhow!("not an mcp tool: {qualified}"))?;
        let (server_name, tool) = rest
            .split_once("__")
            .ok_or_else(|| anyhow::anyhow!("malformed mcp tool name: {qualified}"))?;
        let server = self
            .servers
            .iter_mut()
            .find(|s| s.name == server_name)
            .ok_or_else(|| anyhow::anyhow!("unknown mcp server: {server_name}"))?;

        let result = server
            .request(
                "tools/call",
                json!({"name": tool, "arguments": args}),
                CALL_TIMEOUT,
            )
            .await?;

        let mut out = String::new();
        for block in result["content"].as_array().map(|a| a.as_slice()).unwrap_or_default() {
            match block["type"].as_str() {
                Some("text") => {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(block["text"].as_str().unwrap_or_default());
                }
                Some(other) => out.push_str(&format!("\n[unsupported content block: {other}]")),
                None => {}
            }
        }
        if result["isError"].as_bool() == Some(true) {
            anyhow::bail!("{}", if out.is_empty() { "tool reported an error" } else { &out });
        }
        Ok(if out.is_empty() { "[no content]".into() } else { out })
    }
}

impl McpServer {
    async fn start(name: &str, spec: &Value) -> Result<Self> {
        let command = spec["command"]
            .as_str()
            .context("no `command` (only stdio servers are supported)")?;
        let args: Vec<&str> = spec["args"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();

        let mut cmd = tokio::process::Command::new(command);
        cmd.args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null()) // server logs must not corrupt the UI
            .kill_on_drop(true);
        if let Some(env) = spec["env"].as_object() {
            for (k, v) in env {
                if let Some(v) = v.as_str() {
                    cmd.env(k, v);
                }
            }
        }
        let mut child = cmd.spawn().with_context(|| format!("failed to spawn {command}"))?;

        let stdin = child.stdin.take().expect("piped");
        let lines = BufReader::new(child.stdout.take().expect("piped")).lines();
        let mut server = Self {
            name: name.to_string(),
            _child: child,
            stdin,
            lines,
            next_id: 0,
            tools: Vec::new(),
        };

        server
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "aish", "version": env!("CARGO_PKG_VERSION")},
                }),
                STARTUP_TIMEOUT,
            )
            .await
            .context("initialize handshake failed")?;
        server.notify("notifications/initialized").await?;

        let listed = server
            .request("tools/list", json!({}), STARTUP_TIMEOUT)
            .await
            .context("tools/list failed")?;
        for t in listed["tools"].as_array().map(|a| a.as_slice()).unwrap_or_default() {
            if let Some(tool_name) = t["name"].as_str() {
                server.tools.push(McpTool {
                    name: tool_name.to_string(),
                    description: t["description"].as_str().unwrap_or_default().to_string(),
                    schema: t["inputSchema"].clone(),
                    read_only: t["annotations"]["readOnlyHint"].as_bool().unwrap_or(false),
                });
            }
        }
        Ok(server)
    }

    async fn send(&mut self, msg: &Value) -> Result<()> {
        let mut line = serde_json::to_string(msg)?;
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn notify(&mut self, method: &str) -> Result<()> {
        self.send(&json!({"jsonrpc": "2.0", "method": method})).await
    }

    /// One JSON-RPC round trip. Server-initiated pings are answered inline;
    /// notifications and stale responses are skipped.
    async fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;

        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let line = tokio::time::timeout_at(deadline, self.lines.next_line())
                .await
                .map_err(|_| anyhow::anyhow!("{method}: timed out after {timeout:?}"))?
                .context("server pipe error")?
                .ok_or_else(|| anyhow::anyhow!("{method}: server closed its stdout"))?;
            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                continue; // not JSON — some servers leak banners onto stdout
            };
            if msg["id"].as_u64() == Some(id) && msg["method"].is_null() {
                if !msg["error"].is_null() {
                    anyhow::bail!(
                        "{method}: {}",
                        msg["error"]["message"].as_str().unwrap_or("unknown error")
                    );
                }
                return Ok(msg["result"].clone());
            }
            if msg["method"].as_str() == Some("ping") && !msg["id"].is_null() {
                let pong = json!({"jsonrpc": "2.0", "id": msg["id"].clone(), "result": {}});
                self.send(&pong).await?;
            }
            // anything else: notification or unrelated traffic — keep reading
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A python one-file MCP server: initialize, tools/list with one `add`
    /// tool, tools/call that adds two numbers.
    const MOCK_SERVER: &str = r#"
import json, sys
for line in sys.stdin:
    msg = json.loads(line)
    m, i = msg.get("method"), msg.get("id")
    if m == "initialize":
        r = {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}}, "serverInfo": {"name": "mock", "version": "0"}}
    elif m == "tools/list":
        r = {"tools": [{"name": "add", "description": "Add two numbers", "inputSchema": {"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}}}}]}
    elif m == "tools/call":
        s = msg["params"]["arguments"]["a"] + msg["params"]["arguments"]["b"]
        r = {"content": [{"type": "text", "text": str(s)}], "isError": False}
    else:
        continue  # notification
    if i is not None:
        sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": i, "result": r}) + "\n")
        sys.stdout.flush()
"#;

    #[tokio::test]
    async fn handshake_list_and_call() {
        let script = std::env::temp_dir().join("aish_mock_mcp.py");
        std::fs::write(&script, MOCK_SERVER).unwrap();
        let spec = json!({"command": "python3", "args": [script.to_str().unwrap()]});

        let server = match McpServer::start("mock", &spec).await {
            Ok(s) => s,
            Err(e) if format!("{e:#}").contains("failed to spawn") => return, // no python3 on host
            Err(e) => panic!("handshake failed: {e:#}"),
        };
        let mut host = McpHost { servers: vec![server] };

        let defs = host.tool_defs();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "mcp__mock__add");
        assert_eq!(defs[0].description, "Add two numbers");

        let out = host.call("mcp__mock__add", &json!({"a": 2, "b": 3})).await.unwrap();
        assert_eq!(out, "5");
    }
}
