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

    let source_url = url.clone();
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

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let bytes = match read_limited(response, MAX_IMAGE_BODY).await {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!(%error, "failed to read image");
            return placeholder_response("Image fetch failed");
        }
    };

    // Decoding and SVG rasterization are CPU-bound; keep them off the async workers.
    let converted = tokio::task::spawn_blocking(move || {
        let image = decode_image(&bytes, content_type.as_deref()).map_err(ConvertError::Decode)?;
        encode_jpeg(&image, 85).map_err(ConvertError::Encode)
    })
    .await;

    match converted {
        Ok(Ok(jpeg)) => jpeg_response(jpeg, "public, max-age=3600"),
        Ok(Err(ConvertError::Decode(error))) => {
            tracing::warn!(%error, url = %source_url, "failed to decode image");
            placeholder_response("Image decoding failed")
        }
        Ok(Err(ConvertError::Encode(error))) => {
            tracing::error!(%error, "failed to encode image");
            placeholder_response("Image encoding failed")
        }
        Err(error) => {
            tracing::error!(%error, "image conversion task failed");
            placeholder_response("Image encoding failed")
        }
    }
}

enum ConvertError {
    Decode(String),
    Encode(String),
}

/// Decode a raster image or SVG into RGB, compositing any transparency onto
/// white so transparent areas don't turn black on the e-reader.
fn decode_image(bytes: &[u8], content_type: Option<&str>) -> Result<RgbImage, String> {
    if is_svg(bytes, content_type) {
        return rasterize_svg(bytes);
    }
    let image = image::load_from_memory(bytes).map_err(|error| error.to_string())?;
    Ok(flatten_on_white(&image.to_rgba8()))
}

fn is_svg(bytes: &[u8], content_type: Option<&str>) -> bool {
    if content_type.is_some_and(|value| value.to_ascii_lowercase().contains("image/svg")) {
        return true;
    }
    // Content types are often wrong (e.g. application/octet-stream), so also sniff.
    let head = &bytes[..bytes.len().min(1024)];
    let head = String::from_utf8_lossy(head);
    let head = head.trim_start_matches('\u{feff}').trim_start();
    (head.starts_with("<?xml") || head.starts_with("<svg") || head.starts_with("<!--"))
        && head.contains("<svg")
}

fn rasterize_svg(bytes: &[u8]) -> Result<RgbImage, String> {
    use resvg::{tiny_skia, usvg};

    let mut options = usvg::Options {
        fontdb: svg_fonts(),
        // Used when an SVG names no font at all.
        font_family: "sans-serif".to_string(),
        ..usvg::Options::default()
    };
    // Never let an SVG pull files from the server's disk; only inline data: images render.
    options.image_href_resolver.resolve_string = Box::new(|_, _| None);

    let tree = usvg::Tree::from_data(bytes, &options).map_err(|error| error.to_string())?;
    let size = tree.size();
    let (width, height) = svg_output_size(size.width(), size.height())
        .ok_or_else(|| "SVG has no usable size".to_string())?;

    let mut pixmap = tiny_skia::Pixmap::new(width, height)
        .ok_or_else(|| "failed to allocate SVG canvas".to_string())?;
    pixmap.fill(tiny_skia::Color::WHITE);
    let transform = tiny_skia::Transform::from_scale(
        width as f32 / size.width(),
        height as f32 / size.height(),
    );
    resvg::render(&tree, transform, &mut pixmap.as_mut());

    // The canvas is opaque white, so premultiplied RGBA is plain RGB here.
    let rgb = pixmap
        .data()
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|&[r, g, b, _]| [r, g, b])
        .collect();
    RgbImage::from_raw(width, height, rgb).ok_or_else(|| "invalid SVG raster".to_string())
}

/// Longest side of a rasterized SVG. Small icons render at their own size;
/// big drawings are scaled down to something an e-reader can use.
const MAX_SVG_SIDE: f32 = 1600.0;

fn svg_output_size(width: f32, height: f32) -> Option<(u32, u32)> {
    if !(width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0) {
        return None;
    }
    let scale = (MAX_SVG_SIDE / width.max(height)).min(1.0);
    let width = (width * scale).round().max(1.0) as u32;
    let height = (height * scale).round().max(1.0) as u32;
    Some((width, height))
}

