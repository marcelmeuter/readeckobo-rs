use std::io::Cursor;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use bytes::Bytes;
use http_body_util::BodyExt;
use image::ImageFormat;
use serde_json::{json, Value};
use tower::ServiceExt;
use url::Url;
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use readeckobo::app::App;
use readeckobo::config::{
    BackendKind, Config, KoboConfig, LogLevel, ReadeckConfig, ServerConfig, User,
};

fn test_config(
    readeck_url: &str,
    custom_backend_url: Option<&str>,
    remove_archived: bool,
) -> Config {
    Config {
        server: ServerConfig { port: 8080 },
        log_level: LogLevel::Info,
        public_url: Some(Url::parse("https://readeckobo.example.com").unwrap()),
        readeck: ReadeckConfig {
            host: Url::parse(readeck_url).unwrap(),
        },
        kobo: KoboConfig {
            backend: if custom_backend_url.is_some() {
                BackendKind::Custom
            } else {
                BackendKind::Kobo
            },
            custom_backend_url: custom_backend_url.map(|url| Url::parse(url).unwrap()),
            store_api_url: None,
            remove_archived_from_kobo: remove_archived,
        },
        users: vec![User {
            token: "device-token".to_string(),
            readeck_access_token: "readeck-token".to_string(),
        }],
    }
}

fn app(config: Config) -> Arc<App> {
    Arc::new(App::new(config).unwrap())
}

async fn call(app: &Arc<App>, request: Request<Body>) -> (StatusCode, HeaderMap, Bytes) {
    let response = app.router().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, body)
}

