use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use branchmesh::{AgentPersona, MockProvider, Runtime};
use branchmesh_server::{router, AppState};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, 0x49, 0x48, 0x44, 0x52, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0,
    0, 0, 0x1F, 0x15, 0xC4, 0x89, 0, 0, 0, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0, 1, 0, 0, 5, 0, 1, 0x0D, 0x0A,
    0x2D, 0xB4, 0, 0, 0, 0, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

async fn call(app: &axum::Router, req: Request<Body>) -> (StatusCode, Vec<u8>) {
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    (status, to_bytes(res.into_body(), usize::MAX).await.unwrap().to_vec())
}

#[tokio::test]
async fn upload_fetch_and_send_images() {
    let dir = tempfile::tempdir().unwrap();
    let rt = Runtime::builder(dir.path().join("db")).with_llm(Arc::new(MockProvider::instant())).open().await.unwrap();
    rt.mesh().save_persona(&AgentPersona::new("bot")).unwrap();
    rt.repo().create_branch("t", json!({})).unwrap();
    let app = router(AppState::new(rt, None));

    // Upload: sniffed type, not the declared one.
    let (st, body) = call(&app, Request::post("/api/media").header("content-type", "text/plain").body(Body::from(PNG)).unwrap()).await;
    assert_eq!(st, StatusCode::OK);
    let media: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(media["media_type"], "image/png");
    let hash = media["hash"].as_str().unwrap().to_string();

    // Not an image / too large.
    let (st, _) = call(&app, Request::post("/api/media").body(Body::from("<svg/>")).unwrap()).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let big = vec![0xFFu8; 6 * 1024 * 1024];
    let (st, _) = call(&app, Request::post("/api/media").body(Body::from(big)).unwrap()).await;
    assert_eq!(st, StatusCode::PAYLOAD_TOO_LARGE);

    // Fetch.
    let res = app.clone().oneshot(Request::get(format!("/api/media/{hash}")).body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()["content-type"], "image/png");
    assert_eq!(to_bytes(res.into_body(), usize::MAX).await.unwrap().as_ref(), PNG);
    let (st, _) = call(&app, Request::get(format!("/api/media/{}", "0".repeat(64))).body(Body::empty()).unwrap()).await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // Send an image-only message.
    let send = json!({ "branch": "t", "persona": "bot", "attachments": [hash], "wait": true });
    let (st, body) = call(&app, Request::post("/api/branch/send").header("content-type", "application/json").body(Body::from(send.to_string())).unwrap()).await;
    assert_eq!(st, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let reply: Value = serde_json::from_slice(&body).unwrap();
    assert!(reply["reply"]["text"].as_str().unwrap().contains("I can see 1 image(s)"), "{reply}");

    let bogus = json!({ "branch": "t", "persona": "bot", "content": "hi", "attachments": ["0".repeat(64)] });
    let (st, _) = call(&app, Request::post("/api/branch/send").header("content-type", "application/json").body(Body::from(bogus.to_string())).unwrap()).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}
