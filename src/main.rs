use axum::{
    routing::{get, post},
    Json, Router,
    extract::State,
    http::HeaderMap,
};
use clap::Parser;
use rand::Rng;
use serde_json::Value;
use anyhow::{Result, Context, bail};
use std::net::SocketAddr;
use std::sync::Arc;

struct AppStateData {
    harness_system: String,
    harness_tools: Value,
    model_ids: Vec<String>,
    reqwest_client: reqwest::Client,
    shim_port: u16,
    shim_token: String,
    key: Option<String>,
    upstream: String,
}

#[derive(Parser)]
struct Cli {
    #[arg(long, default_value = "127.0.0.1:8931")]
    bind: SocketAddr,

    #[arg(long)]
    key: Option<String>,

    #[arg(long, default_value = "http://127.0.0.1:8899")]
    proxy: String,

    #[arg(long)]
    proxy_file: Option<String>,

    #[arg(long, default_value = "harness.json")]
    harness: String,

    #[arg(long, default_value = "https://opencode.ai/inference/v1/chat/completions")]
    upstream: String,
}

fn generate_session_id() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let hex_val = (!(ms * 0x1000 + 1)) & ((1u64 << 48) - 1);
    let hex_part = format!("{:012x}", hex_val);
    let base62_alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let base62_part: String = (0..14)
        .map(|_| {
            let idx = rand::thread_rng().gen_range(0..62);
            base62_alphabet.as_bytes()[idx] as char
        })
        .collect();
    format!("ses_{}{}", hex_part, base62_part)
}

async fn call_shim(shim_port: u16, token: &str, body: &str, client: &reqwest::Client) -> Result<Value> {
    let output = client
        .post(&format!("http://127.0.0.1:{}", shim_port))
        .header("Content-Type", "application/json")
        .header("x-ocfree-token", token)
        .body(body.to_string())
        .send()
        .await
        .context("failed to call shim")?;
    let text = output.text().await.context("shim non-text")?;
    let json: Value = serde_json::from_str(&text).context("shim non-JSON")?;
    Ok(json)
}

async fn wait_for_health(shim_port: u16, timeout_secs: u64) -> Result<()> {
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(timeout_secs);
    loop {
        if start.elapsed() > timeout {
            bail!("Timed out waiting for shim /health after {}s", timeout_secs);
        }
        let output = reqwest::Client::new()
            .get(&format!("http://127.0.0.1:{}/health", shim_port))
            .send()
            .await
            .context("failed to check shim health")?;
        let status = output.status().as_u16();
        if status == 200 { return Ok(()); }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

fn build_chat_completion(content: String, usage: Option<Value>) -> Value {
    serde_json::json!({
        "id": "chatcmpl-123",
        "object": "chat.completion",
        "created": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
        "usage": usage,
    })
}

async fn fetch_models_through_shim(shim_port: u16, token: &str, client: &reqwest::Client) -> Result<Vec<String>> {
    let body = serde_json::json!({
        "url": "https://opencode.ai/zen/v1/models",
        "method": "GET",
        "headers": {"authorization": "Bearer public"},
        "body": ""
    });
    let shim_res = call_shim(shim_port, token, &body.to_string(), client).await
        .context("failed to fetch models through shim")?;
    let status = shim_res["status"].as_u64().unwrap_or(0) as u16;
    if status != 200 {
        bail!("Shim returned status {}", status);
    }
    let text = shim_res["body"].as_str().context("no body from shim")?;
    let models_json: Value = serde_json::from_str(text)
        .context("failed to parse models JSON")?;
    let data = models_json["data"].as_array().context("no data in models")?;
    let mut ids = Vec::new();
    for item in data {
        let id = item["id"].as_str().context("model id missing")?.to_string();
        ids.push(id);
    }
    Ok(ids)
}

async fn parse_sse_lines(sse_text: &str, extract_content: bool) -> (Option<String>, Option<String>, Option<Value>) {
    let mut full_content: Option<String> = None;
    let mut finish_reason: Option<String> = None;
    let mut usage: Option<Value> = None;
    
    for line in sse_text.lines() {
        let line = line.trim();
        if line.is_empty() { continue; }
        if line == "[DONE]" { continue; }
        if !line.starts_with("data: ") { continue; }
        let data_str = &line[6..];
        let data: Value = match serde_json::from_str(data_str) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if extract_content {
            if let Some(choices) = data.get("choices").and_then(|c| c.as_array()) {
                if let Some(first) = choices.first() {
                    if let Some(delta) = first.get("delta").and_then(|d| d.as_object()) {
                        if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
                            full_content = Some(content.to_string());
                        }
                    }
                    if let Some(fr) = first.get("finish_reason").and_then(|f| f.as_str()) {
                        finish_reason = Some(fr.to_string());
                    }
                }
            }
        }
        if let Some(usage_val) = data.get("usage") {
            usage = Some(usage_val.clone());
        }
    }
    (full_content, finish_reason, usage)
}

fn parse_sse_for_stream(sse_text: &str) -> String {
    let mut chunks = Vec::new();
    for line in sse_text.lines() {
        let line = line.trim();
        if line.is_empty() { continue; }
        if line == "[DONE]" { continue; }
        if !line.starts_with("data: ") { continue; }
        let data_str = &line[6..];
        let data: Value = match serde_json::from_str(data_str) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(choices) = data.get("choices").and_then(|c| c.as_array()) {
            if let Some(first) = choices.first() {
                if let Some(delta) = first.get("delta").and_then(|d| d.as_object()) {
                    if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
                        chunks.push(content.to_string());
                    }
                }
            }
        }
    }
    chunks.join("")
}

