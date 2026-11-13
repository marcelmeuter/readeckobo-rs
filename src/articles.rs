use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use url::Url;

use crate::app::App;
use crate::kobo::{
    KoboArticleItem, KoboAuthor, KoboDownloadForm, KoboDownloadRequest, KoboGetRequest,
    KoboGetResponse, KoboImage, KoboSendRequest, KoboTag,
};
use crate::readeck::{self, Bookmark, BookmarkSync};

const MAX_KOBO_BODY: usize = 1024 * 1024;

pub async fn kobo_get(State(app): State<Arc<App>>, body: Body) -> Response {
    let body = match to_bytes(body, MAX_KOBO_BODY).await {
        Ok(body) => body,
        Err(_) => return text_response(StatusCode::BAD_REQUEST, "Invalid request body"),
    };
    let request: KoboGetRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => {
            tracing::warn!(%error, "invalid /instapaper/api/kobo/get request");
            return text_response(StatusCode::BAD_REQUEST, "Invalid request body");
        }
    };

    let Some(user) = app.find_user(&request.access_token) else {
        return text_response(StatusCode::UNAUTHORIZED, "Invalid access token");
    };
    let client = app.readeck_client(user);
    let since = parse_since(request.since.as_ref());

    let result = match since {
        Some(since) => handle_incremental_sync(&app, &client, since).await,
        None => handle_full_sync(&client, &request).await,
    };

    match result {
        Ok((list, total)) => Json(KoboGetResponse {
            status: 1,
            list,
            total,
        })
        .into_response(),
        Err(error) => {
            tracing::error!(%error, "kobo sync failed");
            text_response(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error")
        }
    }
}

async fn handle_full_sync(
    client: &readeck::Client,
    request: &KoboGetRequest,
) -> Result<(HashMap<String, KoboArticleItem>, i32), String> {
    let count = parse_i32(request.count.as_deref()).unwrap_or(0).max(0);
    let offset = parse_i32(request.offset.as_deref()).unwrap_or(0).max(0);

    let syncs = client
        .get_bookmarks_sync(None)
        .await
        .map_err(|error| error.to_string())?;
    let ids: Vec<String> = syncs
        .iter()
        .filter(|sync| sync.kind != "delete")
        .map(|sync| sync.id.clone())
        .collect();
    let details = client
        .sync_bookmarks_content(&ids)
        .await
        .map_err(|error| error.to_string())?;

    let mut items = Vec::new();
    for sync in &syncs {
        if sync.kind == "delete" {
            continue;
        }
        let Some(bookmark) = details.get(&sync.id) else {
            continue;
        };
        if bookmark.is_archived {
            continue;
        }
        let mut item = build_article_item(bookmark, sync);
        item.status = "0".to_string();
        item.favorite = if bookmark.is_marked { "1" } else { "0" }.to_string();
        items.push(item);
    }

    let total = items.len() as i32;
    let start = (offset as usize).min(items.len());
    let end = if count == 0 {
        items.len()
    } else {
        ((offset as usize).saturating_add(count as usize)).min(items.len())
    };
    let list = items[start..end]
        .iter()
        .map(|item| (item.item_id.clone(), item.clone()))
        .collect();
    Ok((list, total))
}

async fn handle_incremental_sync(
    app: &App,
    client: &readeck::Client,
    since: i64,
) -> Result<(HashMap<String, KoboArticleItem>, i32), String> {
    let syncs = client
        .get_bookmarks_sync(Some(since))
        .await
        .map_err(|error| error.to_string())?;

    let mut list = HashMap::new();
    let mut candidate_ids = Vec::new();
    for sync in &syncs {
        if sync.kind == "delete" {
            list.insert(
                sync.id.clone(),
                KoboArticleItem {
                    item_id: sync.id.clone(),
                    status: "2".to_string(),
                    ..Default::default()
                },
            );
        } else {
            candidate_ids.push(sync.id.clone());
        }
    }

    if candidate_ids.is_empty() {
        return Ok((list, 0));
    }

    let details = client
        .sync_bookmarks_content(&candidate_ids)
        .await
        .map_err(|error| error.to_string())?;

    let mut total = 0;
    for sync in &syncs {
        if sync.kind == "delete" {
            continue;
        }
        let Some(bookmark) = details.get(&sync.id) else {
            continue;
        };
        let mut item = build_article_item(bookmark, sync);
        item.favorite = if bookmark.is_marked { "1" } else { "0" }.to_string();
        if bookmark.is_archived {
            item.status = if app.config.kobo.remove_archived_from_kobo {
                "2"
            } else {
                "1"
            }
            .to_string();
        } else {
            item.status = "0".to_string();
            total += 1;
        }
        list.insert(bookmark.id.clone(), item);
    }

    Ok((list, total))
}

