use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{DefaultBodyLimit, Request};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::Router;
use url::Url;

use crate::config::{BackendKind, Config, ConfigError, KoboConfig, User};
use crate::{articles, images, readeck, store};

#[derive(Debug, Clone)]
pub struct Backend {
    base: Url,
}

impl Backend {
    pub fn from_config(config: &KoboConfig) -> Result<Self, ConfigError> {
        let custom = config
            .custom_backend_url
            .clone()
            .or_else(|| config.store_api_url.clone());
        let base = match (config.backend, custom) {
            (BackendKind::Custom, Some(base)) => base,
            (BackendKind::Custom, None) => {
                return Err(ConfigError::Invalid(
                    "kobo.custom_backend_url is required when kobo.backend is \"custom\"".into(),
                ));
            }
            // The URL wins even with the default backend; this keeps old
            // `kobo.store_api_url` configs working.
            (BackendKind::Kobo, Some(base)) => base,
            (BackendKind::Kobo, None) => Url::parse("https://storeapi.kobo.com")
                .map_err(|error| ConfigError::Invalid(error.to_string()))?,
        };
        Ok(Self { base })
    }

    pub fn base(&self) -> &Url {
        &self.base
    }

    pub fn host(&self) -> &str {
        self.base.host_str().unwrap_or_default()
    }

    /// Host header value, including a non-default port when present.
    pub fn authority(&self) -> String {
        match self.base.port() {
            Some(port) => format!("{}:{port}", self.host()),
            None => self.host().to_string(),
        }
    }

    pub fn scheme(&self) -> &str {
        self.base.scheme()
    }

    pub fn url_for(&self, path: &str, query: Option<&str>) -> Url {
        let mut url = self.base.clone();
        let base_path = self.base.path().trim_end_matches('/');
        let path = path.trim_start_matches('/');
        let path = if path.is_empty() {
            format!("{base_path}/")
        } else {
            format!("{base_path}/{path}")
        };
        url.set_path(&path);
        url.set_query(query.filter(|query| !query.is_empty()));
        url
    }
}

#[derive(Clone)]
pub struct App {
    pub config: Arc<Config>,
    pub backend: Backend,
    pub readeck_http: reqwest::Client,
    pub image_http: reqwest::Client,
    pub proxy_http: reqwest::Client,
}

impl App {
    pub fn new(config: Config) -> anyhow::Result<Self> {
        config.validate()?;
        let backend = Backend::from_config(&config.kobo)?;
        let user_agent = concat!("readeckobo/", env!("CARGO_PKG_VERSION"));

        let readeck_http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .user_agent(user_agent)
            .build()?;
        let image_http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(5))
            .user_agent(user_agent)
            .build()?;
        // No total timeout: library syncs and book downloads can be long-lived.
        let proxy_http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .pool_idle_timeout(Duration::from_secs(90))
            .user_agent(user_agent)
            .build()?;

