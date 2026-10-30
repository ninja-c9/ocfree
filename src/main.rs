use anyhow::{bail, Context, Result};
use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::Response,
    routing::{get, post},
    Json, Router,
};
use clap::Parser;
use serde_json::{json, Map, Value};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

// Must match the CLI that produced harness.json; override with --user-agent
// (v2 CLI form is `opencode/<channel>/<version>/cli`).
const DEFAULT_UA: &str = "opencode/1.19.4 ai-sdk/provider-utils/4.0.23 runtime/bun/1.4.4";
const DEFAULT_UPSTREAM: &str = "https://opencode.ai/inference/v1/chat/completions";
const DEFAULT_MODELS_URLS: &[&str] = &[
    "https://opencode.ai/inference/v1/models",
    "https://opencode.ai/zen/v1/models",
];
// Headers describing the upstream connection rather than the payload.
const DROP_HEADERS: &[&str] = &[
    "content-encoding",
    "content-length",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "upgrade",
];

struct AppStateData {
    harness_system: String,
    harness_tools: Value,
    model_ids: Vec<String>,
    reqwest_client: reqwest::Client,
    shim_url: String,
    shim_token: String,
    key: Option<String>,
    upstream: String,
    models_urls: Vec<String>,
    user_agent: String,
}

#[derive(Parser)]
struct Cli {
    #[arg(long, default_value = "127.0.0.1:8931")]
    bind: SocketAddr,

    #[arg(long)]
    key: Option<String>,

    /// Comma-separated proxy list handed to the shim. Empty means "use the
    /// shim's own environment" (HTTPS_PROXY / NO_PROXY).
    #[arg(long, default_value = "")]
    proxy: String,

    /// One proxy per line, `#` comments allowed.
    #[arg(long)]
    proxy_file: Option<PathBuf>,

    #[arg(long, default_value = "harness.json")]
    harness: PathBuf,

    /// Path to shim.mjs. Defaults to $OCFREE_SHIM, then beside the binary,
    /// then the build checkout.
    #[arg(long)]
    shim: Option<PathBuf>,

    /// 0 picks a free loopback port.
    #[arg(long, default_value_t = 0)]
    shim_port: u16,

    #[arg(long, default_value = DEFAULT_UPSTREAM)]
    upstream: String,

    /// Overrides where /v1/models is read from; defaults to the upstream's
    /// /models plus the known Zen paths.
    #[arg(long)]
    models_url: Option<String>,

    #[arg(long, default_value = DEFAULT_UA)]
    user_agent: String,
}

fn conversation_key(headers: &HeaderMap, payload: &Value) -> String {
    for name in [
        "x-session-id",
        "x-opencode-session-id",
        "x-opencode-session",
        "x-session-affinity",
        "x-conversation-id",
    ] {
        let value = headers
            .get(name)
            .and_then(|h| h.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty());
        if let Some(value) = value {
            let mut hasher = DefaultHasher::new();
            value.hash(&mut hasher);
            return format!("h:{:016x}", hasher.finish());
        }
    }

    // No identity header: the conversation is the model plus its opening
    // system/user turn, both of which stay put as the transcript grows.
    let mut seed = String::new();
    if let Some(model) = payload.get("model").and_then(Value::as_str) {
        seed.push_str(model);
    }
    if let Some(messages) = payload.get("messages").and_then(Value::as_array) {
        for msg in messages.iter().filter(|msg| msg["role"] == "system").take(1) {
            seed.push('\u{0}');
            seed.push_str(msg["content"].as_str().unwrap_or(""));
        }
        for msg in messages.iter().filter(|msg| msg["role"] == "user").take(1) {
            seed.push('\u{0}');
            seed.push_str(msg["content"].as_str().unwrap_or(""));
        }
    }
    let mut hasher = DefaultHasher::new();
    seed.hash(&mut hasher);
    format!("b:{:016x}", hasher.finish())
}

