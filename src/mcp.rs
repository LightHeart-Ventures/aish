//! Minimal MCP (Model Context Protocol) client — stdio and HTTP transports.
//!
//! Servers are declared in `.mcp.json` (project root, highest precedence) and
//! `~/.aish/.mcp.json` (user scope), using the same shape Claude Code uses:
//!
//! ```json
//! {
//!   "mcpServers": {
//!     "fs":   { "command": "npx", "args": ["-y", "pkg"], "env": {} },
//!     "api":  {
//!       "type": "http",
//!       "url": "https://host/mcp/${TENANT}",
//!       "headers": { "X-Api-Key": "${API_KEY}" },
//!       "credentials": { "file": "~/.atum/credentials", "profile": "aish" }
//!     }
//!   }
//! }
//! ```
//!
//! `${NAME}` placeholders in `url`/`headers` resolve from the referenced
//! INI credentials profile first, then the process environment — so config
//! files carry no secrets. Each server is connected once at startup; its
//! tools join the model's tool set as `mcp__<server>__<tool>`. The protocol
//! is JSON-RPC 2.0: initialize → notifications/initialized → tools/list,
//! then tools/call per use (newline-delimited over stdio; POST per message
//! over HTTP, echoing the server's `mcp-session-id`).

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
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
    /// User-scope `.mcp.json` (last config path) — where `:mcp add`/`remove`
    /// persist changes. None until `start` records it.
    user_config: Option<PathBuf>,
    /// All `.mcp.json` paths `start` was given (project then user). Kept so
    /// `:mcp reload` can re-scan them and connect servers added to the file
    /// since startup.
    config_paths: Vec<PathBuf>,
}

/// One server's state for the `:mcp` listing.
pub struct McpStatus {
    pub name: String,
    pub kind: &'static str, // "stdio" | "http"
    pub detail: String,     // command (+args) or url
    pub tools: usize,
    pub prompts: usize,
}

struct McpTool {
    name: String,
    description: String,
    schema: Value,
    /// The server's `readOnlyHint` annotation — read-only tools skip the
    /// confirmation gate in careful/normal mode.
    read_only: bool,
}

/// An MCP prompt — servers publish reusable skills/playbooks this way
/// (`prompts/list` is the catalog, `prompts/get` expands one).
struct McpPrompt {
    name: String,
    description: String,
    /// (argument name, required)
    args: Vec<(String, bool)>,
}

/// One MCP-provided skill, flattened for the system-prompt catalog.
#[derive(Clone)]
pub struct McpSkill {
    pub server: String,
    pub name: String,
    pub description: String,
    pub args: Vec<(String, bool)>,
}

enum Transport {
    Stdio {
        _child: Child, // held for lifetime; killed on drop
        stdin: ChildStdin,
        lines: Lines<BufReader<ChildStdout>>,
    },
    Http {
        client: reqwest::Client,
        url: String,
        headers: Vec<(String, String)>,
        /// `mcp-session-id` echoed back once the server assigns one.
        session: Option<String>,
    },
}

struct McpServer {
    name: String,
    /// Raw config spec — kept so `:mcp reconnect` can restart the server.
    spec: Value,
    transport: Transport,
    next_id: u64,
    tools: Vec<McpTool>,
    prompts: Vec<McpPrompt>,
}

impl McpHost {
    /// Connect every server across the given configs (earlier paths win on a
    /// name collision — pass project config before user config). A server
    /// that fails to start or handshake is skipped with a warning — one bad
    /// entry must not take the shell down.
    pub async fn start(config_paths: &[&Path]) -> Self {
        let mut host = Self::default();
        // Last path is user scope (project precedes user) — adds/removes land there.
        host.user_config = config_paths.last().map(|p| p.to_path_buf());
        host.config_paths = config_paths.iter().map(|p| p.to_path_buf()).collect();
        host.connect_missing().await;
        host
    }

