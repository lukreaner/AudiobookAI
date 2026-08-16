#[cfg(feature = "embedded-dashboard")]
use axum::{
    Router,
    body::Body,
    extract::Path,
    http::{HeaderValue, Response, StatusCode, header},
    response::IntoResponse,
    routing::get,
};
#[cfg(feature = "embedded-dashboard")]
use rust_embed::RustEmbed;

#[cfg(feature = "embedded-dashboard")]
#[derive(RustEmbed)]
#[folder = "../../web/dist"]
struct DashboardAssets;

#[cfg(feature = "embedded-dashboard")]
pub fn router() -> Router {
    Router::new()
        .route("/", get(index))
        .route("/{*path}", get(asset))
}

#[cfg(not(feature = "embedded-dashboard"))]
pub fn router() -> axum::Router {
    axum::Router::new()
}

#[cfg(feature = "embedded-dashboard")]
async fn index() -> impl IntoResponse {
    response_for("index.html", false)
}

#[cfg(feature = "embedded-dashboard")]
async fn asset(Path(path): Path<String>) -> impl IntoResponse {
    let normalized = path.trim_start_matches('/');
    let is_asset = normalized.contains('.');
    response_for(if is_asset { normalized } else { "index.html" }, is_asset)
}

#[cfg(feature = "embedded-dashboard")]
fn response_for(path: &str, immutable: bool) -> Response<Body> {
    let Some(asset) = DashboardAssets::get(path) else {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::empty())
            .expect("valid response");
    };
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let cache = if immutable {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime.as_ref())
        .header(header::CACHE_CONTROL, cache)
        .header(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(
                "default-src 'self'; connect-src 'self'; img-src 'self' data: blob:; media-src 'self' blob:; style-src 'self' 'unsafe-inline'; script-src 'self'; worker-src 'self' blob:; object-src 'none'; frame-ancestors 'none'; base-uri 'self'; form-action 'self'",
            ),
        )
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(header::X_FRAME_OPTIONS, "DENY")
        .header(header::REFERRER_POLICY, "no-referrer")
        .header("permissions-policy", "camera=(), microphone=(), geolocation=(), payment=(), usb=()")
        .header("cross-origin-resource-policy", "same-origin")
        .body(Body::from(asset.data))
        .expect("valid embedded asset response")
}

#[cfg(all(test, feature = "embedded-dashboard"))]
mod tests {
    use axum::{
        body::to_bytes,
        http::{Request, header},
    };
    use tower::ServiceExt;

    use super::*;

    const DASHBOARD_RESPONSE_LIMIT: usize = 16 * 1024 * 1024;

    async fn request(path: &str) -> Response<Body> {
        router()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .body(Body::empty())
                    .expect("valid dashboard request"),
            )
            .await
            .expect("dashboard router response")
    }

    #[tokio::test]
    async fn embedded_dashboard_serves_entrypoint_and_referenced_asset() {
        let root = request("/").await;
        assert_eq!(root.status(), StatusCode::OK);
        assert_eq!(root.headers()[header::CACHE_CONTROL], "no-cache");
        assert_eq!(root.headers()[header::CONTENT_TYPE], "text/html");
        let root_body = to_bytes(root.into_body(), DASHBOARD_RESPONSE_LIMIT)
            .await
            .expect("dashboard root body");
        assert!(!root_body.is_empty());

        let explicit_index = request("/index.html").await;
        assert_eq!(explicit_index.status(), StatusCode::OK);
        let explicit_index_body = to_bytes(explicit_index.into_body(), DASHBOARD_RESPONSE_LIMIT)
            .await
            .expect("explicit dashboard index body");
        assert_eq!(explicit_index_body, root_body);

        let html = std::str::from_utf8(&root_body).expect("UTF-8 dashboard index");
        let referenced_asset = html
            .split('"')
            .find(|value| {
                value.starts_with("/assets/")
                    && std::path::Path::new(value)
                        .extension()
                        .is_some_and(|extension| {
                            extension.eq_ignore_ascii_case("js")
                                || extension.eq_ignore_ascii_case("css")
                        })
            })
            .expect("dashboard index must reference a compiled asset");
        let asset = request(referenced_asset).await;
        assert_eq!(asset.status(), StatusCode::OK);
        assert_eq!(
            asset.headers()[header::CACHE_CONTROL],
            "public, max-age=31536000, immutable"
        );
        let asset_body = to_bytes(asset.into_body(), DASHBOARD_RESPONSE_LIMIT)
            .await
            .expect("referenced dashboard asset body");
        assert!(!asset_body.is_empty());
    }
}
