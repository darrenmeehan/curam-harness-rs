//! tui.rs — the desktop front end (H5): gpui-kit + libghostty.
//!
//! One window. The transcript is a real terminal: libghostty-vt (via
//! gpui-cn's `Terminal`) emulates the byte stream we write, so scrollback,
//! selection, copy and the Kitty graphics protocol come from Ghostty's core
//! instead of being reimplemented. The status rail and the prompt are
//! gpui-kit components; an ask replaces the prompt with Allow / Session /
//! Deny buttons, and typed text answers it.
//!
//! The agent runs on a worker thread fed by the ui::UiBus, so the window
//! never blocks. Ctrl-C cancels the in-flight run; closing the window quits.
//!
//! The harness feeds the transcript with its own SGR bytes: agent text is
//! untrusted (tool output can carry escape sequences), so every control
//! character is dropped by [`clean`] before it reaches the emulator.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use gpui_cn::terminal::{
    Engine, EngineOptions, StreamPeer, StreamSource, Terminal, TerminalConfig, TerminalState,
};
use gpui_kit::base::StyledExt as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::{
    actions, div, px, AppContext as _, ClickEvent, Context, InteractiveElement as _, IntoElement,
    KeyBinding, ParentElement, Render, Styled, Window,
};

use super::budget::Budget;
use super::config;
use super::journal::Journal;
use super::provider::Provider;
use super::ui::{UiBus, UiEv};

actions!(curam, [CancelRun]);

const RAIL_W: f32 = 220.0;
const MAX_ROWS: usize = 500;
const MAX_CHARS: usize = 300;

/// One visible conversation row (kind drives the colour + prefix).
#[derive(Debug)]
pub struct Row {
    pub kind: String,
    pub text: String,
}

#[derive(Debug)]
pub struct Status {
    pub provider: String,
    pub model: String,
    pub cost: f64,
    pub spent: f64,
    pub iterations: usize,
    pub cage: String,
    pub running: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskPrompt {
    pub question: String,
    pub answer: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    Input,
    Ask(AskPrompt),
}

/// What the transcript must draw for one applied event.
#[derive(Debug, PartialEq, Eq)]
pub enum Out {
    /// More bytes on the open line (a folded thinking delta).
    Append(String),
    /// A new line: kind + text.
    Row(String, String),
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// Printable copy of untrusted text: control characters dropped, capped.
pub fn clean(text: &str, max: usize) -> String {
    text.chars().filter(|c| !c.is_control()).take(max).collect()
}

/// SGR colour per row kind (the POC palette, ported).
fn sgr(kind: &str) -> &'static str {
    match kind {
        "delta" | "thinking" => "\x1b[90m",
        "tool" => "\x1b[36m",
        "result" => "\x1b[37m",
        "error" => "\x1b[31m",
        "ask" | "stuck" => "\x1b[33m",
        "answer" => "\x1b[32m",
        _ => "",
    }
}

/// Line prefix per row kind.
fn marker(kind: &str) -> &'static str {
    match kind {
        "delta" | "thinking" => "… ",
        "tool" => "▶ ",
        "result" => "· ",
        "error" => "✗ ",
        "ask" | "stuck" => "? ",
        "answer" => "→ ",
        _ => "",
    }
}

/// The terminal bytes for one event.
fn bytes(out: &Out) -> String {
    match out {
        Out::Append(text) => format!("\x1b[90m{}\x1b[0m", clean(text, MAX_CHARS)),
        Out::Row(kind, text) => format!(
            "{}{}{}\x1b[0m",
            sgr(kind),
            marker(kind),
            clean(text, MAX_CHARS)
        ),
    }
}

/// The pure view state: rows, status, ask. No gpui inside, so it tests
/// without a window.
#[derive(Debug)]
pub struct App {
    pub rows: Vec<Row>,
    pub status: Status,
    pub mode: Mode,
    pub enter_pending: bool,
}