    /// Scan the configured `.mcp.json` paths and connect every server NOT already
    /// connected (earlier paths win on a name clash). Returns the names connected
    /// this pass. Shared by startup and `:mcp reload`, so a server added to the
    /// file is picked up live. Already-connected servers are left untouched; one
    /// bad entry never aborts the scan.
    async fn connect_missing(&mut self) -> Vec<String> {
        let mut added: Vec<String> = Vec::new();
        let paths = self.config_paths.clone();
        for path in &paths {
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };
            let config: Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!(
                        "\x1b[33maish:\x1b[0m {} is not valid JSON: {e}",
                        path.display()
                    );
                    continue;
                }
            };
            let Some(servers) = config["mcpServers"].as_object() else {
                continue;
            };
            for (name, spec) in servers {
                if self.servers.iter().any(|s| s.name == *name) || added.contains(name) {
                    continue; // already connected (earlier path / a prior pass) wins
                }
                match McpServer::start(name, spec).await {
                    Ok(s) => {
                        let skills = match s.prompts.len() {
                            0 => String::new(),
                            n => format!(", {n} skill{}", if n == 1 { "" } else { "s" }),
                        };
                        eprintln!(
                            "\x1b[2mmcp: {} up ({} tool{}{skills})\x1b[0m",
                            name,
                            s.tools.len(),
                            if s.tools.len() == 1 { "" } else { "s" }
                        );
                        self.servers.push(s);
                        added.push(name.clone());
                    }
                    Err(e) => eprintln!("\x1b[33maish:\x1b[0m mcp server {name} skipped: {e:#}"),
                }
            }
        }
        added
    }

    /// Re-read the `.mcp.json` config paths and connect any servers added to them
    /// since startup (or the last reload). Existing servers are left as-is — use
    /// `:mcp reconnect` to restart a changed one. Returns the names connected.
    pub async fn reload(&mut self) -> Vec<String> {
        self.connect_missing().await
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

    // ---- `:mcp` management -------------------------------------------------

    /// Per-server state for the `:mcp` listing.
    pub fn status(&self) -> Vec<McpStatus> {
        self.servers
            .iter()
            .map(|s| {
                let (kind, detail) = if s.spec["command"].is_string() {
                    let args = s.spec["args"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join(" ")
                        })
                        .unwrap_or_default();
                    (
                        "stdio",
                        format!("{} {args}", s.spec["command"].as_str().unwrap_or(""))
                            .trim()
                            .to_string(),
                    )
                } else {
                    ("http", s.spec["url"].as_str().unwrap_or("").to_string())
                };
                McpStatus {
                    name: s.name.clone(),
                    kind,
                    detail,
                    tools: s.tools.len(),
                    prompts: s.prompts.len(),
                }
            })
            .collect()
    }

    /// Connected server names.
    pub fn server_names(&self) -> Vec<String> {
        self.servers.iter().map(|s| s.name.clone()).collect()
    }

    /// `(tool name, read_only)` for one server, or None if it isn't connected.
    pub fn tools_of(&self, name: &str) -> Option<Vec<(String, bool)>> {
        self.servers.iter().find(|s| s.name == name).map(|s| {
            s.tools
                .iter()
                .map(|t| (t.name.clone(), t.read_only))
                .collect()
        })
    }

    /// Restart one server from its stored spec (drops the old one — its child is
    /// killed on drop). Fresh tool/prompt lists replace the old.
    pub async fn reconnect(&mut self, name: &str) -> Result<()> {
        let idx = self
            .servers
            .iter()
            .position(|s| s.name == name)
            .ok_or_else(|| anyhow::anyhow!("unknown mcp server: {name}"))?;
        let spec = self.servers[idx].spec.clone();
        let fresh = McpServer::start(name, &spec).await?;
        self.servers[idx] = fresh;
        Ok(())
    }

    /// Reconnect every server; returns each result so the caller can report.
    pub async fn reconnect_all(&mut self) -> Vec<(String, Result<()>)> {
        let names = self.server_names();
        let mut out = Vec::with_capacity(names.len());
        for name in names {
            let res = self.reconnect(&name).await;
            out.push((name, res));
        }
        out
    }

    /// Connect a new server from a spec and (optionally) persist it to the
    /// user-scope `.mcp.json`.
    pub async fn add(&mut self, name: &str, spec: Value, persist: bool) -> Result<()> {
        if self.servers.iter().any(|s| s.name == name) {
            anyhow::bail!("server {name} is already connected — use :mcp reconnect {name}");
        }
        let server = McpServer::start(name, &spec).await?;
        self.servers.push(server);
        if persist {
            self.persist_server(name, &spec)?;
        }
        Ok(())
    }

    /// Disconnect a server. Returns `(was_connected, removed_from_user_config)`.
    /// A server defined only in project-scope config is dropped live but returns
    /// on restart — the bools let the caller say so.
    pub fn remove(&mut self, name: &str) -> Result<(bool, bool)> {
        let before = self.servers.len();
        self.servers.retain(|s| s.name != name); // drop kills the child
        let was = self.servers.len() < before;
        let unpersisted = self.unpersist_server(name)?;
        Ok((was, unpersisted))
    }

    fn persist_server(&self, name: &str, spec: &Value) -> Result<()> {
        let path = self
            .user_config
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no user mcp config path to write"))?;
        let mut root: Value = std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_else(|| json!({"mcpServers": {}}));
        if !root["mcpServers"].is_object() {
            root["mcpServers"] = json!({});
        }
        root["mcpServers"][name] = spec.clone();
        std::fs::write(path, format!("{}\n", serde_json::to_string_pretty(&root)?))
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Remove a server from the user config. Returns true if it was present there.
    fn unpersist_server(&self, name: &str) -> Result<bool> {
        let Some(path) = self.user_config.as_ref() else {
            return Ok(false);
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            return Ok(false);
        };
        let mut root: Value = serde_json::from_str(&text).unwrap_or_else(|_| json!({}));
        let present = root["mcpServers"].get(name).is_some();
        if present {
            if let Some(map) = root["mcpServers"].as_object_mut() {
                map.remove(name);
            }
            std::fs::write(path, format!("{}\n", serde_json::to_string_pretty(&root)?))
                .with_context(|| format!("writing {}", path.display()))?;
        }
        Ok(present)
    }

    /// Every MCP-provided skill (prompt) across connected servers.
    pub fn skills(&self) -> Vec<McpSkill> {
        self.servers
            .iter()
            .flat_map(|s| {
                s.prompts.iter().map(|p| McpSkill {
                    server: s.name.clone(),
                    name: p.name.clone(),
                    description: p.description.clone(),
                    args: p.args.clone(),
                })
            })
            .collect()
    }

    /// Expand one skill via `prompts/get`, flattening its messages to text.
    pub async fn get_skill(
        &mut self,
        server_name: &str,
        name: &str,
        args: &Value,
    ) -> Result<String> {
        let server = self
            .servers
            .iter_mut()
            .find(|s| s.name == server_name)
            .ok_or_else(|| anyhow::anyhow!("unknown mcp server: {server_name}"))?;
        if !server.prompts.iter().any(|p| p.name == name) {
            anyhow::bail!("server {server_name} has no skill named {name}");
        }
        let result = server
            .request(
                "prompts/get",
                json!({"name": name, "arguments": args}),
                CALL_TIMEOUT,
            )
            .await?;
        let mut out = String::new();
        for m in result["messages"]
            .as_array()
            .map(|a| a.as_slice())
            .unwrap_or_default()
        {
            // `content` is one block or an array of blocks; text blocks only.
            let one = std::slice::from_ref(&m["content"]);
            let blocks = m["content"].as_array().map(|a| a.as_slice()).unwrap_or(one);
            for b in blocks {
                if b["type"].as_str() == Some("text") {
                    if !out.is_empty() {
                        out.push_str("\n\n");
                    }
                    out.push_str(b["text"].as_str().unwrap_or_default());
                }
            }
        }
        if out.is_empty() {
            anyhow::bail!("skill {name} returned no text content");
        }
        Ok(out)
    }

    /// True when the server annotated this tool read-only (`readOnlyHint`).
    pub fn is_read_only(&self, qualified: &str) -> bool {
        let Some((server_name, tool)) = qualified
            .strip_prefix("mcp__")
            .and_then(|r| r.split_once("__"))
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
        for block in result["content"]
            .as_array()
            .map(|a| a.as_slice())
            .unwrap_or_default()
        {
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
            anyhow::bail!(
                "{}",
                if out.is_empty() {
                    "tool reported an error"
                } else {
                    &out
                }
            );
        }
        Ok(if out.is_empty() {
            "[no content]".into()
        } else {
            out
        })
    }
}