async fn healthz() -> Json<Value> {
    Json(serde_json::json!({ "ok": true }))
}

async fn models_handler(State(state): State<Arc<AppStateData>>) -> Json<Value> {
    let data: Vec<Value> = state.model_ids.iter()
        .map(|id| serde_json::json!({ "id": id }))
        .collect();
    Json(serde_json::json!({
        "object": "list",
        "data": data,
    }))
}

async fn chat_completions(
    State(state): State<Arc<AppStateData>>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> axum::response::Response {
    // Bearer check from HeaderMap
    if let Some(key) = &state.key {
        let auth_val = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .and_then(|a| a.strip_prefix("Bearer "))
            .or_else(|| headers.get(axum::http::header::AUTHORIZATION).and_then(|h| h.to_str().ok()));
        if let Some(auth) = auth_val {
            if auth != *key {
                let body = serde_json::json!({
                    "error": {"message": "invalid bearer token", "type": "invalid_token"}
                });
                return axum::response::Response::builder()
                    .status(401)
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(serde_json::to_string(&body).unwrap()))
                    .unwrap();
            }
        }
    }

    let stream = payload.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
    let model = payload.get("model").and_then(|m| m.as_str()).unwrap_or("");
    let model_id = model.strip_prefix("oc/").unwrap_or(model);

    // Strip system messages from caller, keep harness system
    let mut messages = Vec::new();
    if let Some(msg_array) = payload.get("messages").and_then(|m| m.as_array()) {
        messages.push(serde_json::json!({
            "role": "system",
            "content": state.harness_system
        }));
        for msg in msg_array {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
            if role != "system" {
                messages.push(msg.clone());
            }
        }
    }

    // Build upstream body
    let upstream_model = model_id;
    let upstream_body = serde_json::json!({
        "model": upstream_model,
        "stream": true,
        "max_tokens": 32000,
        "messages": messages,
        "tools": state.harness_tools,
        "tool_choice": "auto",
    });

    let body_str = upstream_body.to_string();

    // Generate fresh session id per request
    let session_id = generate_session_id();

    // Build envelope for shim
    let envelope = serde_json::json!({
        "url": state.upstream,
        "method": "POST",
        "headers": {
            "authorization": "Bearer public",
            "content-type": "application/json",
            "user-agent": "opencode/1.19.4 ai-sdk/provider-utils/4.0.23 runtime/bun/1.4.4",
            "x-opencode-client": "cli",
            "x-opencode-project": "global",
            "x-opencode-session": session_id,
            "x-opencode-session-id": session_id,
            "x-session-affinity": session_id,
            "x-session-id": session_id,
        },
        "body": body_str,
    });
    let envelope_str = envelope.to_string();

    // Call shim with envelope
    let shim_res = match call_shim(state.shim_port, &state.shim_token, &envelope_str, &state.reqwest_client).await {
        Ok(v) => v,
        Err(e) => {
            let body = serde_json::json!({
                "error": {"message": e.to_string(), "type": "shim_error"}
            });
            return axum::response::Response::builder()
                .status(500)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap();
        }
    };
    let status = shim_res["status"].as_u64().unwrap_or(0) as u16;
    let upstream_body_str = match shim_res["body"].as_str() {
        Some(s) => s.to_string(),
        None => {
            let body = serde_json::json!({
                "error": {"message": "no body from shim", "type": "shim_error"}
            });
            return axum::response::Response::builder()
                .status(500)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap();
        }
    };

    if status != 200 {
        let body = serde_json::json!({ "error": {"message": upstream_body_str, "type": "upstream_error"} });
        let mut builder = axum::response::Response::builder().status(status).header("content-type", "application/json");
        if let Some(arr) = shim_res["headers"].as_array() { for h in arr { if let Some(pair) = h.as_array() { let name = pair.first().and_then(|v| v.as_str()).unwrap_or(""); let value = pair.get(1).and_then(|v| v.as_str()).unwrap_or(""); let lower = name.to_ascii_lowercase(); if lower == "retry-after" || lower == "x-opencode-log-id" || lower == "x-ratelimit-reset-after" { builder = builder.header(name, value); } } } }
        return builder.body(axum::body::Body::from(serde_json::to_string(&body).unwrap())).unwrap();
    }

    if stream {
        let mut resp = axum::response::Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .body(axum::body::Body::from(upstream_body_str))
            .unwrap();
        return resp;
    } else {
        // Aggregate SSE by appending every choices[0].delta.content (fix overwrite)
        let mut full_content = String::new();
        let mut usage: Option<Value> = None;
        let mut finish_reason = None;
        
        for line in upstream_body_str.lines() {
            let line = line.trim();
            if line.is_empty() { continue; }
            if line == "[DONE]" { continue; }
            if !line.starts_with("data: ") { continue; }
            let data_str = &line[6..];
            let data: Value = match serde_json::from_str(data_str) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if let Some(choices) = data.get("choices").and_then(|c| c.as_array()) {
                if let Some(first) = choices.first() {
                    if let Some(delta) = first.get("delta").and_then(|d| d.as_object()) {
                        if let Some(c) = delta.get("content").and_then(|c| c.as_str()) {
                            full_content.push_str(c);
                        }
                    }
                    if let Some(fr) = first.get("finish_reason").and_then(|f| f.as_str()) {
                        finish_reason = Some(fr.to_string());
                    }
                }
            }
            if let Some(usage_val) = data.get("usage") {
                usage = Some(usage_val.clone());
            }
        }
        
        let completion = serde_json::json!({
            "id": "chatcmpl-123",
            "object": "chat.completion",
            "created": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            "choices": [{"index": 0, "message": {"role": "assistant", "content": full_content}, "finish_reason": finish_reason.clone().unwrap_or_else(|| "stop".to_string())}],
            "usage": usage,
        });
        
        let body = serde_json::to_string(&completion).unwrap();
        let mut resp = axum::response::Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body))
            .unwrap();
        return resp;
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let harness_str = std::fs::read_to_string(&cli.harness)
        .with_context(|| format!("reading {:?}", cli.harness))?;
    let harness: Value = serde_json::from_str(&harness_str).with_context(|| "parsing harness JSON")?;
    let _system_check = harness["system"].as_str().context("harness missing system")?;

    let shim_token: String = format!("{:032x}", rand::random::<u64>());
    let shim_port: u16 = 8941;

    // Spawn bun shim child process
    let proxy_list = match &cli.proxy_file {
        Some(path) => std::fs::read_to_string(path)
            .with_context(|| format!("reading proxy file {path:?}"))?
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect::<Vec<_>>()
            .join(","),
        None => cli.proxy.clone(),
    };

    let mut child = std::process::Command::new("bun")
        .arg("/storage/cyber/tools/ocfree/shim.mjs")
        .env("OCFREE_SHIM_PORT", "8941")
        .env("OCFREE_SHIM_TOKEN", &shim_token)
        .env("HTTPS_PROXY", &cli.proxy)
        .env("OCFREE_PROXIES", &proxy_list)
        .env("OCFREE_RETRIES", "3")
        .spawn()
        .expect("failed to spawn shim");

    // Poll health endpoint up to 8s
    let timeout = std::time::Duration::from_secs(8);
    let start = std::time::Instant::now();
    loop {
        if start.elapsed() > timeout {
            bail!("Timed out waiting for shim /health after 8s");
        }
        match reqwest::Client::new()
            .get(&format!("http://127.0.0.1:8941/health"))
            .send()
            .await
        {
            Ok(resp) => {
                if resp.status() == 200 { break; }
            }
            Err(_) => {}
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    // Build reqwest client
    let reqwest_client = {
        let client = reqwest::Client::builder()
            .build()
            .expect("reqwest client build");
        client
    };

    let mut state = AppStateData {
        harness_system: harness["system"].as_str().unwrap_or("").to_string(),
        harness_tools: harness["tools"].clone(),
        model_ids: Vec::new(),
        reqwest_client,
        shim_port,
        shim_token: shim_token.clone(),
        key: cli.key,
        upstream: cli.upstream,
    };

    // Fetch models through shim
    let fetched_ids = fetch_models_through_shim(shim_port, &state.shim_token, &state.reqwest_client).await
        .unwrap_or_else(|_| {
            vec![
                "mimo-v2.6-flash-free".to_string(),
                "deepseek-v4.1-flash-free".to_string(),
                "jev-1.13-free".to_string(),
            ]
        });
    for id in &fetched_ids {
        state.model_ids.push(format!("oc/{}", id));
    }

    let arc_state = Arc::new(state);
    let router = Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/models", get(models_handler))
        .with_state(arc_state.clone());

    let listener = tokio::net::TcpListener::bind(cli.bind).await?;
    axum::serve(listener, router).await?;

    // Kill child on exit
    let _ = child.kill();

    Ok(())
}
