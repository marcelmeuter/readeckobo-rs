use std::collections::HashMap;
use std::time::Instant;

use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use url::Url;

use super::models::{Bookmark, BookmarkSync};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("URL error: {0}")]
    Url(#[from] url::ParseError),
    #[error("Readeck API error ({status}): {message}")]
    Api { status: StatusCode, message: String },
    #[error("failed to parse Readeck response: {0}")]
    Parse(String),
}

#[derive(Debug, Clone)]
pub struct Client {
    base: Url,
    token: String,
    http: reqwest::Client,
}

impl Client {
    pub fn new(base: Url, token: String, http: reqwest::Client) -> Self {
        Self { base, token, http }
    }

    fn url(&self, path: &str, query: &[(&str, String)]) -> Result<Url, Error> {
        let mut url = self.base.clone();
        let base_path = self.base.path().trim_end_matches('/');
        let path = path.trim_start_matches('/');
        url.set_path(&format!("{base_path}/{path}"));
        if !query.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (key, value) in query {
                pairs.append_pair(key, value);
            }
        }
        Ok(url)
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<reqwest::Response, Error> {
        let url = self.url(path, query)?;
        let mut request = self
            .http
            .request(method.clone(), url)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", self.token),
            )
            .header(reqwest::header::ACCEPT_ENCODING, "identity");
        if let Some(body) = body {
            request = request.json(body);
        }

        let start = Instant::now();
        let response = request.send().await?;
        tracing::debug!(
            method = %method,
            path,
            status = %response.status(),
            elapsed = ?start.elapsed(),
            "readeck request"
        );

        if !response.status().is_success() {
            let status = response.status();
            let message = response.text().await.unwrap_or_default();
            return Err(Error::Api { status, message });
        }

        Ok(response)
    }

    pub async fn get_bookmarks_sync(&self, since: Option<i64>) -> Result<Vec<BookmarkSync>, Error> {
        let query = since
            .map(|since| vec![("since", since.to_string())])
            .unwrap_or_default();
        let response = self
            .request(Method::GET, "/api/bookmarks/sync", &query, None)
            .await?;
        Ok(response.json().await?)
    }

    pub async fn get_bookmarks(
        &self,
        site: &str,
        page: i32,
        is_archived: Option<bool>,
    ) -> Result<(Vec<Bookmark>, i32), Error> {
        let mut query = vec![("page", page.to_string())];
        if !site.is_empty() {
            query.push(("site", site.to_string()));
        }
        if let Some(is_archived) = is_archived {
            query.push(("is_archived", is_archived.to_string()));
        }

        let response = self
            .request(Method::GET, "/api/bookmarks", &query, None)
            .await?;
        let total_pages = response
            .headers()
            .get("total-pages")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse().ok())
            .unwrap_or(1);
        let bookmarks = response.json().await?;
        Ok((bookmarks, total_pages))
    }

    pub async fn sync_bookmarks_content(
        &self,
        ids: &[String],
    ) -> Result<HashMap<String, Bookmark>, Error> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }

        let body = json!({
            "id": ids,
            "resource_prefix": "%/img",
            "sort": ["created"],
            "with_html": false,
            "with_json": true,
            "with_markdown": false,
            "with_resources": false,
        });
        let response = self
            .request(Method::POST, "/api/bookmarks/sync", &[], Some(&body))
            .await?;
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let bytes = response.bytes().await?;

        let bookmarks = if content_type.starts_with("multipart/") {
            parse_multipart_bookmarks(&bytes, &content_type)?
        } else if content_type.starts_with("application/json") {
            parse_json_bookmarks(&bytes)?
        } else {
            return Err(Error::Parse(format!(
                "unexpected Readeck sync content type: {content_type}"
            )));
        };

        Ok(bookmarks
            .into_iter()
            .map(|bookmark| (bookmark.id.clone(), bookmark))
            .collect())
    }

    pub async fn get_bookmark_article(&self, id: &str) -> Result<String, Error> {
        let path = format!("/api/bookmarks/{id}/article");
        let response = self.request(Method::GET, &path, &[], None).await?;
        Ok(response.text().await?)
    }

    pub async fn update_bookmark(&self, id: &str, updates: &Value) -> Result<(), Error> {
        let path = format!("/api/bookmarks/{id}");
        match self.request(Method::PATCH, &path, &[], Some(updates)).await {
            Ok(_) => Ok(()),
            Err(Error::Api { status, .. }) if status == StatusCode::NOT_FOUND => {
                tracing::info!(
                    bookmark_id = id,
                    "bookmark not found on Readeck; treating as success for Kobo"
                );
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    pub async fn create_bookmark(&self, url: &str) -> Result<(), Error> {
        let body = json!({ "url": url });
        self.request(Method::POST, "/api/bookmarks", &[], Some(&body))
            .await?;
        Ok(())
    }

    pub async fn delete_bookmark(&self, id: &str) -> Result<(), Error> {
        let path = format!("/api/bookmarks/{id}");
        match self.request(Method::DELETE, &path, &[], None).await {
            Ok(_) => Ok(()),
            Err(Error::Api { status, .. }) if status == StatusCode::NOT_FOUND => {
                tracing::info!(
                    bookmark_id = id,
                    "bookmark already missing on Readeck; treating delete as success"
                );
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// Read the bearer token's permissions from Readeck's profile endpoint.
    pub async fn profile_permissions(&self) -> Result<Vec<String>, Error> {
        let response = self.request(Method::GET, "/api/profile", &[], None).await?;
        let profile: Value = response.json().await?;
        let permissions = profile
            .pointer("/provider/permissions")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        Ok(permissions)
    }
}

fn parse_multipart_bookmarks(bytes: &[u8], content_type: &str) -> Result<Vec<Bookmark>, Error> {
    // The HTTP response carries the multipart Content-Type. mailparse expects
    // a complete MIME message, so add the header before parsing the body.
    let mut message = Vec::with_capacity(bytes.len() + content_type.len() + 64);
    message.extend_from_slice(
        format!("Content-Type: {content_type}\r\nMIME-Version: 1.0\r\n\r\n").as_bytes(),
    );
    message.extend_from_slice(bytes);

    let mail = mailparse::parse_mail(&message).map_err(|error| Error::Parse(error.to_string()))?;
    let mut bookmarks = Vec::new();
    for part in mail.subparts {
        if !part.ctype.mimetype.starts_with("application/json") {
            continue;
        }
        let body = part
            .get_body()
            .map_err(|error| Error::Parse(error.to_string()))?;
        let bookmark = serde_json::from_str(&body)
            .map_err(|error| Error::Parse(format!("invalid bookmark part: {error}")))?;
        bookmarks.push(bookmark);
    }
    Ok(bookmarks)
}

fn parse_json_bookmarks(bytes: &[u8]) -> Result<Vec<Bookmark>, Error> {
    match serde_json::from_slice::<Vec<Bookmark>>(bytes) {
        Ok(bookmarks) => Ok(bookmarks),
        Err(_) => serde_json::from_slice::<Bookmark>(bytes)
            .map(|bookmark| vec![bookmark])
            .map_err(|error| Error::Parse(format!("invalid bookmark JSON: {error}"))),
    }
}
