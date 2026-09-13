//! tools.rs — the model's file tools: read / write / edit (port of
//! tools.py). shell + the typed run tool land in H4 behind the cage; the
//! tool *schemas* advertised to the provider grow with them.
//!
//! Path handling matches the POC (resolve inside the workdir; the harness
//! self-sandbox is a deferred release — the exec boundary is the cage).

use serde_json::Value;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use super::cage;

pub const MAX_READ_CHARS: usize = 60_000;
pub const MAX_OUTPUT_CHARS: usize = 8_000;

/// The tool schemas sent to the provider (OpenAI function shape). H3 ships
/// the file tools; shell/run/ask_human/use_skill are added as they land.
const SCHEMA_JSONS: &[&str] = &[
    "{\"type\":\"function\",\"function\":{\"name\":\"read\",\"description\":\"Read a file within the workdir.\",\"parameters\":{\"type\":\"object\",\"properties\":{\"path\":{\"type\":\"string\",\"description\":\"relative path\"}},\"required\":[\"path\"]}}}",
    "{\"type\":\"function\",\"function\":{\"name\":\"write\",\"description\":\"Write a file within the workdir (overwrites).\",\"parameters\":{\"type\":\"object\",\"properties\":{\"path\":{\"type\":\"string\"},\"content\":{\"type\":\"string\"}},\"required\":[\"path\",\"content\"]}}}",
    "{\"type\":\"function\",\"function\":{\"name\":\"edit\",\"description\":\"Replace the first occurrence of old_text with new_text in a file.\",\"parameters\":{\"type\":\"object\",\"properties\":{\"path\":{\"type\":\"string\"},\"old_text\":{\"type\":\"string\"},\"new_text\":{\"type\":\"string\"}},\"required\":[\"path\",\"old_text\",\"new_text\"]}}}",
    "{\"type\":\"function\",\"function\":{\"name\":\"shell\",\"description\":\"Run a shell command in the workdir (sandboxed). Returns exit code, stdout, stderr.\",\"parameters\":{\"type\":\"object\",\"properties\":{\"command\":{\"type\":\"string\"},\"timeout\":{\"type\":\"integer\",\"description\":\"seconds, default 60\"}},\"required\":[\"command\"]}}}",
    "{\"type\":\"function\",\"function\":{\"name\":\"run\",\"description\":\"Run a program with explicit argv (typed exec, no shell). Sandboxed like shell.\",\"parameters\":{\"type\":\"object\",\"properties\":{\"program\":{\"type\":\"string\"},\"argv\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"timeout\":{\"type\":\"integer\"},\"net\":{\"type\":\"boolean\",\"description\":\"egress on/off (default from config)\"}},\"required\":[\"program\"]}}}",
    "{\"type\":\"function\",\"function\":{\"name\":\"ask_human\",\"description\":\"Ask the human for judgment (design fork, ambiguity, irreversible change, or when stuck). Use sparingly — for decisions only a human can make.\",\"parameters\":{\"type\":\"object\",\"properties\":{\"question\":{\"type\":\"string\"},\"options\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}}},\"required\":[\"question\"]}}}",
];

/// The schemas sent to the provider (built once; they never move).
pub fn schemas() -> &'static Vec<Value> {
    static S: OnceLock<Vec<Value>> = OnceLock::new();
    S.get_or_init(|| {
        SCHEMA_JSONS
            .iter()
            .map(|j| serde_json::from_str(j).expect("valid tool schema json"))
            .collect()
    })
}

fn okish(ok: bool, error: &str) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("ok".to_owned(), ok.into());
    if !ok {
        m.insert("error".to_owned(), error.into());
    }
    serde_json::Value::Object(m)
}

fn need_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

pub fn tool_read(workdir: &Path, args: &Value) -> Value {
    let path_str = need_str(args, "path");
    match path_str {
        None => okish(false, "missing path"),
        Some(path_str) => {
            let p = workdir.join(path_str.as_str());
            match std::fs::read_to_string(&p) {
                Ok(text) => {
                    let truncated = text.len() > MAX_READ_CHARS;
                    let content = if truncated {
                        text[0..MAX_READ_CHARS].to_string()
                    } else {
                        text
                    };
                    let mut m = serde_json::Map::new();
                    m.insert("ok".to_owned(), true.into());
                    m.insert("path".to_owned(), format!("{p:?}").into());
                    m.insert("content".to_owned(), content.into());
                    m.insert("truncated".to_owned(), truncated.into());
                    serde_json::Value::Object(m)
                }
                Err(e) => {
                    let mut m = serde_json::Map::new();
                    m.insert("ok".to_owned(), false.into());
                    m.insert("error".to_owned(), format!("{e}").into());
                    serde_json::Value::Object(m)
                }
            }
        }
    }
}