/// Resolve `${NAME}` placeholders from a credentials profile (INI section),
/// falling back to the process environment. Unresolvable names are left
/// verbatim so the error surfaces at the server, not silently.
fn interpolate(s: &str, vars: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                let name = &after[..end];
                match vars.get(name).cloned().or_else(|| std::env::var(name).ok()) {
                    Some(v) => out.push_str(&v),
                    None => {
                        out.push_str("${");
                        out.push_str(name);
                        out.push('}');
                    }
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// Read one `[profile]` section of an INI-style credentials file
/// (`KEY = value` lines, AWS-credentials shaped). Shared with tools.rs for
/// `${profile:KEY}` references in spawn env values.
pub(crate) fn load_profile(file: &str, profile: &str) -> HashMap<String, String> {
    let path = if let Some(rest) = file.strip_prefix("~/") {
        Path::new(&std::env::var("HOME").unwrap_or_default()).join(rest)
    } else {
        Path::new(file).to_path_buf()
    };
    let mut vars = HashMap::new();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return vars;
    };
    let mut in_section = false;
    for line in text.lines() {
        let line = line.trim();
        if let Some(section) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_section = section == profile;
        } else if in_section {
            if let Some((k, v)) = line.split_once('=') {
                vars.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
    }
    vars
}

impl McpServer {
    async fn start(name: &str, spec: &Value) -> Result<Self> {
        let transport = if spec["command"].is_string() {
            Self::start_stdio(spec).await?
        } else if spec["url"].is_string() {
            Self::start_http(spec)?
        } else {
            anyhow::bail!("server needs a `command` (stdio) or `url` (http)");
        };
        let mut server = Self {
            name: name.to_string(),
            spec: spec.clone(),
            transport,
            next_id: 0,
            tools: Vec::new(),
            prompts: Vec::new(),
        };

        let init = server
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
        for t in listed["tools"]
            .as_array()
            .map(|a| a.as_slice())
            .unwrap_or_default()
        {
            if let Some(tool_name) = t["name"].as_str() {
                server.tools.push(McpTool {
                    name: tool_name.to_string(),
                    description: t["description"].as_str().unwrap_or_default().to_string(),
                    schema: t["inputSchema"].clone(),
                    read_only: t["annotations"]["readOnlyHint"].as_bool().unwrap_or(false),
                });
            }
        }

        // Server→client notifications ride a separate GET SSE stream on the
        // same URL (MCP streamable HTTP). Hold one open per HTTP server so
        // pushes (e.g. atum_subscribe_events deliveries) surface on the
        // user's terminal instead of vanishing. Servers without GET support
        // are detected on the first attempt and left alone.
        if let Transport::Http {
            url,
            headers,
            session,
            ..
        } = &server.transport
        {
            tokio::spawn(notification_stream(
                server.name.clone(),
                url.clone(),
                headers.clone(),
                session.clone(),
            ));
        }

        // Prompts are how servers publish skills/playbooks. Fetch the catalog
        // when the server advertises the capability; a failure here must not
        // take the server down — its tools still work.
        if init["capabilities"]["prompts"].is_object() {
            if let Ok(listed) = server
                .request("prompts/list", json!({}), STARTUP_TIMEOUT)
                .await
            {
                for p in listed["prompts"]
                    .as_array()
                    .map(|a| a.as_slice())
                    .unwrap_or_default()
                {
                    if let Some(prompt_name) = p["name"].as_str() {
                        let args = p["arguments"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|arg| {
                                        arg["name"].as_str().map(|n| {
                                            (
                                                n.to_string(),
                                                arg["required"].as_bool().unwrap_or(false),
                                            )
                                        })
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        server.prompts.push(McpPrompt {
                            name: prompt_name.to_string(),
                            description: p["description"].as_str().unwrap_or_default().to_string(),
                            args,
                        });
                    }
                }
            }
        }
        Ok(server)
    }

    async fn start_stdio(spec: &Value) -> Result<Transport> {
        let command = spec["command"].as_str().expect("checked");
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
        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn {command}"))?;
        let stdin = child.stdin.take().expect("piped");
        let lines = BufReader::new(child.stdout.take().expect("piped")).lines();
        Ok(Transport::Stdio {
            _child: child,
            stdin,
            lines,
        })
    }

    fn start_http(spec: &Value) -> Result<Transport> {
        // Secrets come from the referenced credentials profile (or env) via
        // ${NAME} interpolation — never from the config file itself.
        let vars = match (
            spec["credentials"]["file"].as_str(),
            spec["credentials"]["profile"].as_str(),
        ) {
            (Some(file), Some(profile)) => {
                let vars = load_profile(file, profile);
                if vars.is_empty() {
                    anyhow::bail!("credentials profile [{profile}] not found in {file}");
                }
                vars
            }
            _ => HashMap::new(),
        };
        let url = interpolate(spec["url"].as_str().expect("checked"), &vars);
        let headers: Vec<(String, String)> = spec["headers"]
            .as_object()
            .map(|h| {
                h.iter()
                    .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), interpolate(v, &vars))))
                    .collect()
            })
            .unwrap_or_default();
        Ok(Transport::Http {
            client: reqwest::Client::builder().timeout(CALL_TIMEOUT).build()?,
            url,
            headers,
            session: None,
        })
    }

    async fn send(&mut self, msg: &Value) -> Result<()> {
        match &mut self.transport {
            Transport::Stdio { stdin, .. } => {
                let mut line = serde_json::to_string(msg)?;
                line.push('\n');
                stdin.write_all(line.as_bytes()).await?;
                stdin.flush().await?;
                Ok(())
            }
            Transport::Http { .. } => {
                // fire-and-forget JSON-RPC notification over HTTP
                self.http_post(msg, Duration::from_secs(10))
                    .await
                    .map(|_| ())
            }
        }
    }

    async fn notify(&mut self, method: &str) -> Result<()> {
        self.send(&json!({"jsonrpc": "2.0", "method": method}))
            .await
    }

    /// POST one JSON-RPC message; returns the response body (possibly empty
    /// for notifications) and tracks the server's `mcp-session-id`.
    async fn http_post(&mut self, msg: &Value, timeout: Duration) -> Result<String> {
        let Transport::Http {
            client,
            url,
            headers,
            session,
        } = &mut self.transport
        else {
            unreachable!("http_post on stdio transport");
        };
        let mut req = client
            .post(url.as_str())
            .timeout(timeout)
            .header("accept", "application/json, text/event-stream")
            .json(msg);
        for (k, v) in headers.iter() {
            req = req.header(k, v);
        }
        if let Some(s) = session.as_deref() {
            req = req.header("mcp-session-id", s);
        }
        let resp = req.send().await.context("http request failed")?;
        if let Some(sid) = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            *session = Some(sid.to_string());
        }
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!(
                "http {status}: {}",
                body.chars().take(200).collect::<String>()
            );
        }
        Ok(body)
    }

    /// One JSON-RPC round trip on either transport. Over stdio,
    /// server-initiated pings are answered inline and notifications skipped;
    /// over HTTP the response body is the answer (SSE-framed bodies are
    /// unwrapped from their `data:` lines).
    async fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});

        if matches!(self.transport, Transport::Http { .. }) {
            let body = self.http_post(&msg, timeout).await?;
            for candidate in extract_jsonrpc_bodies(&body) {
                if candidate["id"].as_u64() == Some(id) {
                    if !candidate["error"].is_null() {
                        anyhow::bail!(
                            "{method}: {}",
                            candidate["error"]["message"]
                                .as_str()
                                .unwrap_or("unknown error")
                        );
                    }
                    return Ok(candidate["result"].clone());
                }
            }
            anyhow::bail!("{method}: no JSON-RPC response in http body");
        }

        self.send(&msg).await?;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let Transport::Stdio { lines, .. } = &mut self.transport else {
                unreachable!()
            };
            let line = tokio::time::timeout_at(deadline, lines.next_line())
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
                continue;
            }
            // Surface pushes that arrive while we're mid-request. (Between
            // requests nothing reads a stdio pipe — push-heavy servers
            // should use the HTTP transport, which has a dedicated stream.)
            announce_notification(&self.name, &msg);
        }
    }
}

