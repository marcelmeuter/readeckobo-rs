use std::io::Read;
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::Response;
use bytes::{Bytes, BytesMut};

use crate::app::{App, Backend};

const MAX_INITIALIZATION_BODY: usize = 8 * 1024 * 1024;
const MAX_PROXY_REQUEST_BODY: usize = 64 * 1024 * 1024;
const MAX_PROXY_RESPONSE_BODY: usize = 64 * 1024 * 1024;

pub async fn handle_store(
    State(app): State<Arc<App>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let path = uri.path();
    let subpath = path.strip_prefix("/storeapi").unwrap_or(path);
    let subpath = if subpath.is_empty() { "/" } else { subpath };

    if subpath == "/v1/initialization" {
        handle_initialization(app, method, uri, headers, body).await
    } else {
        handle_proxy(app, method, uri, headers, body).await
    }
}

async fn handle_initialization(
    app: Arc<App>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let upstream = app
        .backend
        .url_for(uri.path().trim_start_matches("/storeapi"), uri.query());
    let body = match to_bytes(body, MAX_PROXY_REQUEST_BODY).await {
        Ok(body) => body,
        Err(_) => return text_response(StatusCode::BAD_REQUEST, "Invalid request body"),
    };

    let mut request_headers = filtered_request_headers(&headers);
    request_headers.insert(
        HeaderName::from_static("accept-encoding"),
        HeaderValue::from_static("identity"),
    );
    for name in ["if-none-match", "if-modified-since", "range", "if-range"] {
        request_headers.remove(name);
    }
    if let Err(error) = apply_backend_headers(&mut request_headers, &app.backend) {
        tracing::error!(%error, "failed to build backend request headers");
        return text_response(StatusCode::BAD_GATEWAY, "Bad gateway");
    }

    let mut request = app
        .proxy_http
        .request(method, upstream)
        .headers(request_headers);
    if !body.is_empty() {
        request = request.body(body);
    }

    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            tracing::error!(%error, "initialization upstream request failed");
            return text_response(StatusCode::BAD_GATEWAY, "Bad gateway");
        }
    };

    if response.status() != StatusCode::OK {
        return buffered_response(response).await;
    }

    let status = response.status();
    let response_headers = filtered_response_headers(response.headers());
    let content_encoding = response_headers
        .get("content-encoding")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("identity")
        .to_ascii_lowercase();

    let body = match read_limited(response, MAX_INITIALIZATION_BODY).await {
        Ok(body) => body,
        Err(error) => {
            tracing::error!(%error, "failed to read initialization response");
            return text_response(StatusCode::BAD_GATEWAY, "Bad gateway");
        }
    };

    let body = match content_encoding.as_str() {
        "identity" | "" => body,
        "gzip" => match decompress_gzip(&body) {
            Ok(body) => Bytes::from(body),
            Err(error) => {
                tracing::error!(%error, "failed to decompress initialization response");
                return text_response(StatusCode::BAD_GATEWAY, "Bad gateway");
            }
        },
        encoding => {
            tracing::error!(encoding, "unsupported initialization encoding");
            return text_response(StatusCode::BAD_GATEWAY, "Bad gateway");
        }
    };

    let body = match std::str::from_utf8(&body) {
        Ok(body) => body,
        Err(error) => {
            tracing::error!(%error, "initialization response is not UTF-8");
            return text_response(StatusCode::BAD_GATEWAY, "Bad gateway");
        }
    };

    let instapaper = match app.instapaper_base(&headers) {
        Ok(instapaper) => instapaper,
        Err(error) => {
            tracing::error!(%error, "failed to resolve public origin");
            return text_response(StatusCode::BAD_GATEWAY, "Bad gateway");
        }
    };
    let body = rewrite_initialization(body, &instapaper);

    let mut response_headers = response_headers;
    for name in [
        "content-encoding",
        "content-length",
        "etag",
        "last-modified",
        "content-md5",
        "digest",
        "content-range",
        "accept-ranges",
    ] {
        response_headers.remove(name);
    }
    if let Ok(value) = HeaderValue::from_str(&body.len().to_string()) {
        response_headers.insert("content-length", value);
    }
    response_headers.insert(
        "cache-control",
        HeaderValue::from_static("private, no-store"),
    );

    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    *response.headers_mut() = response_headers;
    response
}

async fn handle_proxy(
    app: Arc<App>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let upstream = app
        .backend
        .url_for(uri.path().trim_start_matches("/storeapi"), uri.query());
    let body = match to_bytes(body, MAX_PROXY_REQUEST_BODY).await {
        Ok(body) => body,
        Err(_) => return text_response(StatusCode::PAYLOAD_TOO_LARGE, "Request body too large"),
    };

    let mut request_headers = filtered_request_headers(&headers);
    if let Err(error) = apply_backend_headers(&mut request_headers, &app.backend) {
        tracing::error!(%error, "failed to build backend request headers");
        return text_response(StatusCode::BAD_GATEWAY, "Bad gateway");
    }

    let mut request = app
        .proxy_http
        .request(method, upstream)
        .headers(request_headers);
    if !body.is_empty() {
        request = request.body(body);
    }

    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            tracing::error!(%error, "store upstream request failed");
            return text_response(StatusCode::BAD_GATEWAY, "Bad gateway");
        }
    };

    let status = response.status();
    let response_headers = filtered_response_headers(response.headers());
    let stream = response.bytes_stream();
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    *response.headers_mut() = response_headers;
    response
}

async fn buffered_response(response: reqwest::Response) -> Response {
    let status = response.status();
    let headers = filtered_response_headers(response.headers());
    match read_limited(response, MAX_PROXY_RESPONSE_BODY).await {
        Ok(body) => {
            let mut response = Response::new(Body::from(body));
            *response.status_mut() = status;
            *response.headers_mut() = headers;
            response
        }
        Err(error) => {
            tracing::error!(%error, "failed to read upstream response");
            text_response(StatusCode::BAD_GATEWAY, "Bad gateway")
        }
    }
}

async fn read_limited(mut response: reqwest::Response, limit: usize) -> Result<Bytes, String> {
    let mut body = BytesMut::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
        if body.len() + chunk.len() > limit {
            return Err("upstream response too large".to_string());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body.freeze())
}

fn decompress_gzip(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut decoder = flate2::read::GzDecoder::new(bytes);
    let mut output = Vec::new();
    decoder
        .read_to_end(&mut output)
        .map_err(|error| error.to_string())?;
    Ok(output)
}

fn rewrite_initialization(body: &str, instapaper: &str) -> String {
    let escaped = instapaper.replace('/', "\\/");
    // Longest forms first so `/api/kobo` is not duplicated.
    body.replace("https:\\/\\/www.instapaper.com\\/api\\/kobo", &escaped)
        .replace("http:\\/\\/www.instapaper.com\\/api\\/kobo", &escaped)
        .replace("https:\\/\\/www.instapaper.com", &escaped)
        .replace("http:\\/\\/www.instapaper.com", &escaped)
        .replace("https://www.instapaper.com/api/kobo", instapaper)
        .replace("http://www.instapaper.com/api/kobo", instapaper)
        .replace("https://www.instapaper.com", instapaper)
        .replace("http://www.instapaper.com", instapaper)
        .replace("https://instapaper.com", instapaper)
        .replace("http://instapaper.com", instapaper)
}

fn filtered_request_headers(headers: &HeaderMap) -> HeaderMap {
    let connection_tokens = connection_tokens(headers);
    let mut output = HeaderMap::new();
    for (name, value) in headers {
        let name_str = name.as_str();
        if name_str == "host"
            || name_str == "content-length"
            || name_str.starts_with("x-forwarded-")
            || name_str == "forwarded"
            || is_hop_by_hop(name_str, &connection_tokens)
        {
            continue;
        }
        output.append(name.clone(), value.clone());
    }
    output
}

