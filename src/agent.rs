//! agent.rs — the loop: messages → provider → tools → repeat (port of
//! agent.py). Journals every step; policy gates exec (today: file tools
//! only — shell/run/ask land in H4, skills in H6). The provider boundary is
//! scrubbed by the sanitizer; the journal keeps the RAW result (invariant:
//! no secrets cross the provider boundary).

use serde_json::Value;
use std::path::PathBuf;
use std::sync::OnceLock;

use regex::Regex;

use super::ask;
use super::budget::Budget;
use super::cage::Cage;
use super::config;
use super::config::{PolicyRulesCfg, RunCfg};
use super::journal::Journal;
use super::policy;
use super::provider::{assemble_sse, parse_chat_json};
use super::provider::{ApiMessage, ChatMessage, Provider, ProviderError, Usage};
use super::sanitize::Sanitizer;
use super::tools as toolmod;
use super::ui;
use super::util::{mapof, mapof2};

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
    cage: Cage,
    budget: Budget,
    policy_rules: PolicyRulesCfg,
    run_cfg: RunCfg,
    fallback_raw: bool,
    sanitizer: Sanitizer,
    pub interactive: bool,
    pub show_thinking: bool,
    pub show_commands: bool,
    pub auto_grant: bool,
    /// When the TUI runs the agent, this bus streams events out and routes
    /// ask replies back in (None = one-shot/REPL plain prints).
    pub ui_bus: Option<ui::UiBus>,
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
            cage: Cage::from_cfg(&cfg.cage),
            budget,
            policy_rules: cfg.policy_rules.clone(),
            run_cfg: cfg.run.clone(),
            fallback_raw: cfg.cage.fallback == "raw",
            sanitizer: Sanitizer::from_cfg(&cfg.pii),
            interactive,
            show_thinking,
            show_commands,
            auto_grant,
            ui_bus: None,
            messages: vec![ApiMessage {
                role: "system".into(),
                content: system_prompt(cfg),
                tool_call_id: None,
                tool_calls: Vec::new(),
            }],
        }
    }

    /// Status accessors for the REPL/status lines (the agent owns the
    /// provider + budget instances).
    pub fn provider_name(&self) -> &str {
        self.provider.name.as_str()
    }

    pub fn model(&self) -> &str {
        self.provider.model.as_str()
    }

    pub fn spend(&self) -> f64 {
        self.budget.spent_usd
    }

    pub fn cap(&self) -> f64 {
        self.budget.cap_usd
    }

    /// Run one task to completion (or the iteration cap), journaling every
    /// step. Returns the final answer (None when the run bailed on a
    /// provider error).
    pub fn run_task(
        &mut self,
        task: &str,
        journal: &mut Journal,
    ) -> Result<Option<String>, String> {
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
            journal.log(
                "model_message",
                &Value::Object(mapof("text", GREETING_REPLY.into())),
            )?;
            return Ok(Some(GREETING_REPLY.into()));
        }

        let mut last_tool: Option<(String, String)> = None;
        let mut identical_streak = 0;
        for _ in 0..MAX_ITERATIONS {
            if self.budget.over_budget() {
                let spent = round4(self.budget.spent_usd);
                let cap = round4(self.budget.cap_usd);
                let msg = format!("Budget exhausted (${} / ${}). Halting.", spent, cap);
                journal.log(
                    "budget_hit",
                    &Value::Object(mapof("msg", msg.clone().into())),
                )?;
                return Ok(Some(msg));
            }

            let resp = self.chat_with_stream();
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
                    let cost = self
                        .budget
                        .charge(self.provider.model.as_str(), &usage_v, journal)
                        .unwrap_or(0.0);
                    journal.log(
                        "model_response",
                        &Value::Object(mapof2(
                            "model",
                            self.provider.model.as_str().into(),
                            "cost",
                            cost.into(),
                        )),
                    )?;
                    if let Some(b) = &self.ui_bus {
                        let spent = round4(self.budget.spent_usd);
                        let _ = b.send(
                            "status",
                            format!("cost=${:.2} spent=${}", cost, spent)
                                .to_string()
                                .as_str(),
                        );
                    }

                    if resp.tool_calls.is_empty() {
                        let answer = resp.content.unwrap_or_default();
                        self.messages.push(ApiMessage {
                            role: "assistant".into(),
                            content: answer.clone(),
                            tool_call_id: None,
                            tool_calls: Vec::new(),
                        });
                        journal.log(
                            "model_message",
                            &Value::Object(mapof("text", answer.clone().into())),
                        )?;
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
                            println!(
                                "\x1b[36m▶ {} {}\x1b[0m",
                                tc.name,
                                serde_json::to_string(&args).expect("args json")
                            );
                        }
                        if let Some(b) = &self.ui_bus {
                            let _ = b.send(
                                "tool",
                                format!(
                                    "{} {}",
                                    tc.name,
                                    serde_json::to_string(&args).expect("args json")
                                )
                                .to_string()
                                .as_str(),
                            );
                        }
                        journal.log(
                            "tool_call",
                            &Value::Object(mapof2(
                                "name",
                                tc.name.clone().into(),
                                "args",
                                args.clone(),
                            )),
                        )?;

                        let result = self.execute_tool(tc.name.as_str(), &args, journal);
                        journal.log(
                            "tool_result",
                            &Value::Object(mapof2(
                                "name",
                                tc.name.clone().into(),
                                "result",
                                result.clone(),
                            )),
                        )?;
                        if self.show_commands {
                            println!(
                                "\x1b[37m{}\x1b[0m",
                                serde_json::to_string(&result).expect("result json")
                            );
                        }
                        if let Some(b) = &self.ui_bus {
                            let rj = serde_json::to_string(&result).expect("result json");
                            let _ = b
                                .send("result", format!("{} {}", tc.name, rj).to_string().as_str());
                        }

                        // stuck detection: N identical consecutive calls
                        let cur = (
                            tc.name.clone(),
                            serde_json::to_string(&args).expect("args ser"),
                        );
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
                            journal.log(
                                "ask_human",
                                &Value::Object(mapof2(
                                    "kind",
                                    "stuck".into(),
                                    "attempts",
                                    serde_json::Value::Array(vec![Value::Object(mapof(
                                        "name",
                                        tc.name.clone().into(),
                                    ))]),
                                )),
                            )?;
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
                        content: resp
                            .content
                            .clone()
                            .unwrap_or_else(|| "[continued tool use]".into()),
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

    /// Provider round-trip with the UI seam: pulls the live stream, forwards
    /// deltas to the bus, and aborts when the UI cancels (Ctrl-C). The body
    /// assembles exactly like chat() — assembly is downstream of streaming.
    fn chat_with_stream(&self) -> Result<ChatMessage, ProviderError> {
        let cs = self.provider.chat_stream(&self.messages);
        match cs {
            Err(e) => Err(e),
            Ok(cs) => {
                let mut mode: String = "plain".into();
                let mut body = String::new();
                loop {
                    if let Some(b) = &self.ui_bus {
                        if b.cancelled() {
                            cs.cancel();
                            return Err(ProviderError::new("cancelled by user".into()));
                        }
                    }
                    match cs.wait(40) {
                        None => continue,
                        Some(ch) => match ch.kind.as_str() {
                            "info" => mode = ch.text.clone(),
                            "delta" => {
                                body += ch.text.as_str();
                                if let Some(b) = &self.ui_bus {
                                    let _ = b.send("delta", ch.text.as_str());
                                }
                            }
                            "done" => {
                                body = ch.text.clone();
                                break;
                            }
                            "cancelled" => {
                                return Err(ProviderError::new("cancelled by user".into()));
                            }
                            "error" => {
                                return Err(ProviderError::new(ch.text.clone()));
                            }
                            _ => {}
                        },
                    }
                }
                let mut parsed: Result<ChatMessage, ProviderError> = if mode == "stream" {
                    assemble_sse(body.as_str())
                } else {
                    parse_chat_json(body.as_str())
                };
                // Lenient fallback: a provider that answers plain JSON to a
                // stream request must not turn into an empty turn.
                if mode == "stream" {
                    if let Ok(m) = &parsed {
                        if m.content.is_none() && m.tool_calls.is_empty() {
                            parsed = parse_chat_json(body.as_str());
                        }
                    }
                }
                parsed
            }
        }
    }

    /// Tool dispatch with policy gating — the only exec path (H3: file tools;
    /// shell/run/ask/skills land in later phases); shell/run/ask_human are
    /// the H4 seam; skills in H6.
    fn execute_tool(&mut self, name: &str, args: &Value, journal: &mut Journal) -> Value {
        match name {
            "read" | "write" | "edit" => toolmod::run(&self.workdir, name, args),
            "shell" => self.tool_shell(args, journal),
            "run" => self.tool_run(args, journal),
            "ask_human" => self.tool_ask_human(args, journal),
            "use_skill" => ok_false("skills land in H6 — not available yet"),
            _ => toolmod::run(&self.workdir, name, args),
        }
    }

    fn denied(reason: &str) -> Value {
        let mut m = mapof("ok", false.into());
        m.insert("denied".to_owned(), true.into());
        m.insert("reason".to_owned(), reason.into());
        serde_json::Value::Object(m)
    }

    /// Approval gate: hard for policy-ask commands. Non-interactive without
    /// auto_grant denies (POC parity); auto_grant auto-allows. With a UI bus
    /// the ask goes through the modal round-trip and the reply decides.
    fn approve(&mut self, cmd: &str, reason: &str, journal: &mut Journal) -> bool {
        if let Some(b) = &self.ui_bus {
            if self.auto_grant {
                journal
                    .log(
                        "approval_granted",
                        &Value::Object(mapof2("command", cmd.into(), "reason", reason.into())),
                    )
                    .ok();
                return true;
            }
            journal
                .log(
                    "approval_requested",
                    &Value::Object(mapof2("command", cmd.into(), "reason", reason.into())),
                )
                .ok();
            let question = format!("{cmd}\n{reason}").to_string();
            let answer = b.ask(question.as_str());
            if answer
                .map(|a| a == "yes" || a == "allow" || a == "y")
                .unwrap_or(false)
            {
                journal
                    .log(
                        "approval_granted",
                        &Value::Object(mapof2("command", cmd.into(), "reason", reason.into())),
                    )
                    .ok();
                true
            } else {
                journal
                    .log(
                        "tool_denied",
                        &Value::Object(mapof2(
                            "command",
                            cmd.into(),
                            "reason",
                            "human denied".into(),
                        )),
                    )
                    .ok();
                false
            }
        } else if !self.interactive && !self.auto_grant {
            let _ = journal;
            false
        } else {
            let grant = if self.auto_grant { Some(true) } else { None };
            ask::ask_approval(journal, cmd, reason, grant).unwrap_or(false)
        }
    }

    /// shell: policy gate → approval → the cage. Verdict chains land in the
    /// journal as tool_verdicts (provable audibility).
    fn tool_shell(&mut self, args: &Value, journal: &mut Journal) -> Value {
        let cmd = args.get("command").and_then(|c| c.as_str());
        match cmd {
            None => ok_false("missing command"),
            Some(cmd) => {
                let verdict = policy::check(cmd, &self.policy_rules);
                match verdict.action.as_str() {
                    "deny" => {
                        let _ = journal.log(
                            "tool_denied",
                            &Value::Object(mapof2(
                                "command",
                                cmd.into(),
                                "reason",
                                verdict.evidence.clone().into(),
                            )),
                        );
                        return Self::denied(
                            format!("{} (denied by policy: {})", cmd, verdict.evidence).as_str(),
                        );
                    }
                    "ask" if !self.approve(cmd, verdict.evidence.as_str(), journal) => {
                        let _ = journal.log(
                            "tool_denied",
                            &Value::Object(mapof2(
                                "command",
                                cmd.into(),
                                "reason",
                                "non-interactive auto-deny".into(),
                            )),
                        );
                        return Self::denied("human denied");
                    }
                    _ => {}
                }
                let binds = toolmod::writable_binds(&self.workdir);
                let result = toolmod::tool_shell(
                    &self.workdir,
                    args,
                    &self.cage,
                    true,
                    &binds,
                    self.run_cfg.default_timeout_s,
                    self.fallback_raw,
                );
                match result {
                    Err(e) => ok_false(format!("cage error: {e}").as_str()),
                    Ok(v) => {
                        journal_verdicts(journal, args, &v);
                        v
                    }
                }
            }
        }
    }

    /// run: typed exec (argv array) checked against the [run] tables.
    fn tool_run(&mut self, args: &Value, journal: &mut Journal) -> Value {
        let program = args.get("program").and_then(|p| p.as_str());
        match program {
            None => ok_false("missing program"),
            Some(program) => {
                let mut words = vec![program.to_string()];
                if let Some(argv) = args.get("argv").and_then(|a| a.as_array()) {
                    for a in argv.iter() {
                        if let Some(tok) = a.as_str() {
                            words.push(tok.to_string());
                        }
                    }
                }
                let joined = words.join(" ");
                let verdict = curam_core::classify_rules(
                    joined.as_str(),
                    Some(self.run_cfg.allow_programs.clone()),
                    Some(self.run_cfg.ask_programs.clone()),
                    Some(self.run_cfg.deny_programs.clone()),
                );
                match verdict.action.as_str() {
                    "deny" => {
                        let _ = journal.log(
                            "tool_denied",
                            &Value::Object(mapof2(
                                "command",
                                joined.clone().into(),
                                "reason",
                                verdict.evidence.clone().into(),
                            )),
                        );
                        return Self::denied(
                            format!("{} (denied by policy: {})", joined, verdict.evidence).as_str(),
                        );
                    }
                    "ask" if !self.approve(joined.as_str(), verdict.evidence.as_str(), journal) => {
                        let _ = journal.log(
                            "tool_denied",
                            &Value::Object(mapof2(
                                "command",
                                joined.clone().into(),
                                "reason",
                                "non-interactive auto-deny".into(),
                            )),
                        );
                        return Self::denied("human denied");
                    }
                    _ => {}
                }
                let result = toolmod::tool_run(
                    &self.workdir,
                    args,
                    &self.cage,
                    self.run_cfg.default_net,
                    self.run_cfg.default_timeout_s,
                    self.fallback_raw,
                );
                match result {
                    Err(e) => ok_false(format!("cage error: {e}").as_str()),
                    Ok(v) => {
                        journal_verdicts(journal, args, &v);
                        v
                    }
                }
            }
        }
    }

    /// ask_human: judgment via the ask layer (non-interactive answers
    /// without prompting, POC parity). With a UI bus the question goes to the
    /// modal and the user's typed reply (or option) comes back.
    fn tool_ask_human(&mut self, args: &Value, journal: &mut Journal) -> Value {
        let q = args
            .get("question")
            .and_then(|x| x.as_str())
            .unwrap_or_default();
        if let Some(b) = &self.ui_bus {
            let mut question = q.to_string();
            if let Some(opts) = args.get("options").and_then(|o| o.as_array()) {
                let joined = opts
                    .iter()
                    .filter_map(|o| o.as_str().map(str::to_string))
                    .collect::<Vec<String>>()
                    .join(", ");
                if !joined.is_empty() {
                    question += format!(" | options: {}", joined).to_string().as_str();
                }
            }
            let answer = b
                .ask(question.as_str())
                .unwrap_or("(no human: denied)".into());
            journal
                .log(
                    "ask_human",
                    &Value::Object(mapof2(
                        "question",
                        q.into(),
                        "answer",
                        answer.clone().into(),
                    )),
                )
                .ok();
            return Value::Object(mapof2("ok", true.into(), "answer", answer.into()));
        }
        let answer = if !self.interactive && !self.auto_grant {
            Some("(non-interactive: no human)".into())
        } else if self.auto_grant {
            Some("1".into())
        } else {
            None
        };
        let ans = ask::ask_judgment(journal, q, None, answer).unwrap_or("(no reply)".into());
        serde_json::Value::Object(mapof2("ok", true.into(), "answer", ans.into()))
    }
}