/// Hold a GET SSE stream open against an HTTP MCP server and announce every
/// `notifications/*` it pushes. Quiet connections get idle-timed out by load
/// balancers, so the reconnect loop is load-bearing; a server that rejects
/// the first GET (405/406, wrong content type) simply doesn't push — give up
/// silently in that case only.
async fn notification_stream(
    server: String,
    url: String,
    headers: Vec<(String, String)>,
    session: Option<String>,
) {
    const RECONNECT: Duration = Duration::from_secs(5);
    // The shared client carries a global request timeout that would kill a
    // long-lived stream; build a timeout-free one for this task.
    let Ok(client) = reqwest::Client::builder().build() else {
        return;
    };
    let mut connected_once = false;
    loop {
        let mut req = client.get(&url).header("accept", "text/event-stream");
        for (k, v) in &headers {
            req = req.header(k, v);
        }
        if let Some(s) = &session {
            req = req.header("mcp-session-id", s);
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(_) if !connected_once => return,
            Err(_) => {
                tokio::time::sleep(RECONNECT).await;
                continue;
            }
        };
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !resp.status().is_success() || !ct.contains("text/event-stream") {
            if !connected_once {
                return; // server doesn't serve a push stream
            }
            tokio::time::sleep(RECONNECT).await;
            continue;
        }
        connected_once = true;

        let mut resp = resp;
        let mut buf = String::new();
        let mut data = String::new();
        while let Ok(Some(chunk)) = resp.chunk().await {
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(nl) = buf.find('\n') {
                let line = buf[..nl].trim_end_matches('\r').to_string();
                buf.drain(..=nl);
                if let Some(d) = line.strip_prefix("data:") {
                    data.push_str(d.trim_start());
                } else if line.is_empty() && !data.is_empty() {
                    if let Ok(msg) = serde_json::from_str::<Value>(&data) {
                        announce_notification(&server, &msg);
                    }
                    data.clear();
                }
            }
        }
        // A silently dead stream must not look like "no events".
        crate::tools::announce(
            &format!("[mcp:{server}]"),
            "notification stream dropped — reconnecting",
        );
        tokio::time::sleep(RECONNECT).await;
    }
}