pub fn tool_write(workdir: &Path, args: &Value) -> Value {
    let path_str = need_str(args, "path");
    let content_str = need_str(args, "content");
    match (path_str, content_str) {
        (Some(path_str), Some(content_str)) => {
            let p = workdir.join(path_str.as_str());
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match std::fs::write(&p, content_str.as_bytes()) {
                Ok(_) => {
                    let mut m = serde_json::Map::new();
                    m.insert("ok".to_owned(), true.into());
                    m.insert("path".to_owned(), format!("{p:?}").into());
                    m.insert("bytes".to_owned(), (content_str.len() as u64).into());
                    serde_json::Value::Object(m)
                }
                Err(e) => {
                    let mut m = serde_json::Map::new();
                    m.insert("ok".to_owned(), false.into());
                    m.insert("error".to_owned(), format!("{e}").into());
                    serde_json::Value::Object(m)
                }
            }
        }
        _ => okish(false, "missing path and/or content"),
    }
}

pub fn tool_edit(workdir: &Path, args: &Value) -> Value {
    let path_str = need_str(args, "path");
    let old_str = need_str(args, "old_text");
    let new_str = need_str(args, "new_text");
    match (path_str, old_str, new_str) {
        (Some(path_str), Some(old_str), Some(new_str)) => {
            let p = workdir.join(path_str.as_str());
            let text = match std::fs::read_to_string(&p) {
                Ok(t) => t,
                Err(e) => {
                    let mut m = serde_json::Map::new();
                    m.insert("ok".to_owned(), false.into());
                    m.insert("error".to_owned(), format!("{e}").into());
                    return serde_json::Value::Object(m);
                }
            };
            // Replace only the FIRST occurrence (Python str.replace(old,new,1)).
            let idx = text.find(old_str.as_str());
            match idx {
                None => {
                    let mut m = serde_json::Map::new();
                    m.insert("ok".to_owned(), false.into());
                    m.insert("error".to_owned(), "old_text not found in file".into());
                    serde_json::Value::Object(m)
                }
                Some(i) => {
                    let out = format!("{}{}{}", &text[0..i], new_str, &text[i + old_str.len()..]);
                    let wres = std::fs::write(&p, out.as_bytes());
                    match wres {
                        Ok(_) => {
                            let mut m = serde_json::Map::new();
                            m.insert("ok".to_owned(), true.into());
                            m.insert("path".to_owned(), format!("{p:?}").into());
                            m.insert("replaced".to_owned(), 1.into());
                            serde_json::Value::Object(m)
                        }
                        Err(e) => {
                            let mut m = serde_json::Map::new();
                            m.insert("ok".to_owned(), false.into());
                            m.insert("error".to_owned(), format!("{e}").into());
                            serde_json::Value::Object(m)
                        }
                    }
                }
            }
        }
        _ => okish(false, "missing path/old_text/new_text"),
    }
}

/// The writable binds the cage mounts: workdir + HOME (git/pip/cargo keep
/// caches + credentials under $HOME, exactly like the POC's _cage_bind).
pub fn writable_binds(workdir: &Path) -> Vec<String> {
    let home = std::env::var_os("HOME")
        .map(|h| {
            std::path::PathBuf::from(h)
                .to_str()
                .unwrap_or_default()
                .to_string()
        })
        .unwrap_or_default();
    let wd = workdir.to_str().unwrap_or_default().to_string();
    vec![format!("{}:{}", wd, wd), format!("{}:{}", home, home)]
}