fn json_request(method: Method, uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn form_request(uri: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn bookmark(id: &str, archived: bool) -> Value {
    json!({
        "id": id,
        "url": "https://www.example.com/article",
        "title": "An article",
        "description": "A description",
        "authors": ["Alice"],
        "labels": ["news"],
        "created": "2026-01-01T00:00:00Z",
        "updated": "2026-01-02T00:00:00Z",
        "is_archived": archived,
        "is_marked": true,
        "word_count": 42,
        "resources": {
            "image": {
                "src": "https://example.com/image.png"
            }
        }
    })
}

fn multipart_bookmarks(bookmarks: &[Value]) -> (String, String) {
    let boundary = "readeck-test-boundary";
    let mut body = String::new();
    for bookmark in bookmarks {
        body.push_str(&format!(
            "--{boundary}\r\nContent-Type: application/json\r\n\r\n{bookmark}\r\n"
        ));
    }
    body.push_str(&format!("--{boundary}--\r\n"));
    (body, format!("multipart/mixed; boundary={boundary}"))
}

fn authority(uri: &str) -> String {
    let url = Url::parse(uri).unwrap();
    match url.port() {
        Some(port) => format!("{}:{port}", url.host_str().unwrap()),
        None => url.host_str().unwrap().to_string(),
    }
}

#[tokio::test]
async fn send_maps_kobo_actions_to_readeck() {
    let server = MockServer::start().await;

    for (method_name, path_value, body) in [
        (
            "PATCH",
            "/api/bookmarks/archive-id",
            json!({"is_archived": true}),
        ),
        (
            "PATCH",
            "/api/bookmarks/readd-id",
            json!({"is_archived": false}),
        ),
        ("PATCH", "/api/bookmarks/fav-id", json!({"is_marked": true})),
        (
            "PATCH",
            "/api/bookmarks/unfav-id",
            json!({"is_marked": false}),
        ),
    ] {
        Mock::given(method(method_name))
            .and(path(path_value))
            .and(body_json(body))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
    }

    Mock::given(method("DELETE"))
        .and(path("/api/bookmarks/delete-id"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/api/bookmarks"))
        .and(body_json(json!({"url": "https://example.com/new"})))
        .respond_with(ResponseTemplate::new(202))
        .expect(1)
        .mount(&server)
        .await;

    let app = app(test_config(&server.uri(), None, false));
    let actions = json!({
        "access_token": "device-token",
        "actions": [
            {"action": "archive", "item_id": "archive-id"},
            {"action": "readd", "item_id": "readd-id"},
            {"action": "favorite", "item_id": "fav-id"},
            {"action": "unfavorite", "item_id": "unfav-id"},
            {"action": "delete", "item_id": "delete-id"},
            {"action": "add", "url": "https://example.com/new"},
            {"action": "opened_item", "item_id": "opened-id"},
            {"action": "left_item", "item_id": "left-id"}
        ]
    });
    let (status, _, body) = call(
        &app,
        json_request(Method::POST, "/instapaper/api/kobo/send", actions),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let response: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(response["status"], 1);
    assert_eq!(
        response["action_results"],
        json!([true, true, true, true, true, true, true, true])
    );
    assert_eq!(
        response["action_errors"],
        json!([null, null, null, null, null, null, null, null])
    );

    server.verify().await;
}

#[tokio::test]
async fn send_reports_unknown_action_without_calling_readeck() {
    let app = app(test_config("https://readeck.example.com", None, false));
    let actions = json!({
        "access_token": "device-token",
        "actions": [{"action": "bogus", "item_id": "x"}]
    });
    let (status, _, body) = call(
        &app,
        json_request(Method::POST, "/instapaper/api/kobo/send", actions),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let response: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(response["status"], 0);
    assert_eq!(response["action_results"], json!([false]));
    assert!(response["action_errors"][0]["message"]
        .as_str()
        .unwrap()
        .contains("unknown action"));
}

#[tokio::test]
async fn full_sync_excludes_archived_bookmarks() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/bookmarks/sync"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": "b1", "type": "update"},
            {"id": "b2", "type": "update"}
        ])))
        .mount(&server)
        .await;

    let (multipart, content_type) =
        multipart_bookmarks(&[bookmark("b1", false), bookmark("b2", true)]);
    Mock::given(method("POST"))
        .and(path("/api/bookmarks/sync"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(multipart, &content_type))
        .mount(&server)
        .await;

    let app = app(test_config(&server.uri(), None, false));
    let (status, _, body) = call(
        &app,
        json_request(
            Method::POST,
            "/instapaper/api/kobo/get",
            json!({"access_token": "device-token"}),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let response: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(response["status"], 1);
    assert_eq!(response["total"], 1);
    assert!(response["list"].get("b1").is_some());
    assert!(response["list"].get("b2").is_none());
    assert_eq!(response["list"]["b1"]["status"], "0");
    assert_eq!(response["list"]["b1"]["favorite"], "1");
}

#[tokio::test]
async fn incremental_sync_reports_archived_and_deleted() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/bookmarks/sync"))
        .and(query_param("since", "123"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": "b1", "type": "update"},
            {"id": "b2", "type": "delete"}
        ])))
        .mount(&server)
        .await;

    let (multipart, content_type) = multipart_bookmarks(&[bookmark("b1", true)]);
    Mock::given(method("POST"))
        .and(path("/api/bookmarks/sync"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(multipart, &content_type))
        .mount(&server)
        .await;

    let app = app(test_config(&server.uri(), None, false));
    let (status, _, body) = call(
        &app,
        json_request(
            Method::POST,
            "/instapaper/api/kobo/get",
            json!({"access_token": "device-token", "since": 123}),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let response: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(response["list"]["b1"]["status"], "1");
    assert_eq!(response["list"]["b2"]["status"], "2");
    assert_eq!(response["total"], 0);
}

#[tokio::test]
async fn incremental_sync_can_remove_archived_from_kobo() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/bookmarks/sync"))
        .and(query_param("since", "123"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": "b1", "type": "update"}
        ])))
        .mount(&server)
        .await;

    let (multipart, content_type) = multipart_bookmarks(&[bookmark("b1", true)]);
    Mock::given(method("POST"))
        .and(path("/api/bookmarks/sync"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(multipart, &content_type))
        .mount(&server)
        .await;

    let app = app(test_config(&server.uri(), None, true));
    let (status, _, body) = call(
        &app,
        json_request(
            Method::POST,
            "/instapaper/api/kobo/get",
            json!({"access_token": "device-token", "since": 123}),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let response: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(response["list"]["b1"]["status"], "2");
}

async fn mount_download_mocks(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/api/bookmarks"))
        .and(query_param("site", "www.example.com"))
        .and(query_param("page", "1"))
        .and(query_param("is_archived", "false"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("total-pages", "1")
                .set_body_json(json!([bookmark("b1", false)])),
        )
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/bookmarks/b1/article"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<p>Hello</p><img src="https://example.com/image.png"><img src="https://example.com/photo.jpg">"#,
        ))
        .mount(server)
        .await;
}

#[tokio::test]
async fn download_rewrites_images_through_convert_endpoint() {
    let server = MockServer::start().await;
    mount_download_mocks(&server).await;
    let app = app(test_config(&server.uri(), None, false));

    let (status, _, body) = call(
        &app,
        json_request(
            Method::POST,
            "/instapaper/api/kobo/download",
            json!({
                "access_token": "device-token",
                "url": "https://www.example.com/article"
            }),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let response: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        response["images"]["0"]["src"],
        "https://readeckobo.example.com/convert-image?url=https%3A%2F%2Fexample.com%2Fimage.png"
    );
    assert_eq!(
        response["images"]["1"]["src"],
        "https://example.com/photo.jpg"
    );
    assert!(response["article"]
        .as_str()
        .unwrap()
        .contains("<!--IMG_0-->"));
    assert!(response["article"]
        .as_str()
        .unwrap()
        .contains("<!--IMG_1-->"));
}

#[tokio::test]
async fn download_accepts_form_body_with_semicolon_in_url() {
    let server = MockServer::start().await;
    mount_download_mocks(&server).await;
    let app = app(test_config(&server.uri(), None, false));

    let (status, _, body) = call(
        &app,
        form_request(
            "/instapaper/api/kobo/download",
            "access_token=device-token&url=https://www.example.com/article?utm=1;2",
        ),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "body: {}",
        String::from_utf8_lossy(&body)
    );
    let response: Value = serde_json::from_slice(&body).unwrap();
    assert!(response["article"].as_str().unwrap().contains("Hello"));
}

#[tokio::test]
async fn store_initialization_rewrites_instapaper_url() {
    let server = MockServer::start().await;
    let backend_url = format!("{}/api/kobo/token", server.uri());
    let host = authority(&server.uri());

    Mock::given(method("GET"))
        .and(path("/api/kobo/token/v1/initialization"))
        .and(header("host", host.as_str()))
        .and(header("x-forwarded-host", host.as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"Resources":{"library_sync":"https://library.example.com/api/kobo/token/v1/library/sync","instapaper":"https://www.instapaper.com"}}"#,
            "application/json",
        ))
        .mount(&server)
        .await;

    let app = app(test_config(
        "https://readeck.example.com",
        Some(&backend_url),
        false,
    ));
    let request = Request::builder()
        .method(Method::GET)
        .uri("/storeapi/v1/initialization")
        .body(Body::empty())
        .unwrap();
    let (status, headers, body) = call(&app, request).await;

    assert_eq!(status, StatusCode::OK);
    let body = String::from_utf8(body.to_vec()).unwrap();
    assert!(body.contains("https://readeckobo.example.com/instapaper"));
    assert!(body.contains("https://library.example.com/api/kobo/token/v1/library/sync"));
    assert!(!body.contains("https://www.instapaper.com"));
    assert_eq!(headers["cache-control"], "private, no-store");
    assert!(headers.get("content-encoding").is_none());
}

#[tokio::test]
async fn store_proxy_streams_response_and_preserves_headers() {
    let server = MockServer::start().await;
    let backend_url = format!("{}/api/kobo/token", server.uri());
    let host = authority(&server.uri());

    Mock::given(method("GET"))
        .and(path("/api/kobo/token/v1/library/sync"))
        .and(header("host", host.as_str()))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-kobo-sync", "sync-token")
                .set_body_string("library"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let app = app(test_config(
        "https://readeck.example.com",
        Some(&backend_url),
        false,
    ));
    let request = Request::builder()
        .method(Method::GET)
        .uri("/storeapi/v1/library/sync")
        .body(Body::empty())
        .unwrap();
    let (status, headers, body) = call(&app, request).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["x-kobo-sync"], "sync-token");
    assert_eq!(body, "library");
    server.verify().await;
}

fn png_bytes() -> Vec<u8> {
    let image = image::RgbImage::from_pixel(4, 4, image::Rgb([255, 0, 0]));
    let mut cursor = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image)
        .write_to(&mut cursor, ImageFormat::Png)
        .unwrap();
    cursor.into_inner()
}

#[tokio::test]
async fn profile_permissions_are_read() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/profile"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "provider": {
                "permissions": ["api:bookmarks:read"]
            }
        })))
        .mount(&server)
        .await;

    let app = app(test_config(&server.uri(), None, false));
    let client = app.readeck_client(&app.config.users[0]);
    let permissions = client.profile_permissions().await.unwrap();
    assert_eq!(permissions, vec!["api:bookmarks:read"]);
}