fn filtered_response_headers(headers: &HeaderMap) -> HeaderMap {
    let connection_tokens = connection_tokens(headers);
    let mut output = HeaderMap::new();
    for (name, value) in headers {
        if is_hop_by_hop(name.as_str(), &connection_tokens) {
            continue;
        }
        output.append(name.clone(), value.clone());
    }
    output
}

fn connection_tokens(headers: &HeaderMap) -> Vec<String> {
    headers
        .get("connection")
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(',')
                .map(|token| token.trim().to_ascii_lowercase())
                .filter(|token| !token.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn is_hop_by_hop(name: &str, connection_tokens: &[String]) -> bool {
    matches!(
        name,
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    ) || connection_tokens.iter().any(|token| token == name)
}

fn apply_backend_headers(headers: &mut HeaderMap, backend: &Backend) -> Result<(), String> {
    let authority =
        HeaderValue::from_str(&backend.authority()).map_err(|error| error.to_string())?;
    let scheme = HeaderValue::from_str(backend.scheme()).map_err(|error| error.to_string())?;
    headers.insert(HeaderName::from_static("host"), authority.clone());
    headers.insert(HeaderName::from_static("x-forwarded-host"), authority);
    headers.insert(HeaderName::from_static("x-forwarded-proto"), scheme);
    Ok(())
}

fn text_response(status: StatusCode, message: &'static str) -> Response {
    let mut response = Response::new(Body::from(message));
    *response.status_mut() = status;
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_instapaper_url_and_keeps_library_urls() {
        let body = r#"{"Resources":{"library_sync":"https://library.example.com/api/kobo/token/v1/library/sync","instapaper":"https://www.instapaper.com"}}"#;
        let rewritten =
            rewrite_initialization(body, "https://readeckobo.example.com/instapaper/api/kobo");
        assert!(rewritten.contains("https://library.example.com/api/kobo/token/v1/library/sync"));
        assert!(rewritten.contains("https://readeckobo.example.com/instapaper/api/kobo"));
        assert!(!rewritten.contains("https://www.instapaper.com"));
    }

    #[test]
    fn rewrites_instapaper_url_without_duplicating_kobo_path() {
        let body = r#"{"instapaper":"https://www.instapaper.com/api/kobo"}"#;
        let rewritten =
            rewrite_initialization(body, "https://readeckobo.example.com/instapaper/api/kobo");
        assert!(rewritten.contains("https://readeckobo.example.com/instapaper/api/kobo"));
        assert!(!rewritten.contains("/api/kobo/api/kobo"));
    }

    #[test]
    fn rewrites_escaped_instapaper_url() {
        let body = r#"{"instapaper":"https:\/\/www.instapaper.com"}"#;
        let rewritten =
            rewrite_initialization(body, "https://readeckobo.example.com/instapaper/api/kobo");
        assert!(rewritten.contains(r"https:\/\/readeckobo.example.com\/instapaper\/api\/kobo"));
    }

    #[test]
    fn decompresses_gzip_initialization_body() {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use std::io::Write;

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(b"{\"instapaper\":\"https://www.instapaper.com\"}")
            .unwrap();
        let compressed = encoder.finish().unwrap();
        let decompressed = decompress_gzip(&compressed).unwrap();
        assert_eq!(
            String::from_utf8(decompressed).unwrap(),
            "{\"instapaper\":\"https://www.instapaper.com\"}"
        );
    }

    #[test]
    fn strips_forwarding_and_hop_by_hop_request_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("public.example.com"));
        headers.insert("connection", HeaderValue::from_static("x-remove-me"));
        headers.insert("x-remove-me", HeaderValue::from_static("gone"));
        headers.insert(
            "x-forwarded-host",
            HeaderValue::from_static("evil.example.com"),
        );
        headers.insert("authorization", HeaderValue::from_static("Bearer token"));
        let filtered = filtered_request_headers(&headers);
        assert!(filtered.get("host").is_none());
        assert!(filtered.get("x-remove-me").is_none());
        assert!(filtered.get("x-forwarded-host").is_none());
        assert_eq!(filtered.get("authorization").unwrap(), "Bearer token");
    }
}