fn build_article_item(bookmark: &Bookmark, sync: &BookmarkSync) -> KoboArticleItem {
    let authors = bookmark
        .authors
        .iter()
        .map(|author| {
            (
                author.clone(),
                KoboAuthor {
                    author_id: author.clone(),
                    name: author.clone(),
                },
            )
        })
        .collect();
    let tags = bookmark
        .labels
        .iter()
        .map(|label| {
            (
                label.clone(),
                KoboTag {
                    item_id: sync.id.clone(),
                    tag: label.clone(),
                },
            )
        })
        .collect();

    let mut item = KoboArticleItem {
        authors,
        excerpt: bookmark.description.clone(),
        given_title: bookmark.title.clone(),
        given_url: bookmark.url.clone(),
        has_image: "0".to_string(),
        has_video: "0".to_string(),
        is_article: "1".to_string(),
        item_id: bookmark.id.clone(),
        resolved_id: bookmark.id.clone(),
        resolved_title: bookmark.title.clone(),
        resolved_url: bookmark.url.clone(),
        tags,
        time_added: bookmark.created.timestamp(),
        time_updated: bookmark.updated.timestamp(),
        word_count: bookmark.word_count,
        ..Default::default()
    };

    if let Some(image) = &bookmark.resources.image {
        if !image.src.is_empty() {
            item.has_image = "1".to_string();
            item.image = Some(KoboImage {
                image_id: String::new(),
                item_id: String::new(),
                src: image.src.clone(),
            });
            item.images.insert(
                "1".to_string(),
                KoboImage {
                    image_id: "1".to_string(),
                    item_id: "1".to_string(),
                    src: image.src.clone(),
                },
            );
            item.optional.insert(
                "top_image_url".to_string(),
                Value::String(image.src.clone()),
            );
        }
    }

    item
}

fn parse_since(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(number) => number.as_f64().map(|value| value as i64),
        Value::String(value) => value
            .parse::<i64>()
            .ok()
            .or_else(|| value.parse::<f64>().ok().map(|value| value as i64)),
        _ => None,
    }
}

fn parse_i32(value: Option<&str>) -> Option<i32> {
    value?.trim().parse().ok()
}