/// Turn a cage result into the tool result shape the model sees.
fn cage_to_result(r: &cage::CageResult, timeout: u64) -> Value {
    let _ = timeout;
    if r.timed_out {
        let mut m = serde_json::Map::new();
        m.insert("ok".to_owned(), false.into());
        m.insert(
            "error".to_owned(),
            format!("timed out after {}s", timeout).into(),
        );
        m.insert("stdout".to_owned(), r.stdout.clone().into());
        m.insert("stderr".to_owned(), r.stderr.clone().into());
        m.insert("timed_out".to_owned(), true.into());
        m.insert("cages".to_owned(), r.cages.clone().into());
        m.insert("verdicts".to_owned(), r.verdicts.clone().into());
        serde_json::Value::Object(m)
    } else {
        let mut m = serde_json::Map::new();
        m.insert("ok".to_owned(), (r.exit == 0).into());
        m.insert("exit".to_owned(), r.exit.into());
        m.insert("stdout".to_owned(), r.stdout.clone().into());
        m.insert("stderr".to_owned(), r.stderr.clone().into());
        m.insert("timed_out".to_owned(), false.into());
        m.insert("cages".to_owned(), r.cages.clone().into());
        m.insert("verdicts".to_owned(), r.verdicts.clone().into());
        serde_json::Value::Object(m)
    }
}

/// `shell` — free-text command, run inside the curam-run cage (net on,
/// system dirs read-only, workdir + HOME writable). `fallback_raw` is the
/// opt-in [cage] fallback for machines without the cage binary.
pub fn tool_shell(
    workdir: &Path,
    args: &Value,
    cage: &cage::Cage,
    net: bool,
    binds: &[String],
    timeout_default: u64,
    fallback_raw: bool,
) -> Result<Value, String> {
    let cmd = args.get("command").and_then(|c| c.as_str());
    match cmd {
        None => Ok(okish(false, "missing command")),
        Some(cmd) => {
            let timeout = args
                .get("timeout")
                .and_then(|t| t.as_u64())
                .unwrap_or(timeout_default);
            let req = cage::CageRequest {
                id: 0,
                cmd: vec!["bash".into(), "-c".into(), cmd.to_string()],
                cwd: Some(workdir.to_str().unwrap_or_default().to_string()),
                timeout_s: Some(timeout),
                net,
                geo: None,
                bind: binds.to_vec(),
                allow_hosts: Vec::new(),
                deny_hosts: Vec::new(),
            };
            match cage.run(&req) {
                Err(e) => Ok(okish(false, format!("cage error: {e}").as_str())),
                Ok(None) => {
                    if fallback_raw {
                        raw_fallback("bash".into(), vec!["-c".into(), cmd.to_string()])
                    } else {
                        Ok(okish(
                            false,
                            "curam-run unavailable — build it: cargo build -p curam-run (in ~/code/curam-security) or set CURAM_RUN_BIN",
                        ))
                    }
                }
                Ok(Some(r)) => Ok(cage_to_result(&r, timeout)),
            }
        }
    }
}

/// `run` — typed exec (argv array, no shell). net defaults from [run].
pub fn tool_run(
    workdir: &Path,
    args: &Value,
    cage: &cage::Cage,
    net_default: bool,
    timeout_default: u64,
    fallback_raw: bool,
) -> Result<Value, String> {
    let program = args.get("program").and_then(|p| p.as_str());
    match program {
        None => Ok(okish(false, "missing program")),
        Some(program) => {
            let argv = args
                .get("argv")
                .and_then(|a| a.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|e| e.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let net = args
                .get("net")
                .and_then(|n| n.as_bool())
                .unwrap_or(net_default);
            let timeout = args
                .get("timeout")
                .and_then(|t| t.as_u64())
                .unwrap_or(timeout_default);
            let mut words = vec![program.to_string()];
            for w in argv {
                words.push(w);
            }
            let req = cage::CageRequest {
                id: 0,
                cmd: words,
                cwd: Some(workdir.to_str().unwrap_or_default().to_string()),
                timeout_s: Some(timeout),
                net,
                geo: None,
                bind: vec![],
                allow_hosts: Vec::new(),
                deny_hosts: Vec::new(),
            };
            match cage.run(&req) {
                Err(e) => Ok(okish(false, format!("cage error: {e}").as_str())),
                Ok(None) => {
                    if fallback_raw {
                        raw_fallback(program.to_string(), Vec::new())
                    } else {
                        Ok(okish(
                            false,
                            "curam-run unavailable — build it: cargo build -p curam-run (in ~/code/curam-security) or set CURAM_RUN_BIN",
                        ))
                    }
                }
                Ok(Some(r)) => Ok(cage_to_result(&r, timeout)),
            }
        }
    }
}

/// The opt-in unsandboxed fallback ([cage] fallback = "raw"). Very loud.
fn raw_fallback(program: String, argv: Vec<String>) -> Result<Value, String> {
    let mut cmd = Command::new(&program);
    cmd.args(argv);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Err(format!("raw fallback spawn failed: {e}")),
    };
    let mut out = String::new();
    if let Some(mut so) = child.stdout.take() {
        let _ = so.read_to_string(&mut out);
    }
    let status = child.wait().unwrap_or_default();
    Ok(okish(
        false,
        format!(
            "RAW fallback (unsandboxed!), exit {} — this is not how curam is meant to run",
            status.code().unwrap_or(1)
        )
        .as_str(),
    ))
}