        Ok(Self {
            config: Arc::new(config),
            backend,
            readeck_http,
            image_http,
            proxy_http,
        })
    }

    pub fn router(self: &Arc<Self>) -> Router {
        Router::new()
            .route("/healthz", get(healthz))
            .route("/storeapi", any(store::handle_store))
            .route("/storeapi/{*path}", any(store::handle_store))
            .route("/instapaper/api/kobo/get", post(articles::kobo_get))
            .route(
                "/instapaper/api/kobo/download",
                post(articles::kobo_download),
            )
            .route("/instapaper/api/kobo/send", post(articles::kobo_send))
            .route("/convert-image", get(images::convert_image))
            .fallback(not_found)
            .layer(DefaultBodyLimit::max(128 * 1024 * 1024))
            .layer(middleware::from_fn(logging))
            .with_state(self.clone())
    }

    pub fn find_user(&self, token: &str) -> Option<&User> {
        self.config.users.iter().find(|user| user.token == token)
    }

    pub fn readeck_client(&self, user: &User) -> readeck::Client {
        readeck::Client::new(
            self.config.readeck.host.clone(),
            user.readeck_access_token.clone(),
            self.readeck_http.clone(),
        )
    }

    /// Warn when a configured Readeck token is read-only. Kobo's archive,
    /// favorite, and delete actions all need bookmarks:write.
    pub async fn warn_on_missing_write_permissions(&self) {
        for (index, user) in self.config.users.iter().enumerate() {
            let client = self.readeck_client(user);
            match client.profile_permissions().await {
                Ok(permissions) => {
                    let can_write = permissions.iter().any(|permission| {
                        permission == "api:bookmarks:write" || permission == "bookmarks:write"
                    });
                    if !can_write {
                        tracing::warn!(
                            user_index = index,
                            "Readeck token lacks bookmarks:write; archive, favorite, and delete actions will fail"
                        );
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        user_index = index,
                        %error,
                        "could not verify Readeck token permissions"
                    );
                }
            }
        }
    }

    /// Resolve the public origin used in rewritten initialization responses.
    pub fn public_origin(&self, headers: &HeaderMap) -> Result<Url, String> {
        if let Some(public_url) = &self.config.public_url {
            return Ok(public_url.clone());
        }

        let scheme = header_first(headers, "x-forwarded-proto").unwrap_or("http");
        let host = header_first(headers, "x-forwarded-host")
            .or_else(|| header_first(headers, "host"))
            .ok_or_else(|| "missing Host header".to_string())?;

        if scheme != "http" && scheme != "https" {
            return Err(format!("unsupported forwarded scheme: {scheme}"));
        }
        if host.is_empty()
            || host
                .chars()
                .any(|character| "/@?#, \t\r\n\"\\".contains(character))
        {
            return Err("invalid forwarded host".to_string());
        }

        let origin = Url::parse(&format!("{scheme}://{host}"))
            .map_err(|error| format!("invalid forwarded origin: {error}"))?;
        if origin.host_str().is_none() {
            return Err("invalid forwarded host".to_string());
        }
        Ok(origin)
    }

    pub fn instapaper_base(&self, headers: &HeaderMap) -> Result<String, String> {
        let origin = self.public_origin(headers)?;
        Ok(format!(
            "{}/instapaper/api/kobo",
            origin.as_str().trim_end_matches('/')
        ))
    }
}

fn header_first<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

async fn healthz() -> &'static str {
    "ok"
}

async fn not_found(uri: Uri) -> Response {
    tracing::warn!(path = %uri.path(), "route not found");
    (StatusCode::NOT_FOUND, "404 Not Found").into_response()
}

async fn logging(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let start = Instant::now();
    let response = next.run(request).await;
    tracing::info!(
        %method,
        path,
        status = response.status().as_u16(),
        elapsed = ?start.elapsed(),
        "request"
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn config() -> Config {
        Config::from_yaml(
            r#"
readeck:
  host: "https://readeck.example.com"
users:
  - token: "device-token"
    readeck_access_token: "readeck-token"
"#,
        )
        .unwrap()
    }

    #[test]
    fn backend_url_joins_base_path() {
        let backend = Backend {
            base: Url::parse("https://library.example.com/api/kobo/token").unwrap(),
        };
        let url = backend.url_for("/v1/initialization", Some("foo=bar"));
        assert_eq!(
            url.as_str(),
            "https://library.example.com/api/kobo/token/v1/initialization?foo=bar"
        );
    }

    #[test]
    fn backend_url_handles_root_path() {
        let backend = Backend {
            base: Url::parse("https://storeapi.kobo.com").unwrap(),
        };
        assert_eq!(
            backend.url_for("/v1/library/sync", None).as_str(),
            "https://storeapi.kobo.com/v1/library/sync"
        );
        assert_eq!(
            backend.url_for("", None).as_str(),
            "https://storeapi.kobo.com/"
        );
    }

    #[test]
    fn public_origin_prefers_configured_url() {
        let mut config = config();
        config.public_url = Some(Url::parse("https://readeckobo.example.com").unwrap());
        let app = App::new(config).unwrap();
        let origin = app.public_origin(&HeaderMap::new()).unwrap();
        assert_eq!(origin.as_str(), "https://readeckobo.example.com/");
    }

    #[test]
    fn public_origin_uses_forwarded_headers() {
        let app = App::new(config()).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        headers.insert(
            "x-forwarded-host",
            HeaderValue::from_static("readeckobo.example.com"),
        );
        assert_eq!(
            app.public_origin(&headers).unwrap().as_str(),
            "https://readeckobo.example.com/"
        );
        assert_eq!(
            app.instapaper_base(&headers).unwrap(),
            "https://readeckobo.example.com/instapaper/api/kobo"
        );
    }

    #[test]
    fn public_origin_rejects_bad_forwarded_host() {
        let app = App::new(config()).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-host",
            HeaderValue::from_static("evil.example.com/redirect"),
        );
        assert!(app.public_origin(&headers).is_err());
    }
}