impl App {
    pub fn new(cfg: &config::Config, provider: &Provider, budget: &Budget) -> Self {
        let cage = if cfg.cage.fallback == "raw" {
            "RAW!".into()
        } else {
            "caged".into()
        };
        Self {
            rows: Vec::new(),
            status: Status {
                provider: provider.name.clone(),
                model: provider.model.clone(),
                cost: 0.0,
                spent: round2(budget.spent_usd),
                iterations: 0,
                cage,
                running: false,
            },
            mode: Mode::Input,
            enter_pending: false,
        }
    }

    pub fn push(&mut self, kind: &str, text: String) {
        self.rows.push(Row {
            kind: kind.into(),
            text,
        });
        // the terminal keeps the full history; the model only needs the tail
        if self.rows.len() > MAX_ROWS {
            self.rows.remove(0);
        }
    }

    /// Apply one agent event to the view state, and say what to draw.
    pub fn apply(&mut self, ev: &UiEv) -> Option<Out> {
        match ev.kind.as_str() {
            "delta" => {
                // fold the token stream into one dim block: extend the last
                // row when it is already the open thinking block.
                if let Some(last) = self.rows.last_mut() {
                    if last.kind == "delta" {
                        last.text.push_str(ev.text.as_str());
                        return Some(Out::Append(ev.text.clone()));
                    }
                }
                self.push("delta", ev.text.clone());
                Some(Out::Row("delta".into(), ev.text.clone()))
            }
            "tool" | "result" | "error" | "answer" => {
                self.push(ev.kind.as_str(), ev.text.clone());
                Some(Out::Row(ev.kind.clone(), ev.text.clone()))
            }
            "status" => {
                let t = ev.text.as_str();
                // "cost=0.12 spent=1.34"
                if let Some(i) = t.find("spent=") {
                    let cost_s = t[5..i].trim();
                    let spent_s = t[i + 6..].trim();
                    self.status.cost = cost_s.parse::<f64>().unwrap_or(0.0);
                    self.status.spent = spent_s.parse::<f64>().unwrap_or(0.0);
                    self.status.iterations += 1;
                }
                None
            }
            "ask" | "stuck" => {
                self.push(ev.kind.as_str(), ev.text.clone());
                self.mode = Mode::Ask(AskPrompt {
                    question: ev.text.clone(),
                    answer: String::new(),
                });
                Some(Out::Row(ev.kind.clone(), ev.text.clone()))
            }
            "done" => {
                self.status.running = false;
                if ev.text.is_empty() {
                    None
                } else {
                    self.push("answer", ev.text.clone());
                    Some(Out::Row("answer".into(), ev.text.clone()))
                }
            }
            _ => None,
        }
    }

    /// Answer the open ask, leaving ask mode. None when no ask is open.
    pub fn reply(&mut self, answer: &str) -> Option<String> {
        if matches!(&self.mode, Mode::Ask(_)) {
            self.mode = Mode::Input;
            return Some(answer.to_string());
        }
        None
    }

    /// Status rail: provider/model, cost, iterations, cage, mode.
    pub fn rail(&self) -> Vec<String> {
        vec![
            format!("curam {}", env!("CARGO_PKG_VERSION")),
            format!("{}  {}", self.status.provider, self.status.model),
            format!(
                "cost=${}  spent=${}",
                round2(self.status.cost),
                round2(self.status.spent)
            ),
            format!("iterations {}", self.status.iterations),
            format!("cage {}", self.status.cage),
            format!(
                "mode {}",
                if self.status.running {
                    "working"
                } else {
                    "idle"
                }
            ),
        ]
    }
}

/// The window: the pure state, the terminal that draws the transcript, and
/// the worker channels.
struct View {
    app: App,
    terminal: gpui_kit::Entity<TerminalState>,
    peer: StreamPeer,
    input: gpui_kit::Entity<InputState>,
    ui_rx: mpsc::Receiver<UiEv>,
    cancel: Arc<AtomicBool>,
    reply_tx: Option<mpsc::Sender<String>>,
    cfg: config::Config,
    provider: Provider,
    budget: Budget,
    workdir: std::path::PathBuf,
    journal_path: std::path::PathBuf,
    agent_tx: mpsc::Sender<UiEv>,
    line_open: bool,
}