fn upstream_body(state: &AppStateData, payload: &Value) -> Result<String> {
    let mut messages = Vec::new();
    let caller = payload
        .get("messages")
        .and_then(Value::as_array)
        .context("messages missing")?;
    // The harness prompt is the gate's, not the caller's: one system message.
    messages.push(json!({ "role": "system", "content": state.harness_system }));
    messages.extend(
        caller
            .iter()
            .filter(|msg| msg["role"] != "system")
            .cloned(),
    );

    let model = payload.get("model").and_then(Value::as_str).unwrap_or("");
    // Key order mirrors the CLI's JSON.stringify, which the gate reads.
    let mut body = Map::new();
    body.insert("model".into(), json!(model.strip_prefix("oc/").unwrap_or(model)));
    body.insert("messages".into(), Value::Array(messages));
    body.insert("tools".into(), state.harness_tools.clone());
    body.insert("stream".into(), json!(true));
    body.insert("max_completion_tokens".into(), json!(32000));
    body.insert("tool_choice".into(), json!("auto"));
    Ok(Value::Object(body).to_string())
}

fn forward_headers(builder: axum::http::response::Builder, headers: &HeaderMap) -> axum::http::response::Builder {
    let mut builder = builder;
    for (name, value) in headers.iter() {
        let lower = name.as_str().to_ascii_lowercase();
        if DROP_HEADERS.contains(&lower.as_str()) {
            continue;
        }
        if !(lower.starts_with("x-ratelimit") || lower.starts_with("x-ocfree") || lower == "retry-after" || lower == "x-opencode-log-id") {
            continue;
        }
        builder = builder.header(name, value);
    }
    builder
}

fn error_response(status: StatusCode, message: impl Into<String>, headers: Option<&HeaderMap>) -> Response {
    let body = json!({ "error": { "message": message.into(), "type": "upstream_error" } });
    let builder = Response::builder().status(status).header(header::CONTENT_TYPE, "application/json");
    let builder = match headers {
        Some(headers) => forward_headers(builder, headers),
        None => builder,
    };
    builder.body(Body::from(body.to_string())).unwrap()
}

fn aggregate_sse(sse: &str) -> Value {
    let mut content = String::new();
    let mut calls: Vec<ToolCallAcc> = Vec::new();
    let mut finish_reason: Option<String> = None;
    let mut usage: Option<Value> = None;

    for line in sse.lines() {
        let Some(data) = line.trim().strip_prefix("data: ") else { continue };
        if data == "[DONE]" { continue }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else { continue };
        if chunk["usage"].is_object() {
            usage = Some(chunk["usage"].clone());
        }
        let Some(choice) = chunk["choices"].get(0) else { continue };
        if let Some(text) = choice["delta"]["content"].as_str() {
            content.push_str(text);
        }
        if let Some(deltas) = choice["delta"]["tool_calls"].as_array() {
            for delta in deltas {
                let index = delta["index"].as_u64().unwrap_or(0) as usize;
                if calls.len() <= index {
                    calls.resize(index + 1, ToolCallAcc::default());
                }
                let call = &mut calls[index];
                if let Some(id) = delta["id"].as_str() {
                    call.id.push_str(id);
                }
                if let Some(name) = delta["function"]["name"].as_str() {
                    call.name.push_str(name);
                }
                if let Some(args) = delta["function"]["arguments"].as_str() {
                    call.arguments.push_str(args);
                }
            }
        }
        if let Some(reason) = choice["finish_reason"].as_str() {
            finish_reason = Some(reason.to_string());
        }
    }

    let mut message = Map::new();
    message.insert("role".into(), json!("assistant"));
    message.insert(
        "content".into(),
        if content.is_empty() { Value::Null } else { json!(content) },
    );
    if !calls.is_empty() {
        let tool_calls: Vec<Value> = calls
            .iter()
            .enumerate()
            .map(|(index, call)| {
                json!({
                    "id": if call.id.is_empty() { format!("call_{index}") } else { call.id.clone() },
                    "type": "function",
                    "function": {
                        "name": call.name,
                        "arguments": if call.arguments.is_empty() { "{}" } else { call.arguments.as_str() },
                    },
                })
            })
            .collect();
        message.insert("tool_calls".into(), Value::Array(tool_calls));
    }

    json!({
        "id": "chatcmpl-ocfree",
        "object": "chat.completion",
        "created": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        "model": "",
        "choices": [{
            "index": 0,
            "message": Value::Object(message),
            "finish_reason": finish_reason.unwrap_or_else(|| "stop".to_string()),
        }],
        "usage": usage,
    })
}

