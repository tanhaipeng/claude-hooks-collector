use std::{env, error::Error};

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

pub struct Config {
    pub(crate) listen_addr: String,
    pub(crate) otlp_endpoint: String,
    pub(crate) otlp_headers: HeaderMap,
    pub(crate) otlp_headers_configured: bool,
    pub(crate) langfuse: bool,
    pub(crate) service_name: String,
    pub(crate) queue_capacity: usize,
    pub(crate) ttl_seconds: u64,
    pub(crate) ttl_sweep_seconds: u64,
}

impl Config {
    pub fn from_env() -> Result<Self, Box<dyn Error>> {
        let listen_addr = env::var("LISTEN_ADDR").unwrap_or_else(|_| "127.0.0.1:4321".to_string());
        let otlp_endpoint = normalize_otlp_endpoint(
            &env::var("OTLP_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:4318".to_string()),
        );
        let otlp_headers = parse_otlp_headers(&env::var("OTLP_HEADERS").unwrap_or_default());
        let otlp_headers_configured = !otlp_headers.is_empty();
        let service_name =
            env::var("SERVICE_NAME").unwrap_or_else(|_| "claude-code-hooks".to_string());
        let backend = env::var("OTEL_BACKEND")
            .unwrap_or_default()
            .to_ascii_lowercase();
        let langfuse = match backend.as_str() {
            "langfuse" => true,
            "jaeger" | "generic" | "otel" => false,
            _ => {
                otlp_endpoint.contains("langfuse")
                    || otlp_endpoint.contains("/api/public/otel")
                    || otlp_endpoint.contains(":3000")
            }
        };
        let queue_capacity = env::var("QUEUE_CAPACITY")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(4096)
            .max(1);
        let ttl_seconds = env::var("SESSION_TTL_SECS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(3600);
        let ttl_sweep_seconds = env::var("TTL_SWEEP_SECS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(60);

        Ok(Self {
            listen_addr,
            otlp_endpoint,
            otlp_headers,
            otlp_headers_configured,
            langfuse,
            service_name,
            queue_capacity,
            ttl_seconds,
            ttl_sweep_seconds,
        })
    }
}

pub(crate) fn parse_otlp_headers(value: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for part in value.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let Some((name, value)) = part.split_once('=') else {
            tracing::warn!("ignoring malformed OTLP_HEADERS entry without '='");
            continue;
        };
        let name = name.trim();
        let value = value.trim();
        match (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            (Ok(name), Ok(value)) => {
                headers.insert(name, value);
            }
            _ => tracing::warn!(header = %name, "ignoring invalid OTLP_HEADERS entry"),
        }
    }
    headers
}

pub(crate) fn normalize_otlp_endpoint(value: &str) -> String {
    let trimmed = value.trim_end_matches('/');
    if trimmed.ends_with("/v1/traces") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/v1/traces")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_jaeger_and_langfuse_endpoints() {
        assert_eq!(
            normalize_otlp_endpoint("http://127.0.0.1:4318"),
            "http://127.0.0.1:4318/v1/traces"
        );
        assert_eq!(
            normalize_otlp_endpoint("http://127.0.0.1:3000/api/public/otel/"),
            "http://127.0.0.1:3000/api/public/otel/v1/traces"
        );
        assert_eq!(
            normalize_otlp_endpoint("https://example.test/v1/traces"),
            "https://example.test/v1/traces"
        );
    }

    #[test]
    fn detects_langfuse_endpoint() {
        assert!(
            normalize_otlp_endpoint("http://127.0.0.1:3000/api/public/otel")
                .contains("/api/public/otel")
        );
        assert!(!normalize_otlp_endpoint("http://127.0.0.1:4318").contains("langfuse"));
    }

    #[test]
    fn parses_multiple_headers() {
        let headers =
            parse_otlp_headers("Authorization=Basic abc==,x-langfuse-ingestion-version=4");
        assert_eq!(
            headers.get("authorization").unwrap().to_str().unwrap(),
            "Basic abc=="
        );
        assert_eq!(
            headers
                .get("x-langfuse-ingestion-version")
                .unwrap()
                .to_str()
                .unwrap(),
            "4"
        );
    }
}