#[tokio::test]
async fn convert_image_returns_jpeg() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/image.png"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "image/png")
                .set_body_bytes(png_bytes()),
        )
        .mount(&server)
        .await;

    let app = app(test_config("https://readeck.example.com", None, false));
    let image_url = format!("{}/image.png", server.uri());
    let mut uri = Url::parse("http://localhost/convert-image").unwrap();
    uri.query_pairs_mut().append_pair("url", &image_url);
    let request = Request::builder()
        .method(Method::GET)
        .uri(format!("{}?{}", uri.path(), uri.query().unwrap_or_default()).as_str())
        .body(Body::empty())
        .unwrap();
    let (status, headers, body) = call(&app, request).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/jpeg");
    assert!(body.starts_with(&[0xff, 0xd8, 0xff]));
}

#[tokio::test]
async fn convert_image_returns_placeholder_on_failure() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/missing.png"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let app = app(test_config("https://readeck.example.com", None, false));
    let image_url = format!("{}/missing.png", server.uri());
    let mut uri = Url::parse("http://localhost/convert-image").unwrap();
    uri.query_pairs_mut().append_pair("url", &image_url);
    let request = Request::builder()
        .method(Method::GET)
        .uri(format!("{}?{}", uri.path(), uri.query().unwrap_or_default()).as_str())
        .body(Body::empty())
        .unwrap();
    let (status, headers, body) = call(&app, request).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/jpeg");
    assert!(body.starts_with(&[0xff, 0xd8, 0xff]));
}

#[tokio::test]
async fn convert_image_rasterizes_svg() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/diagram.svg"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "image/svg+xml")
                .set_body_string(
                    r#"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="20"><rect width="20" height="20" fill="rgb(255,0,0)"/></svg>"#,
                ),
        )
        .mount(&server)
        .await;

    let app = app(test_config("https://readeck.example.com", None, false));
    let image_url = format!("{}/diagram.svg", server.uri());
    let mut uri = Url::parse("http://localhost/convert-image").unwrap();
    uri.query_pairs_mut().append_pair("url", &image_url);
    let request = Request::builder()
        .method(Method::GET)
        .uri(format!("{}?{}", uri.path(), uri.query().unwrap_or_default()).as_str())
        .body(Body::empty())
        .unwrap();
    let (status, headers, body) = call(&app, request).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/jpeg");
    // A placeholder would be 800x600; the real SVG is 40x20.
    let image = image::load_from_memory(&body).unwrap().to_rgb8();
    assert_eq!(image.dimensions(), (40, 20));
    let [r, g, b] = image.get_pixel(5, 10).0;
    assert!(r > 200 && g < 60 && b < 60, "expected red, got {r},{g},{b}");
}
