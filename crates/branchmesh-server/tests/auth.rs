use axum::body::Body;
use axum::http::{Request, StatusCode};
use branchmesh::{MockProvider, Runtime};
use branchmesh_server::{router, AppState, TokenAuthority, TokenCheck};
use std::sync::Arc;
use tower::ServiceExt;

struct AppTokens;
impl TokenAuthority for AppTokens {
    fn check(&self, token: &str) -> TokenCheck {
        if token.starts_with("app_") { TokenCheck::NotOperator } else { TokenCheck::Unknown }
    }
}

async fn status(app: &axum::Router, token: Option<&str>) -> StatusCode {
    let mut req = Request::get("/api/stats");
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    app.clone().oneshot(req.body(Body::empty()).unwrap()).await.unwrap().status()
}

#[tokio::test]
async fn operator_api_rejects_application_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let rt = Runtime::builder(dir.path().join("db")).with_llm(Arc::new(MockProvider::instant())).open().await.unwrap();

    // Open mode: anonymous callers are operators, application tokens are not.
    let open = router(AppState::new(rt.clone(), None).with_authority(Arc::new(AppTokens)));
    assert_eq!(status(&open, None).await, StatusCode::OK);
    assert_eq!(status(&open, Some("app_user")).await, StatusCode::FORBIDDEN);

    let locked = router(AppState::new(rt, Some("secret".into())).with_authority(Arc::new(AppTokens)));
    assert_eq!(status(&locked, None).await, StatusCode::UNAUTHORIZED);
    assert_eq!(status(&locked, Some("wrong")).await, StatusCode::UNAUTHORIZED);
    assert_eq!(status(&locked, Some("app_user")).await, StatusCode::FORBIDDEN);
    assert_eq!(status(&locked, Some("secret")).await, StatusCode::OK);
}