/// One dim line per pushed notification: method tail + compact params.
fn announce_notification(server: &str, msg: &Value) {
    let Some(method) = msg["method"].as_str() else {
        return; // responses to our own POSTs can echo here — not pushes
    };
    let Some(kind) = method.strip_prefix("notifications/") else {
        return;
    };
    let params = serde_json::to_string(&msg["params"]).unwrap_or_default();
    let mut summary = format!("{kind} {params}");
    if summary.len() > 220 {
        let mut cut = 220;
        while !summary.is_char_boundary(cut) {
            cut -= 1;
        }
        summary.truncate(cut);
        summary.push('…');
    }
    crate::tools::announce(&format!("[mcp:{server}]"), &summary);
}

/// An HTTP MCP response body is either one JSON object or an SSE stream
/// (`data: {...}` lines). Yield every JSON object found.
fn extract_jsonrpc_bodies(body: &str) -> Vec<Value> {
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        return vec![v];
    }
    body.lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .filter_map(|d| serde_json::from_str(d.trim()).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A python one-file MCP server: initialize, tools/list with one `add`
    /// tool, tools/call that adds two numbers, and one `greet` prompt.
    const MOCK_SERVER: &str = r#"
import json, sys
for line in sys.stdin:
    msg = json.loads(line)
    m, i = msg.get("method"), msg.get("id")
    if m == "initialize":
        r = {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}, "prompts": {}}, "serverInfo": {"name": "mock", "version": "0"}}
    elif m == "tools/list":
        r = {"tools": [{"name": "add", "description": "Add two numbers", "inputSchema": {"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}}}}]}
    elif m == "tools/call":
        s = msg["params"]["arguments"]["a"] + msg["params"]["arguments"]["b"]
        r = {"content": [{"type": "text", "text": str(s)}], "isError": False}
    elif m == "prompts/list":
        r = {"prompts": [{"name": "greet", "description": "Say hello", "arguments": [{"name": "who", "required": True}, {"name": "tone"}]}]}
    elif m == "prompts/get":
        who = msg["params"].get("arguments", {}).get("who", "?")
        r = {"messages": [{"role": "user", "content": {"type": "text", "text": "Hello " + who}}]}
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
        let mut host = McpHost {
            servers: vec![server],
            ..Default::default()
        };

        let defs = host.tool_defs();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "mcp__mock__add");
        assert_eq!(defs[0].description, "Add two numbers");

        let out = host
            .call("mcp__mock__add", &json!({"a": 2, "b": 3}))
            .await
            .unwrap();
        assert_eq!(out, "5");

        // prompts/list became the skills catalog…
        let skills = host.skills();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].server, "mock");
        assert_eq!(skills[0].name, "greet");
        assert_eq!(
            skills[0].args,
            vec![("who".into(), true), ("tone".into(), false)]
        );

        // …and prompts/get expands one to text
        let body = host
            .get_skill("mock", "greet", &json!({"who": "world"}))
            .await
            .unwrap();
        assert_eq!(body, "Hello world");
        assert!(host.get_skill("mock", "nope", &json!({})).await.is_err());
    }

    #[test]
    fn interpolation_and_profiles() {
        let creds = std::env::temp_dir().join(format!("aish_creds_{}", std::process::id()));
        std::fs::write(
            &creds,
            "[other]\nKEY = nope\n[aish]\nKEY = k123\nTENANT=t_1\n",
        )
        .unwrap();
        let vars = load_profile(creds.to_str().unwrap(), "aish");
        assert_eq!(vars["KEY"], "k123");
        assert_eq!(vars["TENANT"], "t_1");
        assert!(load_profile(creds.to_str().unwrap(), "missing").is_empty());

        assert_eq!(
            interpolate("https://h/${TENANT}/mcp", &vars),
            "https://h/t_1/mcp"
        );
        assert_eq!(interpolate("${KEY}", &vars), "k123");
        assert_eq!(interpolate("${UNSET_XYZ}", &vars), "${UNSET_XYZ}"); // left for the server to reject
        let _ = std::fs::remove_file(&creds);
    }

    #[test]
    fn sse_and_json_bodies() {
        let plain = extract_jsonrpc_bodies(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#);
        assert_eq!(plain.len(), 1);
        let sse = extract_jsonrpc_bodies(
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"ok\":true}}\n\n",
        );
        assert_eq!(sse.len(), 1);
        assert_eq!(sse[0]["id"], 2);
    }
}
