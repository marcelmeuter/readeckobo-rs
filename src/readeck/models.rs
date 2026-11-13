use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer};

#[derive(Debug, Clone, Deserialize)]
pub struct BookmarkSync {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ResourceImage {
    #[serde(default)]
    pub src: String,
    #[serde(default)]
    pub width: i32,
    #[serde(default)]
    pub height: i32,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Resources {
    #[serde(default)]
    pub image: Option<ResourceImage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Bookmark {
    pub id: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default, deserialize_with = "null_to_default")]
    pub description: String,
    #[serde(default, deserialize_with = "null_to_default")]
    pub authors: Vec<String>,
    #[serde(default, deserialize_with = "null_to_default")]
    pub labels: Vec<String>,
    pub created: DateTime<Utc>,
    pub updated: DateTime<Utc>,
    #[serde(default)]
    pub is_archived: bool,
    #[serde(default)]
    pub is_marked: bool,
    #[serde(default, deserialize_with = "null_to_default")]
    pub word_count: i32,
    #[serde(default, deserialize_with = "null_to_default")]
    pub resources: Resources,
}

fn null_to_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}
