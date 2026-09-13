//! provider.rs — one OpenAI-compatible chat-completions client (ureq).
//!
//! Covers Ollama, LiteLLM, vLLM, llama.cpp, and cloud APIs that speak the
//! same /chat/completions shape, ported from provider.py: any base_url,
//! `api_key_env` pointing at an env var (falling back to pi's credential
//! store ~/.pi/agent/auth.json), `usage` surfaced for the budget, and — new
//! in the Rust phase — SSE streaming (`stream: true`) assembled into the
//! same ChatMessage shape so the agent loop is mode-agnostic.

use serde_json::Value;
use std::path::PathBuf;
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
}

/// A tool call the model asked for (arguments arrive as a JSON string; the
/// agent parses them like provider.py's json.loads).
#[derive(Debug)]
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
        Self { content: Some(content.into()), tool_calls: Vec::new(), usage: None }
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
        return Err(ProviderError::new("no providers configured (add [providers.*] to curam.toml)".into()));
    }
    let name = provider_name.unwrap_or(cfg.default_provider.as_str());
    for p in &cfg.providers {
        if p.name == name {
            return Ok(p.clone());
        }
    }
    Err(ProviderError::new(format!("unknown provider {:?} (configured: {})", name, cfg.providers.iter().map(|p| p.name.clone()).collect::<Vec<_>>().join(", "))))
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
    let entry = doc
        .get(provider_name)
        .and_then(|v| v.as_object());
    let key = entry
        .and_then(|e| e.get("key").and_then(|k| k.as_str()))
        .map(|k| k.to_string());
    match key {
        Some(k) if !k.is_empty() => Some(k),
        _ => None,
    }
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
            return Err(ProviderError::new(format!("provider {:?} has no base_url", p.name)));
        }
        if !p.base_url.starts_with("http://") && !p.base_url.starts_with("https://") {
            return Err(ProviderError::new(
                format!("unsupported base_url scheme {:?} (http/https only)", p.base_url),
            ));
        }
        let base_url = p.base_url.trim_end_matches('/').to_string();
        let model = model_override.unwrap_or(p.model.as_str()).to_string();
        let api_key = if p.api_key_env.is_empty() {
            None
        } else {
            std::env::var_os(p.api_key_env.as_str())
                .map(|v| std::path::PathBuf::from(v).to_str().unwrap_or_default().to_string())
                .or_else(|| key_from_pi_auth(p.name.as_str()))
        };
        let timeout = if p.timeout_s > 0.0 { p.timeout_s } else { 120.0 };
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
        let mut req = self.agent.post(url).header("Content-Type", "application/json");
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
            Err(e) => return Err(ProviderError::new(format!("provider read error ({}): {}", self.base_url, e))),
        };
        if !(200..300).contains(&status) {
            return Err(ProviderError::new(format!(
                "provider error {} ({}): {}",
                status, self.base_url, &text[0..SNIPPET_CHARS]
            )));
        }
        if self.stream {
            assemble_sse(text.as_str())
        } else {
            parse_chat_json(text.as_str())
        }
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
fn parse_chat_json(text: &str) -> Result<ChatMessage, ProviderError> {
    let doc: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => return Err(ProviderError::new(format!("non-JSON provider response: {e}"))),
    };
    let msg = doc.get("choices").and_then(|c| c.as_array()).and_then(|arr| arr.first()).and_then(|ch| ch.get("message"));
    let msg = match msg {
        Some(m) => m,
        None => {
            return Err(ProviderError::new(format!(
                "malformed provider response: {}",
                &text[0..SNIPPET_CHARS]
            )));
        }
    };
    let content = msg.get("content").and_then(|c| c.as_str()).map(|s| s.to_string());
    let mut tool_calls = Vec::new();
    if let Some(arr) = msg.get("tool_calls").and_then(|t| t.as_array()) {
        for tc in arr {
            let id = tc.get("id").and_then(|i| i.as_str()).unwrap_or_default();
            let name = tc.get("function").and_then(|f| f.as_object()).and_then(|o| o.get("name")).and_then(|n| n.as_str()).unwrap_or_default();
            let args = tc.get("function").and_then(|f| f.as_object()).and_then(|o| o.get("arguments")).and_then(|a| a.as_str()).unwrap_or_default();
            tool_calls.push(ToolCall { id: id.to_string(), name: name.to_string(), arguments: args.to_string() });
        }
    }
    let usage = usage_of(doc.get("usage"));
    Ok(ChatMessage { content, tool_calls, usage })
}