/// Fonts preferred for CSS generic families. fontdb defaults to Microsoft
/// fonts (Arial, Times New Roman, Courier New), which Linux hosts rarely have;
/// without a match, SVG text silently disappears.
const SANS_FONTS: &[&str] = &["DejaVu Sans", "Liberation Sans", "Noto Sans", "Arial"];
const SERIF_FONTS: &[&str] = &[
    "DejaVu Serif",
    "Liberation Serif",
    "Noto Serif",
    "Times New Roman",
];
const MONO_FONTS: &[&str] = &[
    "DejaVu Sans Mono",
    "Liberation Mono",
    "Noto Sans Mono",
    "Courier New",
];

fn svg_fonts() -> std::sync::Arc<resvg::usvg::fontdb::Database> {
    static FONTS: std::sync::OnceLock<std::sync::Arc<resvg::usvg::fontdb::Database>> =
        std::sync::OnceLock::new();
    FONTS
        .get_or_init(|| {
            let mut database = resvg::usvg::fontdb::Database::new();
            database.load_system_fonts();
            if database.is_empty() {
                tracing::warn!("no system fonts found; text in SVG images will not render");
            }
            if let Some(family) = installed_family(&database, SANS_FONTS) {
                database.set_sans_serif_family(family);
            }
            if let Some(family) = installed_family(&database, SERIF_FONTS) {
                database.set_serif_family(family);
            }
            if let Some(family) = installed_family(&database, MONO_FONTS) {
                database.set_monospace_family(family);
            }
            std::sync::Arc::new(database)
        })
        .clone()
}

/// First preferred family that is installed, else any installed family.
fn installed_family(
    database: &resvg::usvg::fontdb::Database,
    preferred: &[&str],
) -> Option<String> {
    let installed = |name: &str| {
        database
            .faces()
            .any(|face| face.families.iter().any(|(family, _)| family == name))
    };
    preferred
        .iter()
        .find(|name| installed(name))
        .map(|name| name.to_string())
        .or_else(|| {
            database
                .faces()
                .find_map(|face| face.families.first().map(|(family, _)| family.clone()))
        })
}

fn flatten_on_white(image: &image::RgbaImage) -> RgbImage {
    RgbImage::from_fn(image.width(), image.height(), |x, y| {
        let [r, g, b, a] = image.get_pixel(x, y).0;
        let blend = |channel: u8| {
            let alpha = u16::from(a);
            ((u16::from(channel) * alpha + 255 * (255 - alpha) + 127) / 255) as u8
        };
        Rgb([blend(r), blend(g), blend(b)])
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rasterizes_svg_at_natural_size_on_white() {
        let svg = br#"<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" width="40" height="20" viewBox="0 0 40 20">
  <rect x="0" y="0" width="20" height="20" fill="rgb(255,0,0)"/>
</svg>"#;
        let image = decode_image(svg, Some("image/svg+xml")).unwrap();
        assert_eq!(image.dimensions(), (40, 20));
        assert_eq!(image.get_pixel(5, 10).0, [255, 0, 0]);
        // The uncovered half is transparent in the SVG and must become white.
        assert_eq!(image.get_pixel(35, 10).0, [255, 255, 255]);
    }

    #[test]
    fn sniffs_svg_without_content_type() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"/>"#;
        assert!(is_svg(svg, Some("application/octet-stream")));
        assert_eq!(decode_image(svg, None).unwrap().dimensions(), (10, 10));
        assert!(!is_svg(b"<!doctype html><html></html>", Some("text/html")));
    }

    #[test]
    fn scales_down_large_svgs() {
        assert_eq!(svg_output_size(3200.0, 800.0), Some((1600, 400)));
        assert_eq!(svg_output_size(23.0, 23.0), Some((23, 23)));
        assert_eq!(svg_output_size(0.0, 10.0), None);
        assert_eq!(svg_output_size(f32::NAN, 10.0), None);
    }

    #[test]
    fn svg_cannot_read_local_files() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="10" height="10">
  <image width="10" height="10" xlink:href="/etc/hostname"/>
</svg>"#;
        let image = decode_image(svg, Some("image/svg+xml")).unwrap();
        assert!(image.pixels().all(|pixel| pixel.0 == [255, 255, 255]));
    }

    #[test]
    fn transparent_png_pixels_become_white() {
        let mut rgba = image::RgbaImage::from_pixel(2, 1, image::Rgba([0, 0, 0, 0]));
        rgba.put_pixel(1, 0, image::Rgba([0, 0, 255, 255]));
        let flattened = flatten_on_white(&rgba);
        assert_eq!(flattened.get_pixel(0, 0).0, [255, 255, 255]);
        assert_eq!(flattened.get_pixel(1, 0).0, [0, 0, 255]);
    }
}
