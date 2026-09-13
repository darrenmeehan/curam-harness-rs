//! agent.rs — the loop: messages → provider → tools → repeat (port of
//! agent.py). Journals every step; policy gates exec (today: file tools
//! only — shell/run/ask land in H4, skills in H6). The provider boundary is
//! scrubbed by the sanitizer; the journal keeps the RAW result (invariant:
//! no secrets cross the provider boundary).

use serde_json::Value;
use std::path::PathBuf;
use std::sync::OnceLock;

use regex::Regex;

use super::budget::Budget;
use super::config;
use super::journal::Journal;
use super::provider::{ApiMessage, Provider, Usage};
use super::sanitize::Sanitizer;
use super::tools as toolmod;

pub const MAX_ITERATIONS: usize = 25;
pub const STUCK_THRESHOLD: usize = 3;

pub const SYSTEM_PROMPT: &str = "You are curam, a coding agent working inside the project\n\
directory. You can read, write, edit files, and run shell commands.\n\n\
Rules:\n\
1. Prefer doing over asking. Work autonomously toward the task.\n\
2. Use ask_human ONLY for genuinely human judgment: design forks, ambiguous\n\
   or conflicting requirements, irreversible changes, choices about intent or\n\
   cost, or after 3 failed attempts at the same thing. Never ask for routine\n\
   details.\n\
3. Commands run in the workdir. Prefer safe, targeted commands. If a command\n\
   seems destructive (rm, dd, chmod, git push, ...), the harness will prompt\n\
   the human for approval itself — do not be surprised by that.\n\
4. Report progress in short messages between tool calls.\n\
5. Never claim a tool result you did not receive. If a tool errors, read the\n\
   error and retry with a fix.\n\
6. If the user greets you or makes small talk with NO actionable task, reply\n\
   in one or two sentences and do NOT call any tools.";

const GREETING_REPLY: &str = "Hey! I'm curam, your coding agent in this project. \
Give me a task (e.g. \"add a README to this dir\") and I'll get on it.";

/// Tiny deterministic guard (not the model) so a bare "hello" never burns a
/// provider round-trip or triggers tools — the POC proves this behaviorally.
fn greeting_re() -> &'static Regex {
    static G: OnceLock<Regex> = OnceLock::new();
    G.get_or_init(|| {
        Regex::new("(?i)^(?:hi|hello|hey|yo|howdy|hola|sup|good (?:morning|afternoon|evening)|what'?s up|how are (?:you|things))(?:\\s+there)?[!?.\\s]*$")
            .expect("greeting regex")
    })
}

pub fn is_greeting(task: &str) -> bool {
    greeting_re().is_match(task.trim())
}

fn round4(x: f64) -> f64 {
    (x * 10_000.0).round() / 10_000.0
}

fn system_prompt(cfg: &config::Config) -> String {
    if cfg.agent.system_prompt.is_empty() {
        SYSTEM_PROMPT.into()
    } else {
        cfg.agent.system_prompt.clone()
    }
}

fn usage_value(u: &Usage) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("prompt_tokens".to_owned(), u.prompt_tokens.into());
    m.insert("completion_tokens".to_owned(), u.completion_tokens.into());
    serde_json::Value::Object(m)
}

fn parse_tool_args(raw: &str) -> Value {
    let parsed: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => serde_json::Value::Object(serde_json::Map::new()),
    };
    if parsed.as_object().is_some() {
        parsed
    } else {
        serde_json::Value::Object(serde_json::Map::new())
    }
}

fn ok_false(error: &str) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("ok".to_owned(), false.into());
    m.insert("error".to_owned(), error.into());
    serde_json::Value::Object(m)
}

#[derive(Debug)]
pub struct Agent {
    pub workdir: PathBuf,
    provider: Provider,
    budget: Budget,
    sanitizer: Sanitizer,
    pub interactive: bool,
    pub show_thinking: bool,
    pub show_commands: bool,
    pub auto_grant: bool,
    messages: Vec<ApiMessage>,
}

