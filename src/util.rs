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

/// Build a four-key JSON object (skills, verdicts, ...).
pub fn mapof4(
    k1: &str,
    v1: Value,
    k2: &str,
    v2: Value,
    k3: &str,
    v3: Value,
    k4: &str,
    v4: Value,
) -> serde_json::Map<String, Value> {
    let mut m = mapof2(k1, v1, k2, v2);
    m.insert(k3.to_owned(), v3);
    m.insert(k4.to_owned(), v4);
    m
}