/// Dispatch one tool call. Returns the result object the agent journals and
/// (scrubbed) feeds back to the model.
pub fn run(workdir: &Path, name: &str, args: &Value) -> Value {
    match name {
        "read" => tool_read(workdir, args),
        "write" => tool_write(workdir, args),
        "edit" => tool_edit(workdir, args),
        _ => okish(false, format!("unknown tool: {name}").as_str()),
    }
}

pub fn is_known(name: &str) -> bool {
    name == "read"
        || name == "write"
        || name == "edit"
        || name == "shell"
        || name == "run"
        || name == "ask_human"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_read_edit_roundtrip() {
        let dir = std::env::temp_dir()
            .join("curam-h3-t")
            .join(format!("{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wd = &dir;
        let w = tool_write(
            wd,
            &serde_json::from_str("{\"path\":\"a/b.txt\",\"content\":\"hello\"}").unwrap(),
        );
        assert_eq!(w["ok"], true);

        let r = tool_read(wd, &serde_json::from_str("{\"path\":\"a/b.txt\"}").unwrap());
        assert_eq!(r["content"], "hello");

        let e = tool_edit(
            wd,
            &serde_json::from_str(
                "{\"path\":\"a/b.txt\",\"old_text\":\"hello\",\"new_text\":\"bye\"}",
            )
            .unwrap(),
        );
        assert_eq!(e["ok"], true);
        let r2 = tool_read(wd, &serde_json::from_str("{\"path\":\"a/b.txt\"}").unwrap());
        assert_eq!(r2["content"], "bye");

        // missing old_text reports failure, does not crash
        let e2 = tool_edit(
            wd,
            &serde_json::from_str("{\"path\":\"a/b.txt\",\"old_text\":\"zzz\",\"new_text\":\"x\"}")
                .unwrap(),
        );
        assert_eq!(e2["ok"], false);
        assert!(e2["error"].as_str().unwrap().contains("not found"));

        // unknown tool
        let bad = run(wd, "nope", &serde_json::Value::Null);
        assert_eq!(bad["ok"], false);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn edit_replaces_only_first_occurrence() {
        let dir = std::env::temp_dir()
            .join("curam-h3-u")
            .join(format!("{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wd = &dir;
        tool_write(
            wd,
            &serde_json::from_str("{\"path\":\"f.txt\",\"content\":\"a b a\"}").unwrap(),
        );
        tool_edit(
            wd,
            &serde_json::from_str("{\"path\":\"f.txt\",\"old_text\":\"a\",\"new_text\":\"x\"}")
                .unwrap(),
        );
        let r = tool_read(wd, &serde_json::from_str("{\"path\":\"f.txt\"}").unwrap());
        assert_eq!(r["content"], "x b a");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_cage_fails_closed() {
        let dir = std::env::temp_dir()
            .join("curam-h4-t")
            .join(format!("{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wd = &dir;
        let cage = cage::Cage::at("/nonexistent/curam-run");
        assert!(!cage.available());
        let args = serde_json::from_str("{\"command\":\"echo hi\"}").unwrap();
        let binds = writable_binds(wd);
        let res = tool_shell(wd, &args, &cage, true, &binds, 60, false).unwrap();
        assert_eq!(res["ok"], false, "shell must fail closed without the cage");
        assert!(res["error"]
            .as_str()
            .unwrap()
            .contains("curam-run unavailable"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn schemas_are_valid_json() {
        let s = schemas();
        assert_eq!(s.len(), 6);
        assert_eq!(s[0]["function"]["name"], "read");
    }
}