#[derive(Clone, Default)]
struct ToolCallAcc {
    id: String,
    name: String,
    arguments: String,
}

async fn healthz(State(state): State<Arc<AppStateData>>) -> Json<Value> {
    Json(json!({ "ok": true, "models": state.model_ids.len() }))
}

fn authorized(state: &AppStateData, headers: &HeaderMap) -> bool {
    match &state.key {
        None => true,
        Some(key) => headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .is_some_and(|provided| provided == key),
    }
}

async fn models_handler(
    State(state): State<Arc<AppStateData>>,
    headers: HeaderMap,
) -> axum::response::Response {
    if !authorized(&state, &headers) {
        return error_response(StatusCode::UNAUTHORIZED, "invalid bearer token", None);
    }
    let data: Vec<Value> = state
        .model_ids
        .iter()
        .map(|id| json!({ "id": id.clone(), "object": "model" }))
        .collect();
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({ "object": "list", "data": data }).to_string()))
        .unwrap()
}

async fn chat_completions(
    State(state): State<Arc<AppStateData>>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    if !authorized(&state, &headers) {
        return error_response(StatusCode::UNAUTHORIZED, "invalid bearer token", None);
    }

    let stream = payload.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let body = match upstream_body(&state, &payload) {
        Ok(body) => body,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, error.to_string(), None),
    };
    let conv = conversation_key(&headers, &payload);

    // Session id and proxy are pinned by the shim per conversation; it rotates
    // both only when upstream answers 429.
    let envelope = json!({
        "url": state.upstream,
        "method": "POST",
        "conv": conv,
        "headers": {
            "authorization": "Bearer public",
            "content-type": "application/json",
            "accept": "text/event-stream",
            "user-agent": state.user_agent,
            "x-opencode-client": "cli",
            "x-opencode-project": "global",
        },
        "body": body,
    });

    let upstream = match state
        .reqwest_client
        .post(&state.shim_url)
        .header("x-ocfree-token", &state.shim_token)
        .json(&envelope)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => return error_response(StatusCode::BAD_GATEWAY, error.to_string(), None),
    };

    let status = upstream.status();
    if status != StatusCode::OK {
        let headers = upstream.headers().clone();
        let body = upstream.text().await.unwrap_or_default();
        return error_response(status, body, Some(&headers));
    }

    if stream {
        let builder = Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache");
        let builder = forward_headers(builder, upstream.headers());
        return builder
            .body(Body::from_stream(upstream.bytes_stream()))
            .unwrap();
    }

    let sse = upstream.text().await.unwrap_or_default();
    let completion = aggregate_sse(&sse);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(completion.to_string()))
        .unwrap()
}

async fn fetch_models(state: &AppStateData) -> Result<Vec<String>> {
    let mut last_error = None;
    for url in &state.models_urls {
        let envelope = json!({
            "url": url,
            "method": "GET",
            "headers": { "authorization": "Bearer public" },
            "body": "",
        });
        let result = state
            .reqwest_client
            .post(&state.shim_url)
            .header("x-ocfree-token", &state.shim_token)
            .json(&envelope)
            .send()
            .await;
        let response = match result {
            Ok(response) => response,
            Err(error) => {
                last_error = Some(error.to_string());
                continue;
            }
        };
        if response.status() != StatusCode::OK {
            last_error = Some(format!("{url} answered {}", response.status()));
            continue;
        }
        let text = response.text().await.context("models body")?;
        let models: Value = serde_json::from_str(&text).context("models JSON")?;
        let ids = models["data"]
            .as_array()
            .context("no data in models")?
            .iter()
            .filter_map(|item| item["id"].as_str().map(str::to_string))
            .collect();
        return Ok(ids);
    }
    bail!("{}", last_error.unwrap_or_else(|| "no models endpoint".to_string()))
}

