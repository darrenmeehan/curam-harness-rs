//! tools.rs — the model's file tools: read / write / edit (port of
//! tools.py). shell + the typed run tool land in H4 behind the cage; the
//! tool *schemas* advertised to the provider grow with them.
//!
//! Path handling matches the POC (resolve inside the workdir; the harness
//! self-sandbox is a deferred release — the exec boundary is the cage).

use serde_json::Value;
use std::path::Path;
use std::sync::OnceLock;

pub const MAX_READ_CHARS: usize = 60_000;
pub const MAX_OUTPUT_CHARS: usize = 8_000;

/// The tool schemas sent to the provider (OpenAI function shape). H3 ships
/// the file tools; shell/run/ask_human/use_skill are added as they land.
const SCHEMA_JSONS: &[&str] = &[
    "{\"type\":\"function\",\"function\":{\"name\":\"read\",\"description\":\"Read a file within the workdir.\",\"parameters\":{\"type\":\"object\",\"properties\":{\"path\":{\"type\":\"string\",\"description\":\"relative path\"}},\"required\":[\"path\"]}}}",
    "{\"type\":\"function\",\"function\":{\"name\":\"write\",\"description\":\"Write a file within the workdir (overwrites).\",\"parameters\":{\"type\":\"object\",\"properties\":{\"path\":{\"type\":\"string\"},\"content\":{\"type\":\"string\"}},\"required\":[\"path\",\"content\"]}}}",
    "{\"type\":\"function\",\"function\":{\"name\":\"edit\",\"description\":\"Replace the first occurrence of old_text with new_text in a file.\",\"parameters\":{\"type\":\"object\",\"properties\":{\"path\":{\"type\":\"string\"},\"old_text\":{\"type\":\"string\"},\"new_text\":{\"type\":\"string\"}},\"required\":[\"path\",\"old_text\",\"new_text\"]}}}",
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
    args.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
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
    name == "read" || name == "write" || name == "edit"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_read_edit_roundtrip() {
        let dir = std::env::temp_dir().join("curam-h3-t").join(format!("{}", std::process::id()));
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
            &serde_json::from_str("{\"path\":\"a/b.txt\",\"old_text\":\"hello\",\"new_text\":\"bye\"}").unwrap(),
        );
        assert_eq!(e["ok"], true);
        let r2 = tool_read(wd, &serde_json::from_str("{\"path\":\"a/b.txt\"}").unwrap());
        assert_eq!(r2["content"], "bye");

        // missing old_text reports failure, does not crash
        let e2 = tool_edit(
            wd,
            &serde_json::from_str("{\"path\":\"a/b.txt\",\"old_text\":\"zzz\",\"new_text\":\"x\"}").unwrap(),
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
        let dir = std::env::temp_dir().join("curam-h3-u").join(format!("{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wd = &dir;
        tool_write(wd, &serde_json::from_str("{\"path\":\"f.txt\",\"content\":\"a b a\"}").unwrap());
        tool_edit(wd, &serde_json::from_str("{\"path\":\"f.txt\",\"old_text\":\"a\",\"new_text\":\"x\"}").unwrap());
        let r = tool_read(wd, &serde_json::from_str("{\"path\":\"f.txt\"}").unwrap());
        assert_eq!(r["content"], "x b a");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn schemas_are_valid_json() {
        let s = schemas();
        assert_eq!(s.len(), 3);
        assert_eq!(s[0]["function"]["name"], "read");
    }
}