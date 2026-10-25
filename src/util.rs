use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::Value;

pub(crate) static SPAN_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

pub(crate) fn hex_id(input: &str, bytes: usize) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(bytes * 2);
    for byte in &digest[..bytes] {
        out.push_str(&format!("{byte:02x}"));
    }
    if out.bytes().all(|byte| byte == b'0') {
        out.replace_range(0..1, "1");
    }
    out
}

pub(crate) fn unique_span_id(seed: &str) -> String {
    let counter = SPAN_COUNTER.fetch_add(1, Ordering::Relaxed);
    hex_id(&format!("{seed}:{counter}:{}", now_ns()), 8)
}

pub(crate) fn string_field(payload: &Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

pub(crate) fn scalar_field(payload: &Value, key: &str) -> Option<String> {
    match payload.get(key)? {
        Value::String(value) => Some(value.clone()),
        Value::Bool(value) => Some(value.to_string()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

pub(crate) fn u64_field(payload: &Value, key: &str) -> Option<u64> {
    payload.get(key).and_then(Value::as_u64)
}

pub(crate) fn bool_field(payload: &Value, key: &str) -> Option<bool> {
    payload.get(key).and_then(Value::as_bool)
}

pub(crate) fn set_attr(attributes: &mut Vec<(String, String)>, key: &str, value: String) {
    if let Some(existing) = attributes.iter_mut().find(|(known, _)| known == key) {
        existing.1 = value;
    } else {
        attributes.push((key.to_string(), value));
    }
}

pub(crate) fn add_scalar(
    attributes: &mut Vec<(String, String)>,
    payload: &Value,
    key: &str,
    attr: &str,
) {
    if let Some(value) = scalar_field(payload, key) {
        attributes.push((attr.to_string(), value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_expected_hex_lengths() {
        assert_eq!(hex_id("hello", 8).len(), 16);
        assert_eq!(hex_id("hello", 16).len(), 32);
    }

    #[test]
    fn unique_span_ids_differ() {
        assert_ne!(unique_span_id("test"), unique_span_id("test"));
    }

    #[test]
    fn extracts_common_payload_fields() {
        let payload = serde_json::json!({
            "session_id": "s1",
            "stop_hook_active": false,
            "count": 3
        });
        assert_eq!(string_field(&payload, "session_id").as_deref(), Some("s1"));
        assert_eq!(bool_field(&payload, "stop_hook_active"), Some(false));
        assert_eq!(u64_field(&payload, "count"), Some(3));
    }
}
