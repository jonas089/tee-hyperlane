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
    /// Celestia's Tendermint RPC, proxied for a sharper reason than tidiness.
    ///
    /// Mocha's public RPC answers the CORS preflight with `Access-Control-Allow-Origin: *`
    /// and then omits that header from the POST itself. The browser lets the preflight
    /// through and then refuses to read the response, which reaches the user as "Failed to
    /// fetch" with nothing useful in the console. Routing the RPC through this origin means
    /// the browser never has to make that judgement.
    celestia_rpc: String,
}

/// Serve `dir` as a single-page app, with `/api`, `/celestia` and `/celestia-rpc` proxied.
pub async fn serve(
    dir: PathBuf,
    api: String,
    celestia_rest: String,
    celestia_rpc: String,
    addr: &str,
) -> Result<()> {
    let index = dir.join("index.html");
    let upstreams = Upstreams {
        http: reqwest::Client::new(),
        api: api.trim_end_matches('/').to_string(),
        celestia_rest: celestia_rest.trim_end_matches('/').to_string(),
        celestia_rpc: celestia_rpc.trim_end_matches('/').to_string(),
    };

    let app = Router::new()
        .route("/api/{*path}", any(proxy_api))
        .route("/celestia/{*path}", any(proxy_celestia))
        // CosmJS posts to the RPC root, so match both the bare path and anything under it.
        .route("/celestia-rpc", any(proxy_celestia_rpc))
        .route("/celestia-rpc/{*path}", any(proxy_celestia_rpc))
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

async fn proxy_celestia_rpc(State(up): State<Upstreams>, request: Request) -> Response {
    let rest = path_and_query(request.uri());
    let target = format!(
        "{}{}",
        up.celestia_rpc,
        rest.trim_start_matches("/celestia-rpc")
    );
    forward_body(&up, request, target).await
}

fn path_and_query(uri: &Uri) -> String {
    uri.path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| uri.path().into())
}

/// Forward method, content type and body as they arrived.
///
/// The RPC needs this where `forward` will not do: a Tendermint call is a POST carrying a
/// JSON-RPC envelope, and dropping either the method or the body turns every query into a
/// confident wrong answer rather than an error.
async fn forward_body(up: &Upstreams, request: Request, target: String) -> Response {
    let method = reqwest::Method::from_bytes(request.method().as_str().as_bytes())
        .unwrap_or(reqwest::Method::POST);
    let content_type = request
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .cloned();

    // Far above any Tendermint request, far below anything that would make this a memory
    // problem.
    let body = match axum::body::to_bytes(request.into_body(), 2 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };

    let mut outbound = up.http.request(method, &target).body(body);
    if let Some(kind) = content_type {
        outbound = outbound.header(axum::http::header::CONTENT_TYPE, kind);
    }

    match outbound.send().await {
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
