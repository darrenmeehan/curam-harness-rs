//! journal.rs — SQLite WAL session journal (ported 1:1 from journal.py).
//!
//! Every event is a row in `events` (JSON payload + timestamp + session_id);
//! the SLI surface is one query over this table (docs/curam-slos.md). WAL
//! mode + synchronous=NORMAL for a crash-safe journal; `export_jsonl` keeps
//! the human-readable view. The schema matches journal.py exactly, so
//! existing POC journals are readable as-is (the H2 resumability gate).

use rusqlite::{Connection, params};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS events (
    seq        INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT    NOT NULL,
    ts         REAL    NOT NULL,
    etype      TEXT    NOT NULL,
    payload    TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_events_session ON events(session_id, seq);
CREATE INDEX IF NOT EXISTS idx_events_type    ON events(etype);
";

/// One journal row with its payload parsed to JSON.
#[derive(Debug)]
pub struct EventRow {
    pub seq: u64,
    pub session_id: String,
    pub ts: f64,
    pub etype: String,
    pub payload: Value,
}

#[derive(Debug)]
pub struct Journal {
    pub path: PathBuf,
    pub session_id: String,
    conn: Connection,
}

/// The only dialect wrinkle: journal payloads are JSON blobs, never SQL.
fn payload_of(v: &Value) -> String {
    serde_json::to_string(v).expect("payload serializes")
}

pub fn now_ts() -> f64 {
    let now = std::time::SystemTime::now();
    let dur = match now.duration_since(std::time::SystemTime::UNIX_EPOCH) {
        Ok(d) => d,
        Err(_) => std::time::Duration::from_secs_f64(0.0),
    };
    dur.as_secs_f64()
}

fn fresh_session_id() -> String {
    format!("{:x}{:x}", std::process::id(), (now_ts() as u64))
}

impl Journal {
    /// Open (creating parents + schema). `session_id` defaults to a fresh id.
    pub fn open(path: impl Into<PathBuf>, session_id: Option<String>) -> Result<Self, String> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{parent:?}: {e}"))?;
        }
        let conn = Connection::open(&path).map_err(|e| format!("open journal {path:?}: {e}"))?;
        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .map_err(|e| format!("journal WAL pragma: {e}"))?;
        conn.execute_batch("PRAGMA synchronous=NORMAL;")
            .map_err(|e| format!("journal synchronous pragma: {e}"))?;
        conn.execute_batch(SCHEMA).map_err(|e| format!("journal schema: {e}"))?;
        let sid = session_id.unwrap_or_else(fresh_session_id);
        Ok(Self { path, session_id: sid, conn })
    }

    /// Append one event. Returns the rowid (like journal.py's lastrowid).
    pub fn log(&mut self, etype: &str, payload: &Value) -> Result<Option<u64>, String> {
        let row = params!(self.session_id, now_ts(), etype, payload_of(payload));
        let res = self.conn.execute(
            "INSERT INTO events(session_id, ts, etype, payload) VALUES (?,?,?,?)",
            row,
        );
        match res {
            Ok(_) => Ok(Some(self.conn.last_insert_rowid() as u64)),
            Err(e) => Err(format!("journal write: {e}")),
        }
    }

    /// All events, optionally filtered by type, in write order.
    pub fn events(&mut self, etype: Option<&str>) -> Result<Vec<EventRow>, String> {
        let mut stmt = self.conn
            .prepare("SELECT seq, session_id, ts, etype, payload FROM events ORDER BY seq")
            .map_err(|e| format!("journal query: {e}"))?;
        let mut rows = stmt.query(params!()).map_err(|e| format!("journal query: {e}"))?;
        let mut out = Vec::new();
        let mut iter_done = false;
        while !iter_done {
            let row = rows.next();
            match row {
                Ok(Some(r)) => {
            let seq = r.get::<usize, i64>(0).unwrap_or_default();
            let sid = r.get::<usize, String>(1).unwrap_or_default();
            let ts = r.get::<usize, f64>(2).unwrap_or_default();
            let et = r.get::<usize, String>(3).unwrap_or_default();
            if let Some(want) = etype {
                if et != want {
                    continue;
                }
            }
            let raw = r.get::<usize, String>(4).unwrap_or_default();
            let payload = match serde_json::from_str(raw.as_str()) {
                Ok(v) => v,
                Err(_) => serde_json::Value::Null, // tolerate a bad row; never crash
            };
            out.push(EventRow {
                seq: seq as u64,
                session_id: sid,
                ts,
                etype: et,
                payload,
            });
                }
                Ok(None) => iter_done = true,
                Err(e) => return Err(format!("journal iterate: {e}")),
            }
        }

        Ok(out)
    }

    /// Human-readable export: one JSON object per event (journal.py format).
    pub fn export_jsonl(&mut self, path: Option<&Path>) -> Result<String, String> {
        let evs = self.events(None)?;
        let mut lines = Vec::new();
        for e in evs {
            let mut m = serde_json::Map::new();
            m.insert("seq".to_owned(), e.seq.into());
            m.insert("ts".to_owned(), e.ts.into());
            m.insert("etype".to_owned(), e.etype.into());
            if let Some(obj) = e.payload.as_object() { for (k, v) in obj.iter() {
                m.insert(k.to_string(), v.clone());
            } }
            lines.push(serde_json::to_string(&serde_json::Value::Object(m)).expect("jsonl row"));
        }
        let text = lines.join("\n") + "\n";
        if let Some(p) = path {
            std::fs::write(p, text.as_bytes()).map_err(|e| format!("{p:?}: {e}"))?;
        }
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_journal(tag: &str, session: Option<String>) -> Journal {
        let dir = std::env::temp_dir().join(format!("curam-h2-j-{tag}")).join(format!("{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("j.db");
        Journal::open(p, session).unwrap()
    }

    #[test]
    fn log_read_reopen_resume() {
        let mut j = tmp_journal("a", Some("sess1".into()));
        let e1 = j.log("user_message", &serde_json::from_str("{\"text\":\"hi\"}").unwrap()).unwrap();
        assert!(e1.is_some() && e1.unwrap() > 0);
        j.log("tool_call", &serde_json::from_str("{\"name\":\"read\"}").unwrap()).unwrap();
        let all = j.events(None).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].session_id, "sess1");
        assert_eq!(all[0].etype, "user_message");
        assert_eq!(all[0].payload["text"], "hi");
        assert_eq!(j.events(Some("tool_call")).unwrap().len(), 1);

        // Resumability: reopen the same file, the rows are still there.
        let mut j2 = Journal::open(j.path.clone(), Some("sess1".into())).unwrap();
        assert_eq!(j2.events(None).unwrap().len(), 2);
        std::fs::remove_dir_all(&j.path.parent().unwrap()).ok();
    }

    #[test]
    fn export_jsonl_lists_all_events() {
        let mut j = tmp_journal("b", Some("sess2".into()));
        j.log("user_message", &serde_json::from_str("{\"text\":\"hi\"}").unwrap()).unwrap();
        let out = j.export_jsonl(None).unwrap();
        assert!(out.contains("\"seq\":1"), "jsonl starts with seq");
        assert!(out.contains("\"etype\":\"user_message\""), "etype present");
        assert!(out.contains("\"text\":\"hi\""), "payload present");
        std::fs::remove_dir_all(&j.path.parent().unwrap()).ok();
    }
}