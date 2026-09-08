//! Serving the bridge UI.
//!
//! The UI is static files, but a browser cannot load them alone: it needs the attestation API
//! and Celestia's REST endpoint on the *same* origin. Celestia's public REST answers
//! correctly and sends no `Access-Control-Allow-Origin`, so a page that calls it directly
//! throws the response away and every balance reads as unknown.
//!
//! A reverse proxy would also solve that. This does it in-process instead, so a deployment is
//! three binaries and no web server to conflict with whatever else the host is running.

use std::path::PathBuf;

use anyhow::Result;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use tower_http::services::{ServeDir, ServeFile};
use tracing::info;

#[derive(Clone)]
struct Upstreams {
    http: reqwest::Client,
    /// Where the attestation API lives, usually the same host on another port.
    api: String,
    /// Celestia's REST endpoint, proxied so the browser sees one origin.
    celestia_rest: String,
}

/// Serve `dir` as a single-page app, with `/api` and `/celestia` proxied.
pub async fn serve(dir: PathBuf, api: String, celestia_rest: String, addr: &str) -> Result<()> {
    let index = dir.join("index.html");
    let upstreams = Upstreams {
        http: reqwest::Client::new(),
        api: api.trim_end_matches('/').to_string(),
        celestia_rest: celestia_rest.trim_end_matches('/').to_string(),
    };

    let app = Router::new()
        .route("/api/{*path}", any(proxy_api))
        .route("/celestia/{*path}", any(proxy_celestia))
        .with_state(upstreams)
        // Unknown paths are routes, not missing files.
        .fallback_service(ServeDir::new(dir).fallback(ServeFile::new(index)));

    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "bridge ui listening");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn proxy_api(State(up): State<Upstreams>, request: Request) -> Response {
    let target = format!("{}{}", up.api, path_and_query(request.uri()));
    forward(&up, target).await
}

async fn proxy_celestia(State(up): State<Upstreams>, request: Request) -> Response {
    // The route strips nothing, so drop the prefix this proxy is mounted under.
    let rest = path_and_query(request.uri());
    let target = format!(
        "{}{}",
        up.celestia_rest,
        rest.trim_start_matches("/celestia")
    );
    forward(&up, target).await
}

fn path_and_query(uri: &Uri) -> String {
    uri.path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| uri.path().into())
}

async fn forward(up: &Upstreams, target: String) -> Response {
    match up.http.get(&target).send().await {
        Ok(response) => {
            let status =
                StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let mut headers = HeaderMap::new();
            if let Some(kind) = response.headers().get(axum::http::header::CONTENT_TYPE) {
                headers.insert(axum::http::header::CONTENT_TYPE, kind.clone());
            }
            match response.bytes().await {
                Ok(body) => (status, headers, Body::from(body)).into_response(),
                Err(e) => (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
            }
        }
        Err(e) => (StatusCode::BAD_GATEWAY, format!("{target}: {e}")).into_response(),
    }
}
