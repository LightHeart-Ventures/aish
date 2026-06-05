use super::{Msg, Role, ToolCall, ToolDef, Turn};
use anyhow::{Context, Result};
use mistralrs::{
    CalledFunction, Function, GgufModelBuilder, Model, RequestBuilder, TextMessageRole, Tool,
    ToolCallResponse, ToolCallType, ToolChoice, ToolType,
};
use serde_json::Value;
use std::collections::HashMap;
use tokio::sync::OnceCell;

const DEFAULT_MODEL_ID: &str = "Qwen/Qwen3-8B-GGUF";
const DEFAULT_FILE: &str = "Qwen3-8B-Q4_K_M.gguf";
const DEFAULT_TOK_ID: &str = "Qwen/Qwen3-8B";

/// In-process inference via mistral.rs — the node-llama-cpp analog.
/// Weights are lazy-loaded on first use, not at shell startup.
pub struct LocalBackend {
    pub model_id: String,
    pub file: String,
    pub tok_model_id: String,
    model: OnceCell<Model>,
}

impl LocalBackend {
    pub fn new() -> Self {
        Self {
            model_id: DEFAULT_MODEL_ID.into(),
            file: DEFAULT_FILE.into(),
            tok_model_id: DEFAULT_TOK_ID.into(),
            model: OnceCell::new(),
        }
    }

    async fn model(&self) -> Result<&Model> {
        self.model
            .get_or_try_init(|| async {
                eprintln!(
                    "\x1b[2m  loading {} / {} (first use — downloads to the HF cache if needed)…\x1b[0m",
                    self.model_id, self.file
                );
                let model = GgufModelBuilder::new(self.model_id.clone(), vec![self.file.clone()])
                    .with_tok_model_id(self.tok_model_id.clone())
                    .build()
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to load local model: {e}"))?;
                eprintln!("\x1b[2m  model ready\x1b[0m");
                Ok::<_, anyhow::Error>(model)
            })
            .await
    }

    pub async fn complete(&self, system: &str, history: &[Msg], tools: &[ToolDef]) -> Result<Turn> {
        let model = self.model().await?;

        // Qwen3 soft-switch: a shell wants answers, not <think> spelunking.
        let system = format!("{system}\n/no_think");

        let mut req = RequestBuilder::new().add_message(TextMessageRole::System, system);
        for msg in history {
            match msg.role {
                Role::User => {
                    if msg.tool_results.is_empty() {
                        req = req.add_message(TextMessageRole::User, &msg.text);
                    } else {
                        for r in &msg.tool_results {
                            req = req.add_tool_message(&r.content, r.id.clone());
                        }
                    }
                }
                Role::Assistant => {
                    if msg.tool_calls.is_empty() {
                        req = req.add_message(TextMessageRole::Assistant, &msg.text);
                    } else {
                        let calls: Vec<ToolCallResponse> = msg
                            .tool_calls
                            .iter()
                            .enumerate()
                            .map(|(i, tc)| ToolCallResponse {
                                index: i,
                                id: tc.id.clone(),
                                tp: ToolCallType::Function,
                                function: CalledFunction {
                                    name: tc.name.clone(),
                                    arguments: tc.args.to_string(),
                                },
                            })
                            .collect();
                        req = req.add_message_with_tool_call(
                            TextMessageRole::Assistant,
                            msg.text.clone(),
                            calls,
                        );
                    }
                }
            }
        }
        req = req.set_tools(render_tools(tools)?).set_tool_choice(ToolChoice::Auto);

        let response = model
            .send_chat_request(req)
            .await
            .map_err(|e| anyhow::anyhow!("local inference failed: {e}"))?;
        let message = &response
            .choices
            .first()
            .context("local model returned no choices")?
            .message;

        let text = strip_think(message.content.as_deref().unwrap_or(""));
        let tool_calls = message
            .tool_calls
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(|tc| ToolCall {
                id: tc.id.clone(),
                name: tc.function.name.clone(),
                args: serde_json::from_str(&tc.function.arguments)
                    .unwrap_or_else(|_| Value::String(tc.function.arguments.clone())),
            })
            .collect();

        Ok(Turn { text, tool_calls, raw: None })
    }
}

fn render_tools(tools: &[ToolDef]) -> Result<Vec<Tool>> {
    tools
        .iter()
        .map(|t| {
            let parameters: HashMap<String, Value> = serde_json::from_value(t.schema.clone())
                .context("tool schema must be a JSON object")?;
            Ok(Tool {
                tp: ToolType::Function,
                function: Function {
                    name: t.name.to_string(),
                    description: Some(t.description.clone()),
                    parameters: Some(parameters),
                },
            })
        })
        .collect()
}

/// Qwen3 may still emit <think>…</think> even with /no_think, and mistral.rs
/// leaves the raw <tool_call>…</tool_call> text in content even after parsing
/// it into message.tool_calls — drop both.
fn strip_think(s: &str) -> String {
    let s = strip_tag(s, "<think>", "</think>");
    strip_tag(&s, "<tool_call>", "</tool_call>").trim().to_string()
}

fn strip_tag(s: &str, open: &str, close: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let (Some(a), Some(b)) = (rest.find(open), rest.find(close)) {
        if b < a {
            break;
        }
        out.push_str(&rest[..a]);
        rest = &rest[b + close.len()..];
    }
    out.push_str(rest);
    out
}
