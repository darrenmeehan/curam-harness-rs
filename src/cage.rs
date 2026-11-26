//! cage.rs — curam-cage ndjson client (the exec boundary, proto/ndjson-spec
//! cage/1). The harness speaks the documented contract over stdio; it does
//! not import any curam-security code (T6: the contract replayable by any
//! consumer). Binary resolution is shared with `curam doctor`.

use serde_json::Value;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use super::config::{expand_home, CageCfg};

pub const ENVELOPE: &str = "cage/1";

/// One sandboxed execution request (cage/1 request envelope).
#[derive(Debug)]
pub struct CageRequest {
    pub id: u64,
    pub cmd: Vec<String>,
    pub cwd: Option<String>,
    pub timeout_s: Option<u64>,
    pub net: bool,
    pub geo: Option<String>,
    /// "host:guest" writable binds (e.g. the workdir + HOME).
    pub bind: Vec<String>,
    pub allow_hosts: Vec<String>,
    pub deny_hosts: Vec<String>,
}

/// The result envelope (cage/1 result).
#[derive(Debug)]
pub struct CageResult {
    pub id: u64,
    pub exit: u32,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub cages: Vec<String>,
    pub verdicts: Vec<Value>,
}

#[derive(Debug)]
pub struct Cage {
    pub binary: PathBuf,
}

/// Resolve the cage binary: CURAM_CAGE_BIN env > `[cage] binary`, used as a
/// path when it contains a '/', otherwise looked up on PATH.
///
/// Nothing else is consulted. An unresolved name comes back unchanged so the
/// caller reports it as missing: a released `curam` must never discover a
/// developer's checkout under $HOME.
pub fn resolve_binary(cfg: &CageCfg) -> PathBuf {
    if let Some(p) = std::env::var_os("CURAM_CAGE_BIN") {
        return PathBuf::from(p);
    }
    let from_cfg = expand_home(cfg.binary.as_str());
    // to_string_lossy, not {:?}: Debug quotes a PathBuf, so the old
    // format!("{from_cfg:?}") searched PATH for "\"curam-cage\"" (with the
    // quotes) and could never match. The $HOME dev-path fallback hid that.
    let name = from_cfg.to_string_lossy().into_owned();
    if name.contains('/') {
        return from_cfg;
    }
    path_search(&name).unwrap_or(from_cfg)
}

fn path_search(bin: &str) -> Option<PathBuf> {
    let path: String = std::env::var_os("PATH")
        .map(|p| PathBuf::from(p).to_str().unwrap_or_default().to_string())
        .unwrap_or_default();
    path.split(':')
        .map(|d| PathBuf::from(d).join(bin))
        .find(|cand| cand.exists())
}

