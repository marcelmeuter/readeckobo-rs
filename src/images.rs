use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;
use font8x8::{UnicodeFonts, BASIC_FONTS};
use image::codecs::jpeg::JpegEncoder;
use image::{Rgb, RgbImage};
use url::Url;

use crate::app::App;

const MAX_IMAGE_BODY: usize = 20 * 1024 * 1024;

pub async fn convert_image(
    State(app): State<Arc<App>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(url) = params.get("url") else {
        return text_response(StatusCode::BAD_REQUEST, "Missing 'url' parameter");
    };
    let Ok(url) = Url::parse(url) else {
        return text_response(StatusCode::BAD_REQUEST, "Invalid 'url' parameter");
    };
    if url.scheme() != "http" && url.scheme() != "https" {
        return text_response(StatusCode::BAD_REQUEST, "Invalid 'url' parameter");
    }

    let response = match app
        .image_http
        .get(url)
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(%error, "image fetch failed");
            return placeholder_response("Image fetch failed");
        }
    };

    if response.status() != StatusCode::OK {
        tracing::warn!(status = %response.status(), "image fetch returned non-200");
        return placeholder_response("Image not found");
    }

    let bytes = match read_limited(response, MAX_IMAGE_BODY).await {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!(%error, "failed to read image");
            return placeholder_response("Image fetch failed");
        }
    };

    let image = match image::load_from_memory(&bytes) {
        Ok(image) => image,
        Err(error) => {
            tracing::warn!(%error, "failed to decode image");
            return placeholder_response("Image decoding failed");
        }
    };

    match encode_jpeg(&image.to_rgb8(), 85) {
        Ok(jpeg) => jpeg_response(jpeg, "public, max-age=3600"),
        Err(error) => {
            tracing::error!(%error, "failed to encode image");
            placeholder_response("Image encoding failed")
        }
    }
}

async fn read_limited(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
        if body.len() + chunk.len() > limit {
            return Err("image too large".to_string());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn encode_jpeg(image: &RgbImage, quality: u8) -> Result<Vec<u8>, String> {
    let mut output = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut output, quality);
    encoder
        .encode_image(image)
        .map_err(|error| error.to_string())?;
    Ok(output)
}

fn placeholder_response(message: &str) -> Response {
    let mut image = RgbImage::from_pixel(800, 600, Rgb([255, 255, 255]));
    draw_text(&mut image, 20, 300, message, Rgb([0, 0, 0]), 2);
    match encode_jpeg(&image, 85) {
        Ok(jpeg) => jpeg_response(jpeg, "public, max-age=300"),
        Err(error) => {
            tracing::error!(%error, "failed to encode placeholder image");
            text_response(StatusCode::INTERNAL_SERVER_ERROR, "Image encoding failed")
        }
    }
}

fn jpeg_response(body: Vec<u8>, cache_control: &'static str) -> Response {
    let mut response = Response::new(Body::from(body));
    response
        .headers_mut()
        .insert("content-type", HeaderValue::from_static("image/jpeg"));
    response
        .headers_mut()
        .insert("cache-control", HeaderValue::from_static(cache_control));
    response
}

fn draw_text(image: &mut RgbImage, x: i32, y: i32, text: &str, color: Rgb<u8>, scale: i32) {
    let mut cursor_x = x;
    for character in text.chars() {
        let Some(glyph) = BASIC_FONTS.get(character) else {
            cursor_x += 8 * scale;
            continue;
        };
        for (row, bits) in glyph.iter().enumerate() {
            for column in 0..8 {
                if bits & (1 << column) == 0 {
                    continue;
                }
                for dy in 0..scale {
                    for dx in 0..scale {
                        let px = cursor_x + column * scale + dx;
                        let py = y + row as i32 * scale + dy;
                        if px >= 0
                            && py >= 0
                            && (px as u32) < image.width()
                            && (py as u32) < image.height()
                        {
                            image.put_pixel(px as u32, py as u32, color);
                        }
                    }
                }
            }
        }
        cursor_x += 8 * scale;
    }
}

fn text_response(status: StatusCode, message: &'static str) -> Response {
    let mut response = Response::new(Body::from(message));
    *response.status_mut() = status;
    response
}