impl Agent {
    pub fn new(
        cfg: &config::Config,
        provider: Provider,
        budget: Budget,
        workdir: PathBuf,
        interactive: bool,
        show_thinking: bool,
        show_commands: bool,
        auto_grant: bool,
    ) -> Self {
        Self {
            workdir,
            provider,
            budget,
            sanitizer: Sanitizer::from_cfg(&cfg.pii),
            interactive,
            show_thinking,
            show_commands,
            auto_grant,
            messages: vec![ApiMessage {
                role: "system".into(),
                content: system_prompt(cfg),
                tool_call_id: None,
                tool_calls: Vec::new(),
            }],
        }
    }

    /// Run one task to completion (or the iteration cap), journaling every
    /// step. Returns the final answer (None when the run bailed on a
    /// provider error).
    pub fn run_task(&mut self, task: &str, journal: &mut Journal) -> Result<Option<String>, String> {
        journal.log("user_message", &Value::Object(mapof("text", task.into())))?;
        self.messages.push(ApiMessage {
            role: "user".into(),
            content: task.into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
        });
        if is_greeting(task) {
            self.messages.push(ApiMessage {
                role: "assistant".into(),
                content: GREETING_REPLY.into(),
                tool_call_id: None,
                tool_calls: Vec::new(),
            });
            journal.log("model_message", &Value::Object(mapof("text", GREETING_REPLY.into())))?;
            return Ok(Some(GREETING_REPLY.into()));
        }

        let mut last_tool: Option<(String, String)> = None;
        let mut identical_streak = 0;
        for _ in 0..MAX_ITERATIONS {
            if self.budget.over_budget() {
                let spent = round4(self.budget.spent_usd);
                let cap = round4(self.budget.cap_usd);
                let msg = format!("Budget exhausted (${} / ${}). Halting.", spent, cap);
                journal.log("budget_hit", &Value::Object(mapof("msg", msg.clone().into())))?;
                return Ok(Some(msg));
            }

            let resp = self.provider.chat(&self.messages);
            match resp {
                Err(e) => {
                    journal.log("error", &Value::Object(mapof("error", e.msg.into())))?;
                    return Ok(None);
                }
                Ok(resp) => {
                    let usage_v = match &resp.usage {
                        Some(u) => usage_value(u),
                        None => serde_json::Value::Null,
                    };
                    let cost = self.budget.charge(self.provider.model.as_str(), &usage_v, journal).unwrap_or(0.0);
                    journal.log("model_response", &Value::Object(mapof2(
                        "model", self.provider.model.as_str().into(),
                        "cost", cost.into(),
                    )))?;

                    if resp.tool_calls.is_empty() {
                        let answer = resp.content.unwrap_or_default();
                        self.messages.push(ApiMessage {
                            role: "assistant".into(),
                            content: answer.clone(),
                            tool_call_id: None,
                            tool_calls: Vec::new(),
                        });
                        journal.log("model_message", &Value::Object(mapof("text", answer.clone().into())))?;
                        return Ok(Some(answer));
                    }

                    // interim thinking — a glimpse while working
                    if self.show_thinking {
                        if let Some(c) = &resp.content {
                            println!("\x1b[90m{}\x1b[0m", c);
                        }
                    }

                    for tc in &resp.tool_calls {
                        let args = parse_tool_args(tc.arguments.as_str());
                        if self.show_commands {
                            println!("\x1b[36m▶ {} {}\x1b[0m", tc.name, serde_json::to_string(&args).expect("args json"));
                        }
                        journal.log("tool_call", &Value::Object(mapof2(
                            "name", tc.name.clone().into(),
                            "args", args.clone(),
                        )))?;

                        let result = self.execute_tool(tc.name.as_str(), &args, journal);
                        journal.log("tool_result", &Value::Object(mapof2(
                            "name", tc.name.clone().into(),
                            "result", result.clone(),
                        )))?;
                        if self.show_commands {
                            println!("\x1b[37m{}\x1b[0m", serde_json::to_string(&result).expect("result json"));
                        }

                        // stuck detection: N identical consecutive calls
                        let cur = (tc.name.clone(), serde_json::to_string(&args).expect("args ser"));
                        if let Some(prev) = &last_tool {
                            if *prev == cur {
                                identical_streak += 1;
                            } else {
                                identical_streak = 1;
                            }
                        } else {
                            identical_streak = 1;
                        }
                        last_tool = Some(cur);
                        if identical_streak >= STUCK_THRESHOLD && self.interactive {
                            // full ask UX lands in H4; journal the nudge for now
                            journal.log("ask_human", &Value::Object(mapof2(
                                "kind", "stuck".into(),
                                "attempts", serde_json::Value::Array(vec![Value::Object(mapof(
                                    "name", tc.name.clone().into(),
                                ))]),
                            )))?;
                        }

                        // The model context gets the SCRUBBED result; the
                        // journal keeps the RAW one (PII invariant).
                        let raw_json = serde_json::to_string(&result).expect("result json");
                        let scrubbed = self.sanitizer.scrub(raw_json.as_str());
                        let capped = if scrubbed.len() > 4_000 {
                            scrubbed[0..4_000].to_string()
                        } else {
                            scrubbed
                        };
                        self.messages.push(ApiMessage {
                            role: "tool".into(),
                            content: capped,
                            tool_call_id: Some(tc.id.clone()),
                            tool_calls: Vec::new(),
                        });
                    }

                    // assistant continuation, replaying its tool_calls so
                    // strict servers see the full turn (POC omitted this;
                    // required by the OpenAI tool protocol).
                    self.messages.push(ApiMessage {
                        role: "assistant".into(),
                        content: resp.content.clone().unwrap_or_else(|| "[continued tool use]".into()),
                        tool_call_id: None,
                        tool_calls: resp.tool_calls.clone(),
                    });
                }
            }
        }
        let msg = format!("Stopped after {} iterations (cap).", MAX_ITERATIONS);
        journal.log("error", &Value::Object(mapof("msg", msg.clone().into())))?;
        Ok(Some(msg))
    }

