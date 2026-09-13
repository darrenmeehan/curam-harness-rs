//! util.rs — tiny shared helpers (JSON object builders used across modules).

use serde_json::Value;

/// Build a singleton JSON object: `{"k": v}`.
pub fn mapof(k: &str, v: Value) -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    m.insert(k.to_owned(), v);
    m
}

/// Build a two-key JSON object: `{"k1": v1, "k2": v2}`.
pub fn mapof2(k1: &str, v1: Value, k2: &str, v2: Value) -> serde_json::Map<String, Value> {
    let mut m = mapof(k1, v1);
    m.insert(k2.to_owned(), v2);
    m
}