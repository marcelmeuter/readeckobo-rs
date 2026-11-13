use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Default, Deserialize)]
pub struct KoboGetRequest {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub count: Option<String>,
    #[serde(default)]
    pub offset: Option<String>,
    #[serde(default)]
    pub since: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct KoboGetResponse {
    pub status: i32,
    pub list: HashMap<String, KoboArticleItem>,
    pub total: i32,
}

#[derive(Debug, Default, Deserialize)]
pub struct KoboDownloadRequest {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub consumer_key: String,
    #[serde(default)]
    pub images: i32,
    #[serde(default)]
    pub refresh: i32,
    #[serde(default)]
    pub output: String,
    #[serde(default)]
    pub url: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct KoboDownloadForm {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub consumer_key: String,
    #[serde(default)]
    pub images: i32,
    #[serde(default)]
    pub refresh: i32,
    #[serde(default)]
    pub output: String,
    #[serde(default)]
    pub url: String,
}

impl From<KoboDownloadForm> for KoboDownloadRequest {
    fn from(form: KoboDownloadForm) -> Self {
        Self {
            access_token: form.access_token,
            consumer_key: form.consumer_key,
            images: form.images,
            refresh: form.refresh,
            output: form.output,
            url: form.url,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct KoboSendRequest {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub actions: Vec<Value>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct KoboArticleItem {
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub authors: HashMap<String, KoboAuthor>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub excerpt: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub favorite: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub given_title: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub given_url: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub has_image: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub has_video: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<KoboImage>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub images: HashMap<String, KoboImage>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub is_article: String,
    pub item_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub resolved_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub resolved_title: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub resolved_url: String,
    pub status: String,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub tags: HashMap<String, KoboTag>,
    #[serde(skip_serializing_if = "is_zero_i64")]
    pub time_added: i64,
    #[serde(skip_serializing_if = "is_zero_i64")]
    pub time_read: i64,
    #[serde(skip_serializing_if = "is_zero_i64")]
    pub time_updated: i64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub videos: Vec<Value>,
    #[serde(skip_serializing_if = "is_zero_i32")]
    pub word_count: i32,
    #[serde(rename = "_optional", skip_serializing_if = "HashMap::is_empty")]
    pub optional: HashMap<String, Value>,
}

fn is_zero_i64(value: &i64) -> bool {
    *value == 0
}

fn is_zero_i32(value: &i32) -> bool {
    *value == 0
}

#[derive(Debug, Clone, Serialize)]
pub struct KoboAuthor {
    pub author_id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct KoboImage {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub image_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub item_id: String,
    pub src: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct KoboTag {
    pub item_id: String,
    pub tag: String,
}