    /// Tool dispatch with policy gating — the only exec path (H3: file tools;
    /// shell/run/ask/skills land in later phases).
    fn execute_tool(&mut self, name: &str, args: &Value, journal: &mut Journal) -> Value {
        let _ = journal; // ask/verdict journaling lands in H4
        match name {
            "read" | "write" | "edit" => toolmod::run(&self.workdir, name, args),
            "shell" => ok_false("shell lands in H4 (caged) — not available yet"),
            "run" => ok_false("typed run lands in H4 — not available yet"),
            "ask_human" => ok_false("ask layer lands in H4 — not available yet"),
            "use_skill" => ok_false("skills land in H6 — not available yet"),
            _ => toolmod::run(&self.workdir, name, args),
        }
    }
}

pub fn mapof(k: &str, v: Value) -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    m.insert(k.to_owned(), v);
    m
}

pub fn mapof2(k1: &str, v1: Value, k2: &str, v2: Value) -> serde_json::Map<String, Value> {
    let mut m = mapof(k1, v1);
    m.insert(k2.to_owned(), v2);
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    fn test_cfg(base_url: String) -> config::Config {
        let mut cfg = config::Loader::new(None, None).load().unwrap();
        cfg.providers = vec![config::ProviderCfg {
            name: "fixture".into(),
            base_url: base_url.into(),
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

    /// Serve `responses` in order on an ephemeral loopback port, writing
    /// every request BODY it saw (after the header terminator) to `capture`.
    fn scripted(responses: Vec<String>, capture: &std::path::Path) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let cap_owned: std::path::PathBuf = std::path::PathBuf::from(capture);
        thread::spawn(move || {
            for body in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf: [u8; 8192] = [0; 8192];
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(60)));
                let mut total = 0;
                loop {
                    match stream.read(&mut buf[total..]) {
                        Ok(0) => break,
                        Ok(n2) => {
                            total += n2;
                            if total >= buf.len() {
                                break;
                            }
                        }
                        Err(_) => break, // timed out / closed — capture what arrived
                    }
                }
                // Overwrite with the raw request; the LAST capture is the
                // request carrying the (scrubbed) tool result.
                let _ = std::fs::write(&cap_owned, &buf[0..total]);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(), body
                );
                stream.write(response.as_bytes()).ok();
                stream.flush().ok();
            }
        });
        format!("http://127.0.0.1:{}", port)
    }

    fn agent_for(cfg: &config::Config, workdir: &std::path::Path) -> Agent {
        let p = Provider::new(cfg, None, None).unwrap();
        Agent::new(
            cfg,
            p,
            Budget::from_cfg(&cfg.budget),
            PathBuf::from(workdir),
            false, // non-interactive (no asks in H3)
            false,
            false,
            false,
        )
    }

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("curam-h3-a-{tag}")).join(format!("{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn greeting_never_calls_the_provider() {
        let cfg = test_cfg("http://127.0.0.1:1".into()); // nothing listens there
        let dir = tmp_dir("greet");
        let mut a = agent_for(&cfg, &dir);
        let mut j = Journal::open(dir.join("j.db"), Some("sess".into())).unwrap();
        let ans = a.run_task("hello!", &mut j).unwrap();
        assert_eq!(ans, Some(GREETING_REPLY.into()));
        let evts = j.events(None).unwrap();
        assert_eq!(evts.len(), 2); // user_message + model_message, NO provider call
        assert_eq!(j.events(Some("model_response")).unwrap().len(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn agent_completes_a_task_and_scrubs_the_context() {
        let dir = tmp_dir("flow");
        // The fixture file the agent will read — its content carries the secret.
        let secret_file = dir.join("secret-notes.txt");
        std::fs::write(
            &secret_file,
            "planted sk-or-deadbeefdeadbeefdeadbeef used here".as_bytes(),
        ).unwrap();
        let capture = dir.join("bodies.txt");
        let read_call = r#"{"choices":[{"message":{"role":"assistant","content":"Let me read that.","tool_calls":[{"id":"call_1","type":"function","function":{"name":"read","arguments":"{\"path\":\"secret-notes.txt\"}"}}]}}],"usage":{"prompt_tokens":8,"completion_tokens":6}}"#;
        let answer = r#"{"choices":[{"message":{"role":"assistant","content":"Done — read secret-notes.txt."}}],"usage":{"prompt_tokens":20,"completion_tokens":4}}"#;
        let url = scripted(vec![read_call.into(), answer.into()], &capture);
        let cfg = test_cfg(url);
        let mut a = agent_for(&cfg, &dir);
        let mut j = Journal::open(dir.join("j.db"), Some("sess".into())).unwrap();

        let ans = a.run_task("read secret-notes.txt", &mut j).unwrap();
        assert_eq!(ans, Some("Done — read secret-notes.txt.".into()));

        // JOURNAL keeps the RAW secret (the read result carries the content)...
        let trs = j.events(Some("tool_result")).unwrap();
        assert_eq!(trs.len(), 1);
        assert!(
            serde_json::to_string(&trs[0].payload).expect("json").contains("deadbeef"),
            "journal keeps the raw result"
        );

        // ...but the model context (request bodies) is SCRUBBED.
        let captured = std::fs::read_to_string(&capture).unwrap();
        assert!(
            !captured.contains("deadbeef"),
            "secret must not cross the provider boundary: {captured}"
        );
        assert!(captured.contains("{REDACTED}:openrouter"), "context scrubbed");

        // Full journal chain: user_message, tool_call, tool_result, model_message.
        const WANTED: &[&str] = &["user_message", "tool_call", "tool_result", "model_message"];
        let kinds = j.events(None).unwrap().iter().map(|e| e.etype.clone()).collect::<Vec<_>>();
        for want in WANTED {
            let found = kinds.iter().any(|k| k == want);
            assert!(found, "missing journal event {want}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn iteration_cap_stops_runaway_policy() {
        let dir = tmp_dir("cap");
        let call = r#"{"choices":[{"message":{"role":"assistant","tool_calls":[{"id":"c","type":"function","function":{"name":"read","arguments":"{\"path\":\"x.py\"}"}}]}}],"usage":{"prompt_tokens":3,"completion_tokens":3}}"#;
        let mut responses = Vec::new();
        for _ in 0..MAX_ITERATIONS {
            responses.push(call.into());
        }
        responses.push(call.into()); // a bit of slack for the cap math
        let url = scripted(responses, &dir.join("bodies.txt"));
        let cfg = test_cfg(url);
        let mut a = agent_for(&cfg, &dir);
        let mut j = Journal::open(dir.join("j.db"), Some("sess".into())).unwrap();
        let ans = a.run_task("loop", &mut j).unwrap();
        assert!(ans.clone().unwrap().contains("Stopped after 25 iterations"), "{ans:?}");
        let errs = j.events(Some("error")).unwrap();
        assert!(errs.iter().any(|e| e.payload.get("msg").and_then(|m| m.as_str()).unwrap_or_default().contains("iterations")));
        std::fs::remove_dir_all(&dir).ok();
    }
}