impl View {
    #[allow(clippy::too_many_arguments)]
    fn new(
        cfg: config::Config,
        provider: Provider,
        budget: Budget,
        workdir: std::path::PathBuf,
        journal_path: std::path::PathBuf,
        agent_tx: mpsc::Sender<UiEv>,
        ui_rx: mpsc::Receiver<UiEv>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let app = App::new(&cfg, &provider, &budget);

        let (source, peer) = StreamSource::new();
        let terminal = cx.new(|cx| {
            TerminalState::new(
                Engine::new(source, EngineOptions::default()),
                TerminalConfig::default(),
                window,
                cx,
            )
        });
        cx.observe(&terminal, |_, _, cx| cx.notify()).detach();

        let input = cx.new(|cx| InputState::new(window, cx).placeholder("curam> describe a task"));
        cx.subscribe(&input, |this: &mut Self, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::PressEnter { .. }) {
                this.app.enter_pending = true;
                cx.notify();
            }
        })
        .detach();
        input.update(cx, |state, cx| state.focus(window, cx));

        // Drain the agent's events on a timer: the worker is a thread, the
        // window must never block on it.
        cx.spawn_in(window, async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(30))
                .await;
            if this
                .update_in(cx, |view, window, cx| view.pump(window, cx))
                .is_err()
            {
                break;
            }
        })
        .detach();

        Self {
            app,
            terminal,
            peer,
            input,
            ui_rx,
            cancel: Arc::new(AtomicBool::new(false)),
            reply_tx: None,
            cfg,
            provider,
            budget,
            workdir,
            journal_path,
            agent_tx,
            line_open: false,
        }
    }

    /// Apply queued agent events, then act on a parked Enter.
    fn pump(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut dirty = false;
        loop {
            match self.ui_rx.try_recv() {
                Ok(ev) => {
                    if let Some(out) = self.app.apply(&ev) {
                        self.write(&out);
                    }
                    dirty = true;
                }
                Err(_) => break,
            }
        }
        if self.app.enter_pending {
            self.app.enter_pending = false;
            self.submit(window, cx);
            dirty = true;
        }
        if dirty {
            cx.notify();
        }
    }

    /// Write one event's bytes into the terminal, opening a line first when
    /// the event starts a new row.
    fn write(&mut self, out: &Out) {
        let mut chunk = String::new();
        if !matches!(out, Out::Append(_)) {
            if self.line_open {
                chunk.push_str("\r\n");
            }
            self.line_open = true;
        }
        chunk.push_str(&bytes(out));
        self.peer.output(chunk.as_bytes());
    }

    /// Enter, or a click: run the typed task, or answer the open ask.
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().trim().to_string();
        if matches!(&self.app.mode, Mode::Ask(_)) {
            let answer = if text.is_empty() {
                "yes".to_string()
            } else {
                text
            };
            self.reply(&answer, cx);
            self.input
                .update(cx, |state, cx| state.set_value("", window, cx));
            return;
        }
        if text.is_empty() || self.app.status.running {
            return;
        }
        self.input
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.start(text, cx);
    }

    /// Spawn one task run on a worker thread; its events come back on ui_rx.
    fn start(&mut self, task: String, cx: &mut Context<Self>) {
        self.app.push("user", task.clone());
        let out = Out::Row("user".into(), task.clone());
        self.write(&out);
        self.app.status.running = true;
        self.cancel.store(false, Ordering::Relaxed);
        let (reply_tx, reply_rx) = mpsc::channel::<String>();
        self.reply_tx = Some(reply_tx);
        spawn_task(
            self.cfg.clone(),
            self.provider.clone(),
            self.budget.clone(),
            self.workdir.clone(),
            self.journal_path.clone(),
            task,
            self.agent_tx.clone(),
            reply_rx,
            self.cancel.clone(),
        );
        cx.notify();
    }

    fn reply(&mut self, answer: &str, cx: &mut Context<Self>) {
        if let Some(reply) = self.app.reply(answer) {
            if let Some(tx) = &self.reply_tx {
                let _ = tx.send(reply);
            }
        }
        cx.notify();
    }

    fn answer_allow(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.reply("yes", cx);
    }

    fn answer_session(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.reply("allow", cx);
    }

    fn answer_deny(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.reply("no", cx);
    }

    /// Ctrl-C: stop the in-flight run; the worker journals its own error.
    fn cancel_run(&mut self, _: &CancelRun, _: &mut Window, _: &mut Context<Self>) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Render for View {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rail = self.app.rail();
        let bottom = match self.app.mode.clone() {
            Mode::Ask(p) => div()
                .h_flex()
                .items_center()
                .gap_2()
                .p_2()
                .child(div().flex_1().child(p.question))
                .child(
                    Button::new("allow")
                        .label("Allow")
                        .on_click(cx.listener(Self::answer_allow)),
                )
                .child(
                    Button::new("session")
                        .outline()
                        .label("Session")
                        .on_click(cx.listener(Self::answer_session)),
                )
                .child(
                    Button::new("deny")
                        .outline()
                        .label("Deny")
                        .on_click(cx.listener(Self::answer_deny)),
                ),
            Mode::Input => {
                let hint = if self.app.status.running {
                    "running — Ctrl-C cancels"
                } else {
                    "Enter runs · Ctrl-C cancels"
                };
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .p_2()
                    .child(div().flex_1().child(Input::new(&self.input).id("prompt")))
                    .child(div().child(hint))
            }
        };

        div()
            .size_full()
            .v_flex()
            .on_action(cx.listener(Self::cancel_run))
            .child(
                div()
                    .flex_1()
                    .h_flex()
                    .child(
                        Terminal::new("transcript", &self.terminal)
                            .h_full()
                            .flex_1(),
                    )
                    .child(
                        div()
                            .w(px(RAIL_W))
                            .h_full()
                            .p_2()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .children(
                                rail.into_iter()
                                    .map(|line| div().child(gpui_kit::SharedString::from(line))),
                            ),
                    ),
            )
            .child(bottom)
    }
}

/// Spawn one task run. Owns the journal; streams events to the UI bus.
pub fn spawn_task(
    cfg: config::Config,
    provider: Provider,
    budget: Budget,
    workdir: std::path::PathBuf,
    journal_path: std::path::PathBuf,
    task: String,
    agent_tx: mpsc::Sender<UiEv>,
    agent_reply_rx: mpsc::Receiver<String>,
    cancel: Arc<AtomicBool>,
) {
    let cancel_worker: Arc<AtomicBool> = cancel.clone();
    thread::spawn(move || {
        let mut jj = Journal::open(journal_path, None).unwrap();
        let mut a =
            super::agent::Agent::new(&cfg, provider, budget, workdir, true, false, false, false);
        a.ui_bus = Some(UiBus {
            tx: agent_tx.clone(),
            reply_rx: Some(agent_reply_rx),
            cancel: cancel_worker,
        });
        let ans = a.run_task(task.as_str(), &mut jj);
        match ans {
            Ok(Some(text)) => {
                let _ = agent_tx.send(UiEv {
                    kind: "answer".into(),
                    text,
                });
            }
            Err(e) => {
                let _ = agent_tx.send(UiEv {
                    kind: "error".into(),
                    text: e,
                });
            }
            Ok(None) => {}
        }
        // "done" always closes the run, error or not (it clears `running`).
        let _ = agent_tx.send(UiEv {
            kind: "done".into(),
            text: String::new(),
        });
    });
}

/// Open the curam window. Returns the process exit code (0 ok, 1 no display).
pub fn run(
    cfg: config::Config,
    provider: Provider,
    budget: Budget,
    workdir: std::path::PathBuf,
    journal_path: std::path::PathBuf,
) -> i32 {
    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        eprintln!("curam tui: no display (DISPLAY/WAYLAND_DISPLAY unset); use curam repl");
        return 1;
    }
    let (agent_tx, ui_rx) = mpsc::channel::<UiEv>();
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            gpui_cn::init(cx);
            cx.bind_keys([KeyBinding::new("ctrl-c", CancelRun, None)]);
            gpui_kit::open_window(gpui_kit::WindowOptions::default(), cx, move |window, cx| {
                window.set_window_title("curam");
                cx.new(|cx| {
                    View::new(
                        cfg,
                        provider,
                        budget,
                        workdir,
                        journal_path,
                        agent_tx,
                        ui_rx,
                        window,
                        cx,
                    )
                })
            })
            .expect("curam tui: failed to open window");
        });
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_app() -> App {
        let cfg = config::Loader::new(None, None).load().unwrap();
        let p = Provider::new(&cfg, None, None).unwrap();
        let b = Budget::from_cfg(&cfg.budget);
        let mut app = App::new(&cfg, &p, &b);
        app.push("user", "add a README".into());
        app.push("tool", read_tool().as_str().into());
        app.push("result", "{\"ok\":true}".into());
        app.status.cost = 0.5;
        app.status.spent = 0.5;
        app
    }

    fn read_tool() -> String {
        "read {\"path\":\"README.md\"}".to_string()
    }

    #[test]
    fn deltas_fold_into_one_row_and_the_ask_opens() {
        let mut app = sample_app();
        assert_eq!(
            app.apply(&UiEv {
                kind: "delta".into(),
                text: "Hel".into()
            }),
            Some(Out::Row("delta".into(), "Hel".into()))
        );
        assert_eq!(
            app.apply(&UiEv {
                kind: "delta".into(),
                text: "lo".into()
            }),
            Some(Out::Append("lo".into()))
        );
        assert_eq!(app.rows[app.rows.len() - 1].text, "Hello");

        app.apply(&UiEv {
            kind: "ask".into(),
            text: "rm -rf / (destructive)".into(),
        });
        assert_eq!(
            app.mode,
            Mode::Ask(AskPrompt {
                question: "rm -rf / (destructive)".into(),
                answer: String::new(),
            })
        );
        assert_eq!(app.reply("no"), Some("no".into()));
        assert_eq!(app.mode, Mode::Input);
        assert_eq!(app.reply("no"), None);
    }

    #[test]
    fn status_updates_do_not_draw_and_done_clears_running() {
        let mut app = sample_app();
        app.status.running = true;
        assert_eq!(
            app.apply(&UiEv {
                kind: "status".into(),
                text: "cost=0.12 spent=1.34".into()
            }),
            None
        );
        assert_eq!(app.status.cost, 0.12);
        assert_eq!(app.status.iterations, 1);
        assert_eq!(
            app.apply(&UiEv {
                kind: "done".into(),
                text: String::new()
            }),
            None
        );
        assert!(!app.status.running);
    }

    #[test]
    fn escape_sequences_from_tool_output_never_reach_the_emulator() {
        let out = Out::Row("result".into(), "\x1b]52;c;cm0=\x07ok".into());
        let line = bytes(&out);
        assert_eq!(line, "\x1b[37m· ]52;c;cm0=ok\x1b[0m");
        assert_eq!(clean("a\tb\u{7}c\n", 10), "abc");
        // multi-byte text is capped by chars, never split mid-codepoint
        assert_eq!(clean("éééé", 2), "éé");
    }

    #[test]
    fn rail_carries_provider_cost_and_mode() {
        let mut app = sample_app();
        app.status.running = true;
        let rail = app.rail();
        assert!(rail[1].contains(&app.status.provider));
        assert!(rail[2].contains("cost=$0.5"));
        assert_eq!(rail[5], "mode working");
        assert_eq!(app.rows.len(), 3);
    }
}
