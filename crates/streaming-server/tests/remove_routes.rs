//! M1.4 - remove/removeAll respond `200 {}` without needing any torrent or
//! network (empty session, non-existent infohash). The streamed-bytes byte-diff
//! against the container needs reachable peers and is verified separately.
//!
//! These routes are POST-only (a mutation must not be reachable from a foreign
//! page's `<img src>` / navigation, which can only issue a GET), so the tests POST
//! and additionally assert a GET is rejected `405`.

use std::net::SocketAddr;

use serde_json::Value;
use rillio_streaming_server::{router, Config, Engine};

async fn spawn(tag: &str) -> String {
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    // Unique dir per test: parallel engines must not share session/DHT state.
    let dir = std::env::temp_dir().join(format!("stremio-remove-test-{tag}"));
    let engine = Engine::new(dir.clone()).await.unwrap();
    let app = router(Config::local(dir), engine);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

async fn post(base: &str, path: &str) -> (u16, Value) {
    let c = reqwest::Client::new();
    let r = c.post(format!("{base}{path}")).send().await.unwrap();
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or(Value::Null))
}

async fn get_status(base: &str, path: &str) -> u16 {
    reqwest::Client::new()
        .get(format!("{base}{path}"))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

#[tokio::test]
async fn remove_all_on_empty_session() {
    let base = spawn("all").await;
    let (status, body) = post(&base, "/removeAll").await;
    assert_eq!(status, 200);
    // The route reports what it did; on an empty session, nothing.
    assert_eq!(body, serde_json::json!({ "removed": [], "skipped": [], "failed": [] }));
    // GET (the `<img>`/navigation vector) must not drive this mutation.
    assert_eq!(get_status(&base, "/removeAll").await, 405);
}

#[tokio::test]
async fn remove_nonexistent_is_ok() {
    let base = spawn("one").await;
    // Well-formed infohash, not managed → still 200 {} (blob parity).
    let (status, body) = post(&base, "/dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c/remove").await;
    assert_eq!(status, 200);
    assert_eq!(body, serde_json::json!({}));
    assert_eq!(
        get_status(&base, "/dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c/remove").await,
        405
    );
}
