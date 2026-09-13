//! provider.rs — one OpenAI-compatible chat-completions client (ureq).
//!
//! Covers Ollama, LiteLLM, vLLM, llama.cpp, and cloud APIs that speak the
//! same /chat/completions shape, ported from provider.py: any base_url,
//! `api_key_env` pointing at an env var (falling back to pi's credential
//! store ~/.pi/agent/auth.json), `usage` surfaced for the budget, and — new
//! in the Rust phase — SSE streaming (`stream: true`) assembled into the
//! same ChatMessage shape so the agent loop is mode-agnostic.

use serde_json::Value;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use super::config;

/// Loud, actionable provider failure (the POC rule: nothing fails silent).
#[derive(Debug)]
pub struct ProviderError {
    pub msg: String,
}

impl ProviderError {
    pub fn new(msg: String) -> Self {
        Self { msg }
    }
}

/// One outgoing message in the OpenAI chat shape.
#[derive(Debug)]
pub struct ApiMessage {
    pub role: String,
    pub content: String,
    pub tool_call_id: Option<String>,
    /// assistant turns carry the tool_calls they requested (replayed to the
    /// provider so the conversation stays consistent)
    pub tool_calls: Vec<ToolCall>,
}

/// A tool call the model asked for (arguments arrive as a JSON string; the
/// agent parses them like provider.py's json.loads).
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// Token counts a provider returned (used by the budget).
#[derive(Debug)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

/// A completed model turn: final text and/or tool calls, plus usage.
#[derive(Debug)]
pub struct ChatMessage {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
}

impl ChatMessage {
    pub fn plain(content: &str) -> Self {
        Self {
            content: Some(content.into()),
            tool_calls: Vec::new(),
            usage: None,
        }
    }
}

#[derive(Debug)]
pub struct Provider {
    pub name: String,
    pub base_url: String,
    pub model: String,
    pub context_window: u64,
    pub reasoning_effort: Option<String>,
    pub reasoning_supported: bool,
    pub stream: bool,
    api_key: Option<String>,
    agent: ureq::Agent,
}

/// Default bytes shown when a provider returns a malformed body.
const SNIPPET_CHARS: usize = 300;

pub fn resolve_provider_cfg(
    cfg: &config::Config,
    provider_name: Option<&str>,
) -> Result<config::ProviderCfg, ProviderError> {
    if cfg.providers.is_empty() {
        return Err(ProviderError::new(
            "no providers configured (add [providers.*] to curam.toml)".into(),
        ));
    }
    let name = provider_name.unwrap_or(cfg.default_provider.as_str());
    for p in &cfg.providers {
        if p.name == name {
            return Ok(p.clone());
        }
    }
    Err(ProviderError::new(format!(
        "unknown provider {:?} (configured: {})",
        name,
        cfg.providers
            .iter()
            .map(|p| p.name.clone())
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

/// Read an API key from pi's credential store (~/.pi/agent/auth.json),
/// mirroring how pi itself authenticates. auth.json shape:
/// {"<provider>": {"type": "api_key", "key": "sk-..."}}
fn key_from_pi_auth(provider_name: &str) -> Option<String> {
    let path = PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
        .join(".pi")
        .join("agent")
        .join("auth.json");
    if !path.exists() {
        return None;
    }
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let doc = serde_json::from_str(text.as_str()).unwrap_or(serde_json::Value::Null);
    let entry = doc.get(provider_name).and_then(|v| v.as_object());
    let key = entry
        .and_then(|e| e.get("key").and_then(|k| k.as_str()))
        .map(|k| k.to_string());
    match key {
        Some(k) if !k.is_empty() => Some(k),
        _ => None,
    }
}

/// One chunk on a live provider stream (the chat_stream reader thread).
#[derive(Debug)]
pub struct StreamChunk {
    pub kind: String, // "info" | "delta" | "done" | "error" | "cancelled"
    pub text: String,
}

/// Live handle to an in-flight provider call. The reader runs on a worker
/// thread; the caller pulls chunks with wait() and can cancel() at any point
/// (TUI Ctrl-C). The reader checks the flag between reads.
#[derive(Debug)]
pub struct ChatStream {
    pub cancel: Arc<AtomicBool>,
    rx: mpsc::Receiver<StreamChunk>,
}

impl ChatStream {
    /// Block up to ms milliseconds for the next chunk. None on timeout — the
    /// caller decides whether to keep waiting, cancel, or drain other work.
    pub fn wait(&self, ms: u64) -> Option<StreamChunk> {
        self.rx.recv_timeout(Duration::from_millis(ms)).ok()
    }

    /// Request cancellation of the in-flight provider call.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Split one "delta" payload down to the token text when it is a standard
/// chat.completion.chunk (choices[0].delta.content); otherwise pass the raw
/// line through (provider-specific shapes still render).
fn chunk_content(payload: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(payload).unwrap_or(serde_json::Value::Null);
    if let Some(c) = v.get("choices") {
        if let Some(arr) = c.as_array() {
            if let Some(first) = arr.first() {
                if let Some(d) = first.get("delta") {
                    if let Some(cont) = d.get("content") {
                        if let Some(s) = cont.as_str() {
                            return s.to_string();
                        }
                    }
                }
            }
        }
    }
    payload.to_string()
}

/// Emit one `data:` SSE line as a delta chunk (skips meta lines, [DONE]).
fn emit_sse_line(line: &str, tx: &mpsc::Sender<StreamChunk>) {
    if !line.starts_with("data:") {
        return;
    }
    let payload = line[5..].trim();
    if payload.is_empty() || payload == "[DONE]" {
        return;
    }
    tx.send(StreamChunk {
        kind: "delta".into(),
        text: chunk_content(payload),
    })
        .ok();
}

impl Provider {
    /// Build a provider from the effective config. `provider_name` selects
    /// the [providers.*] entry; `model_override` swaps only the model
    /// (the classifier's seam in classify.py).
    pub fn new(
        cfg: &config::Config,
        provider_name: Option<&str>,
        model_override: Option<&str>,
    ) -> Result<Self, ProviderError> {
        let p = resolve_provider_cfg(cfg, provider_name)?;
        if p.base_url.is_empty() {
            return Err(ProviderError::new(format!(
                "provider {:?} has no base_url",
                p.name
            )));
        }
        if !p.base_url.starts_with("http://") && !p.base_url.starts_with("https://") {
            return Err(ProviderError::new(format!(
                "unsupported base_url scheme {:?} (http/https only)",
                p.base_url
            )));
        }
        let base_url = p.base_url.trim_end_matches('/').to_string();
        let model = model_override.unwrap_or(p.model.as_str()).to_string();
        let api_key = if p.api_key_env.is_empty() {
            None
        } else {
            std::env::var_os(p.api_key_env.as_str())
                .map(|v| {
                    std::path::PathBuf::from(v)
                        .to_str()
                        .unwrap_or_default()
                        .to_string()
                })
                .or_else(|| key_from_pi_auth(p.name.as_str()))
        };
        let timeout = if p.timeout_s > 0.0 {
            p.timeout_s
        } else {
            120.0
        };
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(timeout as u64)))
            .build()
            .into();
        Ok(Self {
            name: p.name,
            base_url,
            model,
            context_window: p.context_window,
            reasoning_effort: p.reasoning_effort.clone(),
            reasoning_supported: p.reasoning_effort_supported,
            stream: p.stream,
            api_key,
            agent,
        })
    }

    fn headers(&self) -> Vec<(String, String)> {
        let mut h = vec![("Content-Type".into(), "application/json".into())];
        if let Some(k) = &self.api_key {
            h.push(("Authorization".into(), format!("Bearer {}", k)));
        }
        h
    }

    /// POST /chat/completions and return the assembled message (same shape
    /// for stream and non-stream modes).
    pub fn chat(&self, messages: &[ApiMessage]) -> Result<ChatMessage, ProviderError> {
        let body = self.request_body(messages);
        let url = format!("{}/chat/completions", self.base_url);
        let mut req = self
            .agent
            .post(url)
            .header("Content-Type", "application/json");
        if let Some(k) = &self.api_key {
            req = req.header("Authorization", format!("Bearer {}", k));
        }
        let mut resp = match req.send(body) {
            Ok(r) => r,
            Err(e) => {
                return Err(ProviderError::new(format!(
                    "provider unreachable ({}) — start your LLM (e.g. `ollama serve`) or check curam.toml default_provider: {}",
                    self.base_url, e
                )));
            }
        };
        let status = resp.status().as_u16();
        let text = match resp.body_mut().read_to_string() {
            Ok(t) => t,
            Err(e) => {
                return Err(ProviderError::new(format!(
                    "provider read error ({}): {}",
                    self.base_url, e
                )))
            }
        };
        if !(200..300).contains(&status) {
            return Err(ProviderError::new(format!(
                "provider error {} ({}): {}",
                status,
                self.base_url,
                &text[0..SNIPPET_CHARS]
            )));
        }
        if self.stream {
            assemble_sse(text.as_str())
        } else {
            parse_chat_json(text.as_str())
        }
    }

    /// Streaming variant of chat(): the request runs on a worker thread and
    /// the caller pulls chunks with ChatStream::wait() — deltas as the SSE
    /// lines arrive, then the assembled full body as "done". cancel() aborts
    /// the in-flight call (TUI Ctrl-C); the worker notices between reads.
    pub fn chat_stream(&self, messages: &[ApiMessage]) -> Result<ChatStream, ProviderError> {
        let body = self.request_body(messages);
        let url = format!("{}/chat/completions", self.base_url);
        let (tx, rx) = mpsc::channel::<StreamChunk>();
        let cancel = Arc::new(AtomicBool::new(false));
        let mode: String = if self.stream { "stream".into() } else { "plain".into() };
        let base_url: String = self.base_url.clone();
        let api_key: Option<String> = self.api_key.clone();
        let agent = self.agent.clone();
        let body_owned: String = body;
        let url_owned: String = url;
        let mode_owned: String = mode;
        let cancel_shared: Arc<AtomicBool> = cancel.clone();
        thread::spawn(move || {
            let mut req = agent.post(url_owned).header("Content-Type", "application/json");
            if let Some(k) = &api_key {
                req = req.header("Authorization", format!("Bearer {}", k));
            }
            tx.send(StreamChunk {
                kind: "info".into(),
                text: mode_owned.clone(),
            })
                .ok();
            let mut resp = match req.send(body_owned) {
                Ok(r) => r,
                Err(e) => {
                    tx.send(StreamChunk {
                        kind: "error".into(),
                        text: format!(
                            "provider unreachable ({}): {}",
                            base_url, e
                        )
                        .to_string(),
                    })
                        .ok();
                    return;
                }
            };
            let status = resp.status().as_u16();
            if !(200..300).contains(&status) {
                let full = resp.body_mut().read_to_string().unwrap_or_default();
                let head = if full.len() > SNIPPET_CHARS {
                    full[0..SNIPPET_CHARS].to_string()
                } else {
                    full
                };
                tx.send(StreamChunk {
                    kind: "error".into(),
                    text: format!(
                        "provider error {} ({}): {}",
                        status, base_url, head
                    )
                    .to_string(),
                })
                    .ok();
                return;
            }
            let mut rd = resp.body_mut().with_config().reader();
            let mut full = String::new();
            let mut whole = String::new(); // entire raw body (assembly input)
            let mut carry: Vec<u8> = vec![];
            let mut buf: [u8; 4096] = [0; 4096];
            let stream: bool = mode_owned == "stream";
            loop {
                if cancel_shared.load(Ordering::Relaxed) {
                    tx.send(StreamChunk {
                        kind: "cancelled".into(),
                        text: String::new(),
                    })
                        .ok();
                    return;
                }
                match rd.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let mut all: Vec<u8> = vec![];
                        for b in &carry {
                            all.push(*b);
                        }
                        for i in 0..n {
                            all.push(buf[i]);
                        }
                        carry = vec![];
                        match String::from_utf8(all.clone()) {
                            Err(_) => {
                                // incomplete multibyte tail — carry to next read
                                carry = all;
                            }
                            Ok(text) => {
                                whole += text.as_str();
                                if stream {
                                    full += text.as_str();
                                    let parts: Vec<&str> = full
                                        .split('\n')
                                        .collect::<Vec<&str>>();
                                    if parts.len() > 1 {
                                        for k in 0..(parts.len() - 1) {
                                            emit_sse_line(parts[k], &tx);
                                        }
                                        full = parts[parts.len() - 1].to_string();
                                    } else if !parts.is_empty() {
                                        full = parts[0].to_string();
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tx.send(StreamChunk {
                            kind: "error".into(),
                            text: format!(
                                "provider read error ({}): {}",
                                base_url, e
                            )
                            .to_string(),
                        })
                            .ok();
                        return;
                    }
                }
            }
            if stream && !full.is_empty() {
                whole += full.as_str(); // flush the trailing partial line
            }
            if !stream && !whole.is_empty() {
                // non-SSE: one delta so the UI shows the body arriving
                tx.send(StreamChunk {
                    kind: "delta".into(),
                    text: whole.clone(),
                })
                    .ok();
            }
            tx.send(StreamChunk {
                kind: "done".into(),
                text: whole.clone(),
            })
                .ok();
        });
        Ok(ChatStream { cancel, rx })
    }

    /// Build the request body: {model, messages, stream, reasoning_effort?,
    /// max_tokens?}. Tools ride this seam in H3 (the plan's tool schemas).
    fn request_body(&self, messages: &[ApiMessage]) -> String {
        let mut m = serde_json::Map::new();
        m.insert("model".to_owned(), self.model.as_str().into());
        m.insert("stream".to_owned(), self.stream.into());
        let arr = messages.iter().map(|msg| {
            let mut mm = serde_json::Map::new();
            mm.insert("role".to_owned(), msg.role.as_str().into());
            mm.insert("content".to_owned(), msg.content.as_str().into());
            if let Some(id) = &msg.tool_call_id {
                mm.insert("tool_call_id".to_owned(), id.to_string().into());
            }
            if !msg.tool_calls.is_empty() {
                let tcs = msg.tool_calls.iter().map(|tc| {
                    let mut f = serde_json::Map::new();
                    f.insert("name".to_owned(), tc.name.as_str().into());
                    f.insert("arguments".to_owned(), tc.arguments.as_str().into());
                    let mut tcm = serde_json::Map::new();
                    tcm.insert("id".to_owned(), tc.id.as_str().into());
                    tcm.insert("type".to_owned(), "function".into());
                    tcm.insert("function".to_owned(), serde_json::Value::Object(f));
                    serde_json::Value::Object(tcm)
                });
                mm.insert("tool_calls".to_owned(), tcs.collect::<Vec<Value>>().into());
            }
            serde_json::Value::Object(mm)
        });
        m.insert("messages".to_owned(), arr.collect::<Vec<Value>>().into());
        if let Some(re) = &self.reasoning_effort {
            if self.reasoning_supported {
                m.insert("reasoning_effort".to_owned(), re.as_str().into());
            }
        }
        serde_json::to_string(&serde_json::Value::Object(m)).expect("request body serializes")
    }
}

/// Parse a non-streaming /chat/completions response into a ChatMessage.
pub fn parse_chat_json(text: &str) -> Result<ChatMessage, ProviderError> {
    let doc: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            return Err(ProviderError::new(format!(
                "non-JSON provider response: {e}"
            )))
        }
    };
    let msg = doc
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.first())
        .and_then(|ch| ch.get("message"));
    let msg = match msg {
        Some(m) => m,
        None => {
            return Err(ProviderError::new(format!(
                "malformed provider response: {}",
                &text[0..SNIPPET_CHARS]
            )));
        }
    };
    let content = msg
        .get("content")
        .and_then(|c| c.as_str())
        .map(|s| s.to_string());
    let mut tool_calls = Vec::new();
    if let Some(arr) = msg.get("tool_calls").and_then(|t| t.as_array()) {
        for tc in arr {
            let id = tc.get("id").and_then(|i| i.as_str()).unwrap_or_default();
            let name = tc
                .get("function")
                .and_then(|f| f.as_object())
                .and_then(|o| o.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or_default();
            let args = tc
                .get("function")
                .and_then(|f| f.as_object())
                .and_then(|o| o.get("arguments"))
                .and_then(|a| a.as_str())
                .unwrap_or_default();
            tool_calls.push(ToolCall {
                id: id.to_string(),
                name: name.to_string(),
                arguments: args.to_string(),
            });
        }
    }
    let usage = usage_of(doc.get("usage"));
    Ok(ChatMessage {
        content,
        tool_calls,
        usage,
    })
}

fn usage_of(v: Option<&Value>) -> Option<Usage> {
    v.and_then(|u| {
        let pt = u.get("prompt_tokens").and_then(|x| x.as_u64());
        let ct = u.get("completion_tokens").and_then(|x| x.as_u64());
        if pt.is_some() && ct.is_some() {
            Some(Usage {
                prompt_tokens: pt.unwrap(),
                completion_tokens: ct.unwrap(),
            })
        } else {
            None
        }
    })
}

/// Assemble an SSE body into a ChatMessage. OpenAI-compatible servers send
/// one `data:` JSON per line and close after `data: [DONE]`; the parser is
/// tolerant of missing [DONE] (EOF ends the stream) and of usage arriving in
/// the final chunk (stream_options include_usage).
///
/// The full-body read happens first (H2); the progressive TUI reader wraps
/// this same per-line logic in H5.
pub fn assemble_sse(text: &str) -> Result<ChatMessage, ProviderError> {
    let mut content: Option<String> = None;
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut usage: Option<Usage> = None;
    let mut acc = String::new();
    let mut acc_set = false;
    for line in text.split('\n') {
        let line = line.trim_end_matches('\r');
        if !line.starts_with("data:") {
            continue;
        }
        let payload = line[5..].trim();
        if payload.is_empty() {
            continue;
        }
        if payload == "[DONE]" {
            break;
        }
        let chunk: serde_json::Value = match serde_json::from_str(payload) {
            Ok(c) => c,
            Err(_) => continue, // tolerate a malformed chunk, keep the stream
        };
        if let Some(u) = usage_of(chunk.get("usage")) {
            usage = Some(u)
        }
        let delta = chunk
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
            .and_then(|ch| ch.get("delta"));
        let delta = match delta {
            Some(d) => d,
            None => continue,
        };
        if let Some(seg) = delta.get("content").and_then(|c| c.as_str()) {
            acc.push_str(seg);
            acc_set = true;
        }
        if let Some(arr) = delta.get("tool_calls").and_then(|t| t.as_array()) {
            for tc in arr {
                let index = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                while tool_calls.len() <= index {
                    tool_calls.push(ToolCall {
                        id: String::new(),
                        name: String::new(),
                        arguments: String::new(),
                    });
                }
                let target = &mut tool_calls[index];
                if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                    target.id = id.to_string();
                }
                let fobj = tc.get("function").and_then(|f| f.as_object());
                if let Some(f) = fobj {
                    if let Some(n) = f.get("name").and_then(|n| n.as_str()) {
                        target.name = n.to_string();
                    }
                    if let Some(a) = f.get("arguments").and_then(|a| a.as_str()) {
                        target.arguments.push_str(a);
                    }
                }
            }
        }
    }
    if acc_set {
        content = Some(acc);
    }
    Ok(ChatMessage {
        content,
        tool_calls,
        usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    /// Serve one canned HTTP response on an ephemeral loopback port,
    /// returning the base_url a Provider can point at.
    fn serve(body: &str, status: u16) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let body_owned: String = body.to_string();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut drain: [u8; 8192] = [0; 8192];
            let _ = stream.read(&mut drain);
            let response = format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body_owned.len(), body_owned
            );
            stream.write(response.as_bytes()).ok();
            stream.flush().ok();
        });
        format!("http://127.0.0.1:{}", port)
    }

    fn test_cfg(base_url: String) -> config::Config {
        let mut cfg = config::Loader::new(None, None).load().unwrap();
        cfg.providers = vec![config::ProviderCfg {
            name: "fixture".into(),
            base_url: base_url,
            model: "test-model".into(),
            api_key_env: String::new(),
            context_window: 32768,
            reasoning_effort_supported: true,
            reasoning_effort: Some("medium".into()),
            timeout_s: 10.0,
            max_tokens: None,
            stream: false,
        }];
        cfg.default_provider = "fixture".into();
        cfg
    }

    #[test]
    fn chat_json_with_tool_calls_and_usage() {
        let body = "{\"choices\":[{\"message\":{\"role\":\"assistant\",\"content\":null,\"tool_calls\":[{\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"a.py\\\"}\"}}]}}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}";
        let url = serve(body, 200);
        let p = Provider::new(&test_cfg(url), None, None).unwrap();
        let msg = p
            .chat(&[ApiMessage {
                role: "user".into(),
                content: "hi".into(),
                tool_call_id: None,
                tool_calls: Vec::new(),
            }])
            .unwrap();
        assert_eq!(msg.tool_calls.len(), 1);
        assert_eq!(msg.tool_calls[0].name, "read");
        assert_eq!(msg.tool_calls[0].arguments, "{\"path\":\"a.py\"}");
        assert_eq!(msg.usage.unwrap().completion_tokens, 5);
    }

    /// Serve a response in TWO chunked writes with a gap, so streaming reads
    /// really arrive in stages (and a cancel can land between them).
    fn serve_staged(first: &str, second: &str, gap_ms: u64) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let first_owned: String = first.to_string();
        let second_owned: String = second.to_string();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut drain: [u8; 8192] = [0; 8192];
            let _ = stream.read(&mut drain);
            let head = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
            let part1 = format!("{:x}\r\n{}\r\n", first_owned.len(), first_owned);
            let part2 = format!("{:x}\r\n{}\r\n0\r\n\r\n", second_owned.len(), second_owned);
            stream.write(head.as_bytes()).ok();
            stream.write(part1.as_bytes()).ok();
            stream.flush().ok();
            std::thread::sleep(Duration::from_millis(gap_ms));
            stream.write(part2.as_bytes()).ok();
            stream.flush().ok();
        });
        format!("http://127.0.0.1:{}", port)
    }

    fn sse_cfg(url: String) -> config::Config {
        let mut cfg = test_cfg(url);
        cfg.providers[0].stream = true;
        cfg
    }

    #[test]
    fn chat_stream_delivers_deltas_then_the_full_body() {
        let mut sse = String::new();
        sse.push_str("data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n");
        sse.push_str("data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n");
        sse.push_str("data: [DONE]\n");
        let url = serve_staged(
            &sse[0..sse.len() - 12].to_string(), // first two lines
            &sse[sse.len() - 12..].to_string(), // "[DONE]\n"
            30,
        );
        let p = Provider::new(&sse_cfg(url), None, None).unwrap();
        let cs = p
            .chat_stream(&[ApiMessage {
                role: "user".into(),
                content: "hi".into(),
                tool_call_id: None,
                tool_calls: Vec::new(),
            }])
            .unwrap();
        let mut kinds: Vec<String> = vec![];
        let mut deltas: Vec<String> = vec![];
        let mut full = String::new();
        loop {
            let ch = cs.wait(2000);
            match ch {
                None => break,
                Some(c) => {
                    kinds.push(c.kind.clone());
                    if c.kind == "delta" {
                        deltas.push(c.text.clone());
                    }
                    if c.kind == "done" {
                        full = c.text;
                        break;
                    }
                }
            }
        }
        assert_eq!(kinds.iter().filter(|k| k.as_str() == "delta").count() >= 2, true);
        assert_eq!(deltas.join(""), "Hello");
        assert_eq!(assemble_sse(full.as_str()).unwrap().content.unwrap(), "Hello");
    }

    #[test]
    fn chat_stream_plain_mode_parses_content_length_bodies() {
        // serve() writes a single Content-Length body — the plain (non-SSE)
        // path of the reader must deliver it byte-exact for the JSON parser.
        let body = "{\"choices\":[{\"message\":{\"role\":\"assistant\",\"content\":\"hello there\"}}],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":2}}";
        let url = serve(body, 200);
        let p = Provider::new(&test_cfg(url), None, None).unwrap();
        let cs = p
            .chat_stream(&[ApiMessage {
                role: "user".into(),
                content: "hi".into(),
                tool_call_id: None,
                tool_calls: Vec::new(),
            }])
            .unwrap();
        let mut full = String::new();
        loop {
            match cs.wait(2000) {
                None => break,
                Some(c) => {
                    if c.kind == "done" {
                        full = c.text;
                        break;
                    }
                }
            }
        }
        assert_eq!(
            full.len(),
            body.len(),
            "reader must deliver the exact body: {full}"
        );
        assert_eq!(parse_chat_json(full.as_str()).unwrap().content.unwrap(), "hello there");
    }

    #[test]
    fn chat_stream_cancel_aborts_before_the_second_burst() {
        let mut sse = String::new();
        sse.push_str("data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n");
        sse.push_str("data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n");
        sse.push_str("data: [DONE]\n");
        let half = sse.len() / 2;
        let url = serve_staged(&sse[0..half].to_string(), &sse[half..].to_string(), 400);
        let p = Provider::new(&sse_cfg(url), None, None).unwrap();
        let cs = p
            .chat_stream(&[ApiMessage {
                role: "user".into(),
                content: "hi".into(),
                tool_call_id: None,
                tool_calls: Vec::new(),
            }])
            .unwrap();
        // info arrives first
        assert_eq!(cs.wait(2000).unwrap().kind, "info");
        // wait for the first burst's delta, then cancel before the second
        let mut saw_delta = false;
        for _ in 0..200 {
            match cs.wait(25) {
                None => continue,
                Some(c) => {
                    if c.kind == "delta" {
                        saw_delta = true;
                        break;
                    }
                    if c.kind == "done" || c.kind == "error" {
                        break;
                    }
                }
            }
        }
        assert_eq!(saw_delta, true, "first burst rendered before cancel");
        cs.cancel();
        let mut saw_cancelled = false;
        let mut saw_done = false;
        for _ in 0..300 {
            match cs.wait(25) {
                None => continue,
                Some(c) => {
                    if c.kind == "cancelled" {
                        saw_cancelled = true;
                        break;
                    }
                    if c.kind == "done" {
                        saw_done = true;
                        break;
                    }
                }
            }
        }
        assert_eq!(saw_cancelled, true, "cancel wins over the pending body");
        assert_eq!(saw_done, false, "the second burst never gets consumed");
    }

    #[test]
    fn chat_plain_text_message() {
        let body = "{\"choices\":[{\"message\":{\"role\":\"assistant\",\"content\":\"hello there\"}}],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":2}}";
        let url = serve(body, 200);
        let p = Provider::new(&test_cfg(url), None, None).unwrap();
        let msg = p
            .chat(&[ApiMessage {
                role: "user".into(),
                content: "hi".into(),
                tool_call_id: None,
                tool_calls: Vec::new(),
            }])
            .unwrap();
        assert_eq!(msg.content.unwrap(), "hello there");
        assert_eq!(msg.tool_calls.len(), 0);
        assert_eq!(msg.usage.unwrap().prompt_tokens, 4);
    }

    #[test]
    fn chat_stream_assembles_chunks_and_usage() {
        let mut sse = String::new();
        sse.push_str("data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n");
        sse.push_str("data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n");
        sse.push_str("data: {\"choices\":[{\"delta\":{\"content\":null,\"tool_calls\":[{\"index\":0,\"id\":\"call_2\",\"function\":{\"name\":\"write\",\"arguments\":\"{\\\"path\\\":\\\"b.txt\\\",\\\"content\\\":\\\"x\\\"}\"}}]}}]}\n");
        sse.push_str("data: {\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":9}}\n");
        sse.push_str("data: [DONE]\n");
        let url = serve(sse.as_str(), 200);
        let mut cfg = test_cfg(url);
        cfg.providers[0].stream = true;
        let p = Provider::new(&cfg, None, None).unwrap();
        let msg = p
            .chat(&[ApiMessage {
                role: "user".into(),
                content: "hi".into(),
                tool_call_id: None,
                tool_calls: Vec::new(),
            }])
            .unwrap();
        assert_eq!(msg.content.unwrap(), "Hello");
        assert_eq!(msg.tool_calls.len(), 1);
        assert_eq!(msg.tool_calls[0].name, "write");
        assert_eq!(msg.usage.unwrap().prompt_tokens, 12);
    }

    #[test]
    fn provider_down_is_loud() {
        let url: String = "http://127.0.0.1:1".into(); // nothing listening
        let p = Provider::new(&test_cfg(url.clone()), None, None).unwrap();
        let res = p.chat(&[ApiMessage {
            role: "user".into(),
            content: "hi".into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
        }]);
        assert!(res.is_err());
        let e = res.unwrap_err();
        assert!(
            e.msg.contains("provider unreachable"),
            "loud provider error"
        );
        assert!(e.msg.contains(url.as_str()), "error names the base_url");
    }

    #[test]
    fn malformed_body_is_loud() {
        let url = serve("this is not json", 200);
        let p = Provider::new(&test_cfg(url), None, None).unwrap();
        let res = p.chat(&[ApiMessage {
            role: "user".into(),
            content: "hi".into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
        }]);
        assert!(res.is_err());
        assert!(res.unwrap_err().msg.contains("non-JSON"), "non-JSON error");
    }

    #[test]
    fn http_error_status_is_loud() {
        let url = serve("{\"error\":\"bad key\"}", 401);
        let p = Provider::new(&test_cfg(url), None, None).unwrap();
        let res = p.chat(&[ApiMessage {
            role: "user".into(),
            content: "hi".into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
        }]);
        assert!(res.is_err());
        assert!(res.unwrap_err().msg.contains("401"), "status code surfaced");
    }

    #[test]
    fn resolve_provider_by_name_and_default() {
        let cfg = config::Loader::new(None, None).load().unwrap();
        // defaults ship ollama + openrouter; default_provider = openrouter
        let p = resolve_provider_cfg(&cfg, None).unwrap();
        assert_eq!(p.name, "openrouter");
        let o = resolve_provider_cfg(&cfg, Some("ollama")).unwrap();
        assert_eq!(o.name, "ollama");
        assert_eq!(o.base_url, "http://localhost:11434/v1");
        let bad = resolve_provider_cfg(&cfg, Some("nope"));
        assert!(bad.is_err());
    }

    #[test]
    fn invalid_scheme_is_rejected_at_construction() {
        let cfg = test_cfg("file:///etc/passwd".into());
        let p = Provider::new(&cfg, None, None);
        assert!(p.is_err());
        let _ = cfg;
    }

    #[test]
    fn key_resolves_from_env() {
        std::env::set_var("CURAM_TEST_KEY", "sekret");
        let mut cfg = test_cfg("http://127.0.0.1:1".into());
        cfg.providers[0].api_key_env = "CURAM_TEST_KEY".into();
        let p = Provider::new(&cfg, None, None).unwrap();
        assert_eq!(p.api_key.as_deref(), Some("sekret"));
        std::env::remove_var("CURAM_TEST_KEY");
    }
}