pub async fn kobo_download(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let body = match to_bytes(body, MAX_KOBO_BODY).await {
        Ok(body) => body,
        Err(_) => return text_response(StatusCode::BAD_REQUEST, "Invalid request body"),
    };
    let request = match serde_json::from_slice::<KoboDownloadRequest>(&body) {
        Ok(request) => request,
        Err(_) => match serde_urlencoded::from_bytes::<KoboDownloadForm>(&body) {
            Ok(form) => form.into(),
            Err(error) => {
                tracing::warn!(%error, "invalid /instapaper/api/kobo/download request");
                return text_response(StatusCode::BAD_REQUEST, "Invalid request body or form data");
            }
        },
    };

    let Some(user) = app.find_user(&request.access_token) else {
        return text_response(StatusCode::UNAUTHORIZED, "Invalid access token");
    };
    let client = app.readeck_client(user);

    let requested_url = request.url.trim();
    if requested_url.is_empty() {
        return text_response(StatusCode::BAD_REQUEST, "Missing 'url' parameter");
    }
    let parsed = match Url::parse(requested_url) {
        Ok(parsed) => parsed,
        Err(error) => {
            tracing::warn!(%error, "invalid article URL");
            return text_response(StatusCode::BAD_REQUEST, "Invalid 'url' parameter");
        }
    };
    let Some(host) = parsed.host_str() else {
        return text_response(StatusCode::BAD_REQUEST, "Invalid 'url' parameter");
    };

    let bookmark = match find_bookmark(&client, requested_url, host).await {
        Ok(Some(bookmark)) => bookmark,
        Ok(None) => return text_response(StatusCode::NOT_FOUND, "Article not found"),
        Err(error) => {
            tracing::error!(%error, "failed to find bookmark");
            return text_response(StatusCode::BAD_GATEWAY, "Bad gateway");
        }
    };

    let article = match client.get_bookmark_article(&bookmark.id).await {
        Ok(article) => article,
        Err(error) => {
            tracing::error!(%error, bookmark_id = bookmark.id, "failed to fetch article");
            return text_response(StatusCode::BAD_GATEWAY, "Bad gateway");
        }
    };

    let (article, images) = match rewrite_article_html(&article, &bookmark.url, &app, &headers) {
        Ok(result) => result,
        Err(error) => {
            tracing::error!(%error, bookmark_id = bookmark.id, "failed to rewrite article");
            return text_response(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error");
        }
    };

    Json(json!({ "images": images, "article": article })).into_response()
}

async fn find_bookmark(
    client: &readeck::Client,
    requested_url: &str,
    host: &str,
) -> Result<Option<Bookmark>, String> {
    let mut had_error = false;
    for site in sites_to_try(host) {
        let mut page = 1;
        loop {
            let (bookmarks, total_pages) =
                match client.get_bookmarks(&site, page, Some(false)).await {
                    Ok(result) => result,
                    Err(error) => {
                        tracing::warn!(site, %error, "failed to search Readeck bookmarks");
                        had_error = true;
                        break;
                    }
                };

            for bookmark in bookmarks {
                if compare_urls(&bookmark.url, requested_url).unwrap_or(false) {
                    return Ok(Some(bookmark));
                }
            }

            if page >= total_pages || total_pages <= 0 || page >= 1000 {
                break;
            }
            page += 1;
        }
    }

    if had_error {
        Err("failed to search Readeck bookmarks".to_string())
    } else {
        Ok(None)
    }
}

fn sites_to_try(host: &str) -> Vec<String> {
    let mut parts: Vec<&str> = host.split('.').collect();
    let mut sites = Vec::new();
    while parts.len() >= 2 {
        let site = parts.join(".");
        if !site.is_empty() {
            sites.push(site);
        }
        if parts.len() == 2 {
            break;
        }
        parts.remove(0);
    }
    sites
}

fn compare_urls(left: &str, right: &str) -> Option<bool> {
    let left = Url::parse(left.trim()).ok()?;
    let right = Url::parse(right.trim()).ok()?;
    let left_host = left.host_str()?.trim_start_matches("www.");
    let right_host = right.host_str()?.trim_start_matches("www.");
    Some(left.scheme() == right.scheme() && left_host == right_host && left.path() == right.path())
}

fn rewrite_article_html(
    html: &str,
    bookmark_url: &str,
    app: &App,
    headers: &HeaderMap,
) -> Result<(String, HashMap<String, KoboImage>), String> {
    let base = Url::parse(bookmark_url).ok();
    let origin = app.public_origin(headers)?;
    let images = RefCell::new(HashMap::new());
    let index = Cell::new(0usize);

    let output = lol_html::rewrite_str(
        html,
        lol_html::RewriteStrSettings {
            element_content_handlers: vec![lol_html::element!("img[src]", |element| {
                let src = element
                    .get_attribute("src")
                    .map(|src| src.to_string())
                    .unwrap_or_default();
                let resolved = resolve_image_url(&src, base.as_ref());
                let converted = convert_image_url(&origin, &resolved).unwrap_or(resolved);
                let image_index = index.get();

                images.borrow_mut().insert(
                    image_index.to_string(),
                    KoboImage {
                        image_id: image_index.to_string(),
                        item_id: image_index.to_string(),
                        src: converted,
                    },
                );
                element.replace(
                    &format!("<!--IMG_{image_index}-->"),
                    lol_html::html_content::ContentType::Html,
                );
                index.set(image_index + 1);
                Ok(())
            })],
            ..Default::default()
        },
    )
    .map_err(|error| error.to_string())?;

    Ok((output, images.into_inner()))
}

fn resolve_image_url(src: &str, base: Option<&Url>) -> String {
    if src.starts_with("data:") {
        return src.to_string();
    }
    if let Ok(url) = Url::parse(src) {
        if url.scheme() == "http" || url.scheme() == "https" {
            return url.to_string();
        }
        return src.to_string();
    }
    if let Some(base) = base {
        if let Ok(url) = base.join(src) {
            return url.to_string();
        }
    }
    src.to_string()
}

fn convert_image_url(origin: &Url, src: &str) -> Result<String, String> {
    if src.starts_with("data:") {
        return Ok(src.to_string());
    }
    let parsed = match Url::parse(src) {
        Ok(parsed) => parsed,
        Err(_) => return Ok(src.to_string()),
    };
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Ok(src.to_string());
    }
    let path = parsed.path().to_ascii_lowercase();
    if path.ends_with(".jpg") || path.ends_with(".jpeg") {
        return Ok(src.to_string());
    }

    let mut url = Url::parse(&format!(
        "{}/convert-image",
        origin.as_str().trim_end_matches('/')
    ))
    .map_err(|error| error.to_string())?;
    url.query_pairs_mut().append_pair("url", src);
    Ok(url.to_string())
}