impl Cage {
    /// A cage pinned to an explicit binary path (tests, unusual setups).
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self {
            binary: path.into(),
        }
    }

    pub fn from_cfg(cfg: &CageCfg) -> Self {
        Self {
            binary: resolve_binary(cfg),
        }
    }

    pub fn available(&self) -> bool {
        self.binary.exists()
    }

    fn build_request(req: &CageRequest) -> String {
        let mut m = serde_json::Map::new();
        m.insert("id".to_owned(), req.id.into());
        m.insert("v".to_owned(), ENVELOPE.into());
        m.insert("cmd".to_owned(), req.cmd.clone().into());
        if let Some(c) = &req.cwd {
            m.insert("cwd".to_owned(), c.as_str().into());
        }
        if let Some(t) = &req.timeout_s {
            m.insert("timeout_s".to_owned(), (*t).into());
        }
        m.insert("net".to_owned(), req.net.into());
        if let Some(g) = &req.geo {
            m.insert("geo".to_owned(), g.as_str().into());
        }
        m.insert("bind".to_owned(), req.bind.clone().into());
        m.insert("allow_hosts".to_owned(), req.allow_hosts.clone().into());
        m.insert("deny_hosts".to_owned(), req.deny_hosts.clone().into());
        serde_json::to_string(&serde_json::Value::Object(m)).expect("cage request serializes")
    }

    fn parse_result(line: &str) -> Option<CageResult> {
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => return None,
        };
        if v.get("error").is_some() {
            return None;
        }
        let verdicts: Vec<Value> = v
            .get("verdicts")
            .and_then(|a| a.as_array())
            .map(|a| a.to_vec())
            .unwrap_or_default();
        let cages: Vec<String> = v
            .get("cages")
            .and_then(|a| a.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|e| e.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Some(CageResult {
            id: v.get("id").and_then(|x| x.as_u64()).unwrap_or(0),
            exit: v.get("exit").and_then(|x| x.as_u64()).unwrap_or(1) as u32,
            stdout: v
                .get("stdout")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string(),
            stderr: v
                .get("stderr")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string(),
            timed_out: v
                .get("timed_out")
                .and_then(|x| x.as_bool())
                .unwrap_or(false),
            cages,
            verdicts,
        })
    }

    /// Run one request in the cage. Returns None when the binary is missing
    /// or the request failed (the caller applies [cage] fallback).
    pub fn run(&self, req: &CageRequest) -> Result<Option<CageResult>, String> {
        if !self.available() {
            return Ok(None);
        }
        let mut cmd = Command::new(&self.binary);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(_) => return Ok(None),
        };
        let line = Self::build_request(req);
        if let Some(mut si) = child.stdin.take() {
            let _ = si.write_all(line.as_bytes());
        }
        // Safety net: curam-cage enforces the internal timeout; cap the
        // client-side wait at timeout + 30s so a wedged cage can't hang us.
        let timeout = req.timeout_s.unwrap_or(0);
        let deadline = if timeout > 0 {
            Some(SystemTime::now() + Duration::from_secs(timeout + 30))
        } else {
            None
        };
        let mut timed_out = false;
        loop {
            if let Some(dl) = deadline {
                if SystemTime::now() >= dl {
                    let _ = child.kill();
                    timed_out = true;
                }
            }
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(_) => break,
            }
        }
        let mut out = String::new();
        if let Some(mut so) = child.stdout.take() {
            let _ = so.read_to_string(&mut out);
        }
        let _ = child.wait().unwrap_or_default();
        let _ = timed_out;
        // The kernel-level timeout is reported in the result line; the
        // client cap only protects against a hung binary.
        let last = out.split('\n').rfind(|l| !l.trim().is_empty());
        let line_ptr = match last {
            Some(l) => l,
            None => "",
        };
        Ok(Self::parse_result(line_ptr))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_result_envelope_shape() {
        let line = "{\"id\":7,\"exit\":3,\"stdout\":\"hi\\n\",\"stderr\":\"\",\"timed_out\":false,\"cages\":[\"bwrap:unshare-net\"],\"verdicts\":[{\"action\":\"deny\",\"rule_id\":\"host_deny\",\"source\":\"deny list\",\"source_hash\":\"cage/1\",\"evidence\":\"evil.example\"}]}";
        let r = Cage::parse_result(line);
        assert!(r.is_some());
        let r = r.unwrap();
        assert_eq!(r.id, 7);
        assert_eq!(r.exit, 3);
        assert_eq!(r.stdout, "hi\n");
        assert_eq!(r.cages.len(), 1);
        assert_eq!(r.verdicts[0]["rule_id"], "host_deny");
    }

    #[test]
    fn parse_result_error_is_none() {
        assert!(Cage::parse_result("{\"error\":\"bad json\"}").is_none());
        assert!(Cage::parse_result("not json").is_none());
    }

    #[test]
    fn build_request_contract_fields() {
        let req = CageRequest {
            id: 1,
            cmd: vec!["bash".into(), "-c".into(), "echo hi".into()],
            cwd: Some("/tmp".into()),
            timeout_s: Some(5),
            net: false,
            geo: None,
            bind: vec!["/home/x:/home/x".into()],
            allow_hosts: vec!["github.com".into()],
            deny_hosts: vec!["evil.example".into()],
        };
        let text = Cage::build_request(&req);
        let v: serde_json::Value = serde_json::from_str(text.as_str()).expect("valid json");
        assert_eq!(v["v"], "cage/1");
        assert_eq!(v["cmd"][0], "bash");
        assert_eq!(v["timeout_s"], 5);
        assert_eq!(v["net"], false);
    }

    fn cage_cfg(binary: &str) -> CageCfg {
        CageCfg {
            binary: binary.into(),
            lists_dir: "~/.local/share/curam/lists".into(),
            geo: String::new(),
            fallback: "deny".into(),
        }
    }

    #[test]
    fn resolve_uses_path_for_a_bare_name() {
        // Regression: the search name was built with {:?}, which quotes a
        // PathBuf, so a bare name never matched anything on PATH.
        let found = resolve_binary(&cage_cfg("sh"));
        assert!(found.exists(), "sh must resolve via PATH, got {found:?}");
    }

    #[test]
    fn resolve_never_invents_a_dev_path() {
        let cfg = cage_cfg("curam-cage-not-installed");
        assert_eq!(
            resolve_binary(&cfg),
            PathBuf::from("curam-cage-not-installed"),
            "an unresolved name must come back unchanged"
        );
        assert!(!Cage::from_cfg(&cfg).available());
    }
}
