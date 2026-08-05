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