pub async fn kobo_send(State(app): State<Arc<App>>, body: Body) -> Response {
    let body = match to_bytes(body, MAX_KOBO_BODY).await {
        Ok(body) => body,
        Err(_) => return text_response(StatusCode::BAD_REQUEST, "Invalid request body"),
    };
    let request: KoboSendRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => {
            tracing::warn!(%error, "invalid /instapaper/api/kobo/send request");
            return text_response(StatusCode::BAD_REQUEST, "Invalid request body");
        }
    };

    let Some(user) = app.find_user(&request.access_token) else {
        return text_response(StatusCode::UNAUTHORIZED, "Invalid access token");
    };
    let client = app.readeck_client(user);

    let mut results = Vec::with_capacity(request.actions.len());
    let mut errors = Vec::with_capacity(request.actions.len());
    let mut all_succeeded = true;

    for action in &request.actions {
        let Some(object) = action.as_object() else {
            results.push(false);
            errors.push(Some(json!({ "message": "invalid action" })));
            all_succeeded = false;
            continue;
        };
        let action_name = object
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let item_id = object
            .get("item_id")
            .and_then(Value::as_str)
            .unwrap_or_default();

        let result = match action_name {
            "archive" => {
                client
                    .update_bookmark(item_id, &json!({ "is_archived": true }))
                    .await
            }
            "readd" => {
                client
                    .update_bookmark(item_id, &json!({ "is_archived": false }))
                    .await
            }
            "favorite" => {
                client
                    .update_bookmark(item_id, &json!({ "is_marked": true }))
                    .await
            }
            "unfavorite" => {
                client
                    .update_bookmark(item_id, &json!({ "is_marked": false }))
                    .await
            }
            "delete" => client.delete_bookmark(item_id).await,
            "add" => {
                let url = object
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                client.create_bookmark(url).await
            }
            "opened_item" | "left_item" => Ok(()),
            other => Err(readeck::Error::Parse(format!("unknown action: {other}"))),
        };

        match result {
            Ok(()) => {
                results.push(true);
                errors.push(None);
            }
            Err(error) => {
                tracing::warn!(action = action_name, %error, "kobo action failed");
                results.push(false);
                errors.push(Some(json!({ "message": error.to_string() })));
                all_succeeded = false;
            }
        }
    }

    Json(json!({
        "status": if all_succeeded { 1 } else { 0 },
        "action_results": results,
        "action_errors": errors,
    }))
    .into_response()
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
    fn strips_subdomains_progressively() {
        assert_eq!(
            sites_to_try("a.b.example.com"),
            vec!["a.b.example.com", "b.example.com", "example.com"]
        );
        assert_eq!(sites_to_try("example.com"), vec!["example.com"]);
    }

    #[test]
    fn compares_urls_ignoring_www_and_query() {
        assert_eq!(
            compare_urls(
                "https://www.example.com/article?utm=1",
                "https://example.com/article?utm=2"
            ),
            Some(true)
        );
        assert_eq!(
            compare_urls("https://example.com/a", "https://example.com/b"),
            Some(false)
        );
    }

    #[test]
    fn rewrites_images_through_convert_endpoint() {
        let origin = Url::parse("https://readeckobo.example.com").unwrap();
        let converted = convert_image_url(&origin, "https://example.com/image.png").unwrap();
        assert_eq!(
            converted,
            "https://readeckobo.example.com/convert-image?url=https%3A%2F%2Fexample.com%2Fimage.png"
        );
        assert_eq!(
            convert_image_url(&origin, "https://example.com/image.jpg").unwrap(),
            "https://example.com/image.jpg"
        );
    }

    #[test]
    fn parses_since_from_numbers_and_strings() {
        assert_eq!(parse_since(Some(&json!(1700000000))), Some(1700000000));
        assert_eq!(parse_since(Some(&json!("1700000000"))), Some(1700000000));
        assert_eq!(parse_since(Some(&Value::Null)), None);
    }
}