/// Port of agent.py _journal_verdicts: cage/verdict chains for shell/run.
fn journal_verdicts(journal: &mut Journal, args: &Value, result: &Value) {
    if result.get("verdicts").is_none() && result.get("cages").is_none() {
        return;
    }
    let mut payload = mapof(
        "command",
        serde_json::to_string(args).expect("args json").into(),
    );
    if let Some(vs) = result.get("verdicts").and_then(|v| v.as_array()) {
        if !vs.is_empty() {
            payload.insert("verdicts".to_owned(), vs.clone().into());
        }
    }
    if let Some(c) = result.get("cages").and_then(|v| v.as_array()) {
        payload.insert("cages".to_owned(), c.clone().into());
    }
    if result
        .get("timed_out")
        .and_then(|t| t.as_bool())
        .unwrap_or(false)
    {
        payload.insert("timed_out".to_owned(), true.into());
    }
    if payload.len() > 1 {
        let _ = journal.log("tool_verdicts", &serde_json::Value::Object(payload));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

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
        let dir = std::env::temp_dir()
            .join(format!("curam-h3-a-{tag}"))
            .join(format!("{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Serve an SSE body in two chunked writes with a gap (the reader
    /// genuinely streams two bursts — exactly what cancel must interrupt).
    fn staged_sse(first: &str, second: &str, gap_ms: u64) -> String {
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

    #[test]
    fn ui_stream_deltas_reach_the_bus_and_cancel_aborts() {
        let dir = tmp_dir("uistream");
        let burst1 = "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n";
        let burst2 = "data: {\"choices\":[{\"delta\":{\"content\":null}}]}\ndata: [DONE]\n";
        let url = staged_sse(burst1, burst2, 400);
        let mut cfg = test_cfg(url);
        cfg.providers[0].stream = true;

        let (tx, rx) = mpsc::channel::<ui::UiEv>();
        let (reply_tx, reply_rx) = mpsc::channel::<String>();
        let cancel = Arc::new(AtomicBool::new(false));
        let task_owned: String = "stream me".into();
        let cancel_worker: Arc<AtomicBool> = cancel.clone();
        let dir_owned: std::path::PathBuf = dir.clone();
        thread::spawn(move || {
            let mut a = agent_for(&cfg, &dir_owned);
            a.interactive = true;
            a.ui_bus = Some(ui::UiBus {
                tx,
                reply_rx: Some(reply_rx),
                cancel: cancel_worker,
            });
            let mut jj = Journal::open(dir_owned.join("j.db"), Some("sess".into())).unwrap();
            let ans = a.run_task(task_owned.as_str(), &mut jj);
            let out = match ans {
                Ok(x) => x.unwrap_or_default(),
                Err(_) => String::new(),
            };
            if let Some(b) = &a.ui_bus {
                let _ = b.send("done", out.as_str());
            }
        });

        // The mpsc::Receiver for UiEv is moved into the worker's UI bus, so
        // consume it there — this test drives the bus from ANOTHER thread:
        // first: deltas stream by; then cancel lands between the two bursts.
        let mut saw_delta = false;
        loop {
            match rx.recv_timeout(Duration::from_millis(60)) {
                Ok(ev) => {
                    if ev.kind == "delta" {
                        saw_delta = true;
                        break;
                    }
                    if ev.kind == "done" || ev.kind == "error" {
                        break;
                    }
                }
                Err(_) => {}
            }
        }
        assert!(saw_delta, "the first burst rendered before the cancel");
        cancel.store(true, Ordering::Relaxed);

        // The run must abort: a "done" with an EMPTY answer, never a model
        // message with the second burst's content.
        let mut done_text: Option<String> = None;
        for _ in 0..300 {
            match rx.recv_timeout(Duration::from_millis(25)) {
                Ok(ev) => {
                    if ev.kind == "done" {
                        done_text = Some(ev.text.clone());
                        break;
                    }
                    if ev.kind == "error" {
                        break;
                    }
                }
                Err(_) => {}
            }
        }
        assert_eq!(done_text.unwrap_or_default(), "", "run aborted, no answer");

        // The journal carries the graceful abort (read from a second
        // connection while the worker is gone).
        let mut j2 = Journal::open(dir.join("j.db"), Some("sess".into())).unwrap();
        let errs = j2.events(Some("error")).unwrap();
        let cancelled = errs.iter().any(|e| {
            serde_json::to_string(&e.payload)
                .expect("json")
                .contains("cancelled")
        });
        assert!(
            cancelled,
            "cancel surfaces as a journaled error, not a hang"
        );
        assert_eq!(j2.events(Some("model_message")).unwrap().len(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ui_ask_roundtrip_delivers_the_answer() {
        let dir = tmp_dir("uiask");
        let capture = dir.join("bodies.txt");
        let ask_call = r#"{"choices":[{"message":{"role":"assistant","content":"Asking.","tool_calls":[{"id":"call_1","type":"function","function":{"name":"ask_human","arguments":"{\"question\":\"which color?\",\"options\":[\"red\",\"blue\"]}"}}]}}],"usage":{"prompt_tokens":4,"completion_tokens":3}}"#;
        let answer = r#"{"choices":[{"message":{"role":"assistant","content":"Got it: red."}}],"usage":{"prompt_tokens":10,"completion_tokens":4}}"#;
        let url = scripted(vec![ask_call.into(), answer.into()], &capture);
        let cfg = test_cfg(url);

        let (tx, rx) = mpsc::channel::<ui::UiEv>();
        let (reply_tx, reply_rx) = mpsc::channel::<String>();
        let cancel = Arc::new(AtomicBool::new(false));
        let task_owned: String = "pick a color".into();
        let cancel_worker: Arc<AtomicBool> = cancel.clone();
        let dir_owned: std::path::PathBuf = dir.clone();
        thread::spawn(move || {
            let mut a = agent_for(&cfg, &dir_owned);
            a.interactive = true;
            a.ui_bus = Some(ui::UiBus {
                tx,
                reply_rx: Some(reply_rx),
                cancel: cancel_worker,
            });
            let mut jj = Journal::open(dir_owned.join("j.db"), Some("sess".into())).unwrap();
            let ans = a.run_task(task_owned.as_str(), &mut jj);
            let out = match ans {
                Ok(x) => x.unwrap_or_default(),
                Err(_) => String::new(),
            };
            if let Some(b) = &a.ui_bus {
                let _ = b.send("done", out.as_str());
            }
        });

        // UI side: wait for the ask modal event, then answer "red" (= option
        // text the modal sends back).
        let mut asked = false;
        for _ in 0..300 {
            match rx.recv_timeout(Duration::from_millis(25)) {
                Ok(ev) => {
                    if ev.kind == "ask" {
                        asked = true;
                        reply_tx.send("red".into()).ok();
                        break;
                    }
                    if ev.kind == "done" || ev.kind == "error" {
                        break;
                    }
                }
                Err(_) => {}
            }
        }
        assert!(asked, "ask_human surfaced as a modal event");

        let mut done_text: Option<String> = None;
        for _ in 0..300 {
            match rx.recv_timeout(Duration::from_millis(25)) {
                Ok(ev) => {
                    if ev.kind == "done" {
                        done_text = Some(ev.text.clone());
                        break;
                    }
                    if ev.kind == "error" {
                        break;
                    }
                }
                Err(_) => {}
            }
        }
        assert_eq!(done_text, Some("Got it: red.".into()));

        let mut j2 = Journal::open(dir.join("j.db"), Some("sess".into())).unwrap();
        let asks = j2.events(Some("ask_human")).unwrap();
        assert_eq!(asks.len(), 1);
        assert!(
            serde_json::to_string(&asks[0].payload)
                .expect("json")
                .contains("red"),
            "the human answer is journaled"
        );
        std::fs::remove_dir_all(&dir).ok();
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
        )
        .unwrap();
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
            serde_json::to_string(&trs[0].payload)
                .expect("json")
                .contains("deadbeef"),
            "journal keeps the raw result"
        );

        // ...but the model context (request bodies) is SCRUBBED.
        let captured = std::fs::read_to_string(&capture).unwrap();
        assert!(
            !captured.contains("deadbeef"),
            "secret must not cross the provider boundary: {captured}"
        );
        assert!(
            captured.contains("{REDACTED}:openrouter"),
            "context scrubbed"
        );

        // Full journal chain: user_message, tool_call, tool_result, model_message.
        const WANTED: &[&str] = &["user_message", "tool_call", "tool_result", "model_message"];
        let kinds = j
            .events(None)
            .unwrap()
            .iter()
            .map(|e| e.etype.clone())
            .collect::<Vec<_>>();
        for want in WANTED {
            let found = kinds.iter().any(|k| k == want);
            assert!(found, "missing journal event {want}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn shell_denied_by_policy_never_reaches_the_cage() {
        let dir = tmp_dir("deny");
        let call = r#"{"choices":[{"message":{"role":"assistant","tool_calls":[{"id":"c1","type":"function","function":{"name":"shell","arguments":"{\"command\":\"rm -rf /\"}"}}]}}],"usage":{"prompt_tokens":4,"completion_tokens":3}}"#;
        let answer = r#"{"choices":[{"message":{"role":"assistant","content":"Blocked as expected."}}],"usage":{"prompt_tokens":8,"completion_tokens":2}}"#;
        let url = scripted(vec![call.into(), answer.into()], &dir.join("bodies.txt"));
        let mut cfg = test_cfg(url);
        // The cage binary is missing — policy DENY must stop the command first.
        cfg.cage.binary = "/nonexistent/curam-run".into();
        let mut a = agent_for(&cfg, &dir);
        let mut j = Journal::open(dir.join("j.db"), Some("sess".into())).unwrap();

        let ans = a.run_task("remove root", &mut j).unwrap();
        assert!(ans.unwrap().contains("Blocked as expected"));

        let ds = j.events(Some("tool_denied")).unwrap();
        assert_eq!(ds.len(), 1);
        assert!(serde_json::to_string(&ds[0].payload)
            .expect("json")
            .contains("rm -rf /"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn shell_runs_through_the_cage_and_journals_verdicts() {
        let dir = tmp_dir("cage");
        // Resolve the real cage binary from the default config first; skip
        // when this machine has no curam-run build (CI portability guard).
        let default_cfg = config::Loader::new(None, None).load().unwrap();
        let cage = Cage::from_cfg(&default_cfg.cage);
        if !cage.available() {
            return;
        }
        let call = r#"{"choices":[{"message":{"role":"assistant","tool_calls":[{"id":"c1","type":"function","function":{"name":"shell","arguments":"{\"command\":\"echo caged-ok\"}"}}]}}],"usage":{"prompt_tokens":4,"completion_tokens":3}}"#;
        let answer = r#"{"choices":[{"message":{"role":"assistant","content":"Cage says caged-ok."}}],"usage":{"prompt_tokens":8,"completion_tokens":2}}"#;
        let url = scripted(vec![call.into(), answer.into()], &dir.join("bodies.txt"));
        let cfg = test_cfg(url);
        let mut a = agent_for(&cfg, &dir);
        let mut j = Journal::open(dir.join("j.db"), Some("sess".into())).unwrap();

        let ans = a.run_task("say it", &mut j).unwrap();
        assert!(ans.unwrap().contains("Cage says caged-ok"));

        // The cage verdict chain is in the journal (tool_verdicts).
        let vs = j.events(Some("tool_verdicts")).unwrap();
        assert_eq!(vs.len(), 1, "cage layers must be journaled");
        assert!(serde_json::to_string(&vs[0].payload)
            .expect("json")
            .contains("bwrap"));
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
        assert!(
            ans.clone().unwrap().contains("Stopped after 25 iterations"),
            "{ans:?}"
        );
        let errs = j.events(Some("error")).unwrap();
        assert!(errs.iter().any(|e| e
            .payload
            .get("msg")
            .and_then(|m| m.as_str())
            .unwrap_or_default()
            .contains("iterations")));
        std::fs::remove_dir_all(&dir).ok();
    }
}