fn usage_of(v: Option<&Value>) -> Option<Usage> {
    v.and_then(|u| {
        let pt = u.get("prompt_tokens").and_then(|x| x.as_u64());
        let ct = u.get("completion_tokens").and_then(|x| x.as_u64());
        if pt.is_some() && ct.is_some() {
            Some(Usage { prompt_tokens: pt.unwrap(), completion_tokens: ct.unwrap() })
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
fn assemble_sse(text: &str) -> Result<ChatMessage, ProviderError> {
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
        if let Some(u) = usage_of(chunk.get("usage")) { usage = Some(u) }
        let delta = chunk.get("choices").and_then(|c| c.as_array()).and_then(|a| a.first()).and_then(|ch| ch.get("delta"));
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
                    tool_calls.push(ToolCall { id: String::new(), name: String::new(), arguments: String::new() });
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
    Ok(ChatMessage { content, tool_calls, usage })
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
        let msg = p.chat(&[ApiMessage { role: "user".into(), content: "hi".into(), tool_call_id: None }]).unwrap();
        assert_eq!(msg.tool_calls.len(), 1);
        assert_eq!(msg.tool_calls[0].name, "read");
        assert_eq!(msg.tool_calls[0].arguments, "{\"path\":\"a.py\"}");
        assert_eq!(msg.usage.unwrap().completion_tokens, 5);
    }

    #[test]
    fn chat_plain_text_message() {
        let body = "{\"choices\":[{\"message\":{\"role\":\"assistant\",\"content\":\"hello there\"}}],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":2}}";
        let url = serve(body, 200);
        let p = Provider::new(&test_cfg(url), None, None).unwrap();
        let msg = p.chat(&[ApiMessage { role: "user".into(), content: "hi".into(), tool_call_id: None }]).unwrap();
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
        let msg = p.chat(&[ApiMessage { role: "user".into(), content: "hi".into(), tool_call_id: None }]).unwrap();
        assert_eq!(msg.content.unwrap(), "Hello");
        assert_eq!(msg.tool_calls.len(), 1);
        assert_eq!(msg.tool_calls[0].name, "write");
        assert_eq!(msg.usage.unwrap().prompt_tokens, 12);
    }

    #[test]
    fn provider_down_is_loud() {
        let url: String = "http://127.0.0.1:1".into(); // nothing listening
        let p = Provider::new(&test_cfg(url.clone()), None, None).unwrap();
        let res = p.chat(&[ApiMessage { role: "user".into(), content: "hi".into(), tool_call_id: None }]);
        assert!(res.is_err());
        let e = res.unwrap_err();
        assert!(e.msg.contains("provider unreachable"), "loud provider error");
        assert!(e.msg.contains(url.as_str()), "error names the base_url");
    }

    #[test]
    fn malformed_body_is_loud() {
        let url = serve("this is not json", 200);
        let p = Provider::new(&test_cfg(url), None, None).unwrap();
        let res = p.chat(&[ApiMessage { role: "user".into(), content: "hi".into(), tool_call_id: None }]);
        assert!(res.is_err());
        assert!(res.unwrap_err().msg.contains("non-JSON"), "non-JSON error");
    }

    #[test]
    fn http_error_status_is_loud() {
        let url = serve("{\"error\":\"bad key\"}", 401);
        let p = Provider::new(&test_cfg(url), None, None).unwrap();
        let res = p.chat(&[ApiMessage { role: "user".into(), content: "hi".into(), tool_call_id: None }]);
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