//! Orbio gateway compatibility: attestation status codes, model catalog,
//! chat `inference-id`, signature text, and a canary that must not be logged.

#[path = "common/mod.rs"]
mod common;

use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CANARY: &str = "CANARY_PROMPT_orbio_9f3a2c";
const KEY_CANARY: &str = "KEYCANARY_aabbccddeeff00112233445566778899";

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl Logs {
    fn writer(&self) -> LogWriter {
        LogWriter(self.0.clone())
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

struct LogWriter(Arc<Mutex<Vec<u8>>>);

impl Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

async fn read_body(
    response: axum::response::Response,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let status = response.status();
    let headers = response.headers().clone();
    use http_body_util::BodyExt;
    let bytes = response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, headers, bytes)
}

#[tokio::test]
async fn attestation_rejects_wrong_model_and_unknown_signing_address() {
    let app = common::build_test_app("http://127.0.0.1:9", common::TestAppOptions::default());
    let nonce = "ab".repeat(32);

    let missing = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/v1/attestation/report?model=other-model&signing_algo=ed25519&include_tls_fingerprint=false&nonce={nonce}"
                ))
                .header("x-no-aliasing", "true")
                .header("authorization", "Bearer test-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);

    let unknown_key = app
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/v1/attestation/report?model=test-model&provider=orbio&signing_algo=ed25519&include_tls_fingerprint=false&signing_address={}&nonce={nonce}",
                    "11".repeat(32)
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, _, body) = read_body(unknown_key).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let text = String::from_utf8(body).unwrap();
    assert!(text.contains("Signing address not found"), "{text}");
}

#[tokio::test]
async fn model_list_is_public_near_shape() {
    let app = common::build_test_app("http://127.0.0.1:9", common::TestAppOptions::default());
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/model/list?limit=10&offset=0")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, _, body) = read_body(response).await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["total"], 1);
    assert_eq!(json["limit"], 10);
    assert_eq!(json["models"][0]["modelId"], "test-model");
    assert_eq!(json["models"][0]["metadata"]["isReady"], true);
    assert_eq!(json["models"][0]["metadata"]["contextLength"], 262144);
    assert_eq!(json["models"][0]["metadata"]["maxOutputLength"], 8192);
    assert_eq!(json["models"][0]["metadata"]["providerType"], "vllm");
    assert_eq!(json["models"][0]["inputCostPerToken"]["scale"], 9);
    assert_eq!(json["models"][0]["inputCostPerToken"]["currency"], "USD");
    assert_eq!(json["models"][0]["outputCostPerToken"]["amount"], 0);

    let page = app
        .oneshot(
            Request::builder()
                .uri("/v1/model/list?offset=1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, _, body) = read_body(page).await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json["models"].as_array().unwrap().is_empty());
    assert_eq!(json["total"], 1);
}

#[tokio::test]
async fn chat_sets_inference_id_signature_and_does_not_log_the_prompt() {
    let server = MockServer::start().await;
    let upstream = serde_json::json!({
        "id": "chatcmpl-orbio",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "finish_reason": "stop",
            "message": {
                "role": "assistant",
                "content": "pong",
                "reasoning_content": ""
            }
        }],
        "usage": {
            "prompt_tokens": 4,
            "completion_tokens": 1,
            "prompt_tokens_details": {"cached_tokens": 2}
        }
    });
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(upstream))
        .expect(1)
        .mount(&server)
        .await;

    let app = common::build_test_app(&server.uri(), common::TestAppOptions::default());
    let logs = Logs::default();
    let writer_logs = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_ansi(false)
        .with_writer(move || writer_logs.writer())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let request_body = format!(
        r#"{{"model":"test-model","stream":false,"messages":[{{"role":"user","content":"{CANARY}"}}]}}"#
    );
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("authorization", "Bearer test-token")
                .header("content-type", "application/json")
                .header("x-model-pub-key", KEY_CANARY)
                .body(Body::from(request_body.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, headers, body) = read_body(response).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert!(headers
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .contains("application/json"));
    assert!(headers.get("content-encoding").is_none());
    let inference_id = headers.get("inference-id").unwrap().to_str().unwrap();
    let parsed = uuid::Uuid::parse_str(inference_id).unwrap();
    assert_eq!(parsed.get_version(), Some(uuid::Version::Random));

    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["choices"].as_array().unwrap().len(), 1);
    assert_eq!(json["choices"][0]["finish_reason"], "stop");
    assert_eq!(json["choices"][0]["message"]["content"], "pong");
    assert_eq!(json["usage"]["prompt_tokens"], 4);
    assert_eq!(json["usage"]["completion_tokens"], 1);
    assert_eq!(json["usage"]["prompt_tokens_details"]["cached_tokens"], 2);
    let id = json["id"].as_str().unwrap();

    let expected_text = format!(
        "test-model:{}:{}",
        sha256_hex(request_body.as_bytes()),
        sha256_hex(&body)
    );
    let signature = app
        .oneshot(
            Request::builder()
                .uri(format!("/v1/signature/{id}?signing_algo=ed25519"))
                .header("authorization", "Bearer test-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, headers, sig_body) = read_body(signature).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&sig_body)
    );
    assert!(headers.get("content-encoding").is_none());
    let sig: serde_json::Value = serde_json::from_slice(&sig_body).unwrap();
    assert_eq!(sig["text"], expected_text);
    assert_eq!(sig["signing_algo"], "ed25519");
    assert_eq!(sig["signature_kind"], "provider_tee");
    assert_eq!(sig["signature"].as_str().unwrap().len(), 128);
    assert!(sig["signature"]
        .as_str()
        .unwrap()
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    assert!(!sig_body
        .windows(CANARY.len())
        .any(|w| w == CANARY.as_bytes()));

    let captured = logs.text();
    assert!(
        captured.contains("request completed") || captured.contains("request finished"),
        "expected a metadata log line, got: {captured}"
    );
    for forbidden in [CANARY, KEY_CANARY, "test-token", "Bearer"] {
        assert!(
            !captured.contains(forbidden),
            "log leaked {forbidden}: {captured}"
        );
    }
}

#[tokio::test]
async fn wrong_model_pub_key_is_421_before_upstream() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let app = common::build_test_app(&server.uri(), common::TestAppOptions::default());
    // RFC 8032 test 1 public key: a real point, not this process's key.
    let client_pub = "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("authorization", "Bearer test-token")
                .header("content-type", "application/json")
                .header("x-signing-algo", "ed25519")
                .header("x-client-pub-key", client_pub)
                .header("x-model-pub-key", "11".repeat(32))
                .header("x-encryption-version", "2")
                .header("x-encrypt-all-fields", "true")
                .body(Body::from(
                    r#"{"model":"test-model","messages":[{"role":"user","content":"hi"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, _, body) = read_body(response).await;
    assert_eq!(status, StatusCode::MISDIRECTED_REQUEST);
    let text = String::from_utf8(body).unwrap();
    assert!(!text.contains(&"11".repeat(32)));
    assert!(!text.contains(client_pub));
}