async fn wait_for_shim(shim_url: &str, token: &str) -> Result<()> {
    let client = reqwest::Client::new();
    for _ in 0..40 {
        if let Ok(response) = client
            .get(format!("{shim_url}/health"))
            .header("x-ocfree-token", token)
            .send()
            .await
        {
            if response.status() == StatusCode::OK {
                return Ok(());
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    bail!("timed out waiting for the shim on {shim_url}/health")
}

fn pick_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    Ok(listener.local_addr()?.port())
}

fn resolve_shim(cli: &Cli) -> Result<PathBuf> {
    if let Some(path) = &cli.shim {
        return Ok(path.clone());
    }
    if let Ok(path) = std::env::var("OCFREE_SHIM") {
        return Ok(PathBuf::from(path));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let beside = dir.join("shim.mjs");
            if beside.exists() {
                return Ok(beside);
            }
            // target/release/ocfree -> checkout root
            if let Some(root) = dir.parent().and_then(|dir| dir.parent()) {
                let checkout = root.join("shim.mjs");
                if checkout.exists() {
                    return Ok(checkout);
                }
            }
        }
    }
    let checkout = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("shim.mjs");
    if checkout.exists() {
        return Ok(checkout);
    }
    bail!("shim.mjs not found; pass --shim")
}

fn models_urls(cli: &Cli) -> Vec<String> {
    if let Some(url) = &cli.models_url {
        return vec![url.clone()];
    }
    let mut urls = vec![cli.upstream.replace("/chat/completions", "/models")];
    urls.extend(DEFAULT_MODELS_URLS.iter().map(|url| url.to_string()));
    urls.dedup();
    urls
}

fn proxy_list(cli: &Cli) -> Result<Vec<String>> {
    let mut proxies: Vec<String> = cli
        .proxy
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect();
    if let Some(path) = &cli.proxy_file {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading proxy file {path:?}"))?;
        proxies.extend(
            text.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .map(str::to_string),
        );
    }
    Ok(proxies)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let harness_str = std::fs::read_to_string(&cli.harness)
        .with_context(|| format!("reading {:?}", cli.harness))?;
    let harness: Value = serde_json::from_str(&harness_str).context("parsing harness JSON")?;
    let harness_system = harness["system"].as_str().context("harness missing system")?;
    let harness_tools = harness["tools"].clone();

    let shim_path = resolve_shim(&cli)?;
    let shim_port = if cli.shim_port == 0 { pick_port()? } else { cli.shim_port };
    let shim_token = format!("{:032x}", rand::random::<u64>());
    let proxies = proxy_list(&cli)?;

    let bun = std::env::var("OCFREE_BUN").unwrap_or_else(|_| "bun".to_string());
    let mut command = std::process::Command::new(bun);
    command
        .arg(&shim_path)
        .env("OCFREE_SHIM_PORT", shim_port.to_string())
        .env("OCFREE_SHIM_TOKEN", &shim_token)
        .env("OCFREE_RETRIES", "3");
    if !proxies.is_empty() {
        command.env("OCFREE_PROXIES", proxies.join(","));
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("spawning bun {shim_path:?}"))?;

    let shim_url = format!("http://127.0.0.1:{shim_port}");
    if let Err(error) = wait_for_shim(&shim_url, &shim_token).await {
        let _ = child.kill();
        return Err(error);
    }

    let model_endpoints = models_urls(&cli);
    let mut state = AppStateData {
        harness_system: harness_system.to_string(),
        harness_tools,
        model_ids: Vec::new(),
        reqwest_client: reqwest::Client::builder()
            .build()
            .context("reqwest client build")?,
        shim_url,
        shim_token,
        key: cli.key,
        upstream: cli.upstream.clone(),
        models_urls: model_endpoints,
        user_agent: cli.user_agent,
    };

    match fetch_models(&state).await {
        Ok(models) => state.model_ids = models,
        Err(error) => eprintln!("models unavailable at startup: {error}"),
    }

    let router = Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/models", get(models_handler))
        .with_state(Arc::new(state));

    let listener = tokio::net::TcpListener::bind(cli.bind).await?;
    let result = axum::serve(listener, router).await;
    let _ = child.kill();
    result.map_err(Into::into)
}
