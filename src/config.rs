use std::collections::HashSet;
use std::path::Path;

use serde::Deserialize;
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
}

impl LogLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
        }
    }

    pub fn as_tracing(self) -> tracing::Level {
        match self {
            Self::Error => tracing::Level::ERROR,
            Self::Warn => tracing::Level::WARN,
            Self::Info => tracing::Level::INFO,
            Self::Debug => tracing::Level::DEBUG,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    #[default]
    Kobo,
    Custom,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_port")]
    pub port: u16,
}

fn default_port() -> u16 {
    8080
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: default_port(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReadeckConfig {
    pub host: Url,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct KoboConfig {
    #[serde(default)]
    pub backend: BackendKind,
    pub custom_backend_url: Option<Url>,
    /// Deprecated alias for `custom_backend_url`, kept for configs written
    /// before the backend switch was introduced.
    pub store_api_url: Option<Url>,
    /// When true, incremental sync sends archived bookmarks with Kobo
    /// status "2" (remove from the device) instead of "1" (archived).
    #[serde(default)]
    pub remove_archived_from_kobo: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub token: String,
    pub readeck_access_token: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub log_level: LogLevel,
    /// Public origin used in rewritten Kobo initialization responses.
    /// When omitted, the origin is derived from the request and its
    /// forwarding headers.
    pub public_url: Option<Url>,
    pub readeck: ReadeckConfig,
    #[serde(default)]
    pub kobo: KoboConfig,
    pub users: Vec<User>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config: {0}")]
    Read(#[from] std::io::Error),
    #[error("failed to parse config: {0}")]
    Parse(#[from] serde_yaml_ng::Error),
    #[error("invalid configuration: {0}")]
    Invalid(String),
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let contents = std::fs::read_to_string(path)?;
        Self::from_yaml(&contents)
    }

    pub fn from_yaml(contents: &str) -> Result<Self, ConfigError> {
        let config: Self = serde_yaml_ng::from_str(contents)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.server.port == 0 {
            return Err(ConfigError::Invalid(
                "server.port must be between 1 and 65535".into(),
            ));
        }
        validate_http_url("readeck.host", &self.readeck.host, true)?;

        if let Some(public_url) = &self.public_url {
            validate_http_url("public_url", public_url, false)?;
            if public_url.query().is_some() || public_url.fragment().is_some() {
                return Err(ConfigError::Invalid(
                    "public_url must not contain a query or fragment".into(),
                ));
            }
        }

        if self.kobo.custom_backend_url.is_some() && self.kobo.store_api_url.is_some() {
            return Err(ConfigError::Invalid(
                "set only one of kobo.custom_backend_url or kobo.store_api_url".into(),
            ));
        }

        match self.kobo.backend {
            BackendKind::Kobo => {
                if let Some(url) = self
                    .kobo
                    .custom_backend_url
                    .as_ref()
                    .or(self.kobo.store_api_url.as_ref())
                {
                    validate_custom_backend_url(url)?;
                }
            }
            BackendKind::Custom => {
                let Some(custom) = self
                    .kobo
                    .custom_backend_url
                    .as_ref()
                    .or(self.kobo.store_api_url.as_ref())
                else {
                    return Err(ConfigError::Invalid(
                        "kobo.custom_backend_url is required when kobo.backend is \"custom\""
                            .into(),
                    ));
                };
                validate_custom_backend_url(custom)?;
            }
        }

        if self.users.is_empty() {
            return Err(ConfigError::Invalid("at least one user is required".into()));
        }

        let mut tokens = HashSet::new();
        for (index, user) in self.users.iter().enumerate() {
            if user.token.trim().is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "users[{index}].token must not be empty"
                )));
            }
            if user.readeck_access_token.trim().is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "users[{index}].readeck_access_token must not be empty"
                )));
            }
            if !tokens.insert(user.token.as_str()) {
                return Err(ConfigError::Invalid(format!(
                    "users[{index}].token is duplicated"
                )));
            }
        }

        Ok(())
    }
}

fn validate_http_url(name: &str, url: &Url, allow_path: bool) -> Result<(), ConfigError> {
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(ConfigError::Invalid(format!(
            "{name} must use http or https"
        )));
    }
    if url.host_str().is_none() {
        return Err(ConfigError::Invalid(format!("{name} must contain a host")));
    }
    if !allow_path && url.path() != "/" {
        return Err(ConfigError::Invalid(format!(
            "{name} must not contain a path"
        )));
    }
    Ok(())
}

fn validate_custom_backend_url(url: &Url) -> Result<(), ConfigError> {
    validate_http_url("kobo.custom_backend_url", url, true)?;
    if url.query().is_some() || url.fragment().is_some() {
        return Err(ConfigError::Invalid(
            "kobo.custom_backend_url must not contain a query or fragment".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Backend;

    const VALID: &str = r#"
server:
  port: 9090
log_level: debug
public_url: "https://readeckobo.example.com"
readeck:
  host: "https://readeck.example.com"
kobo:
  backend: custom
  custom_backend_url: "https://library.example.com/api/kobo/token"
users:
  - token: "device-token"
    readeck_access_token: "readeck-token"
"#;

    #[test]
    fn loads_valid_config() {
        let config = Config::from_yaml(VALID).unwrap();
        assert_eq!(config.server.port, 9090);
        assert_eq!(config.log_level, LogLevel::Debug);
        assert_eq!(config.kobo.backend, BackendKind::Custom);
        assert_eq!(
            config.kobo.custom_backend_url.unwrap().as_str(),
            "https://library.example.com/api/kobo/token"
        );
    }

    #[test]
    fn applies_defaults() {
        let config = Config::from_yaml(
            r#"
readeck:
  host: "https://readeck.example.com"
users:
  - token: "device-token"
    readeck_access_token: "readeck-token"
"#,
        )
        .unwrap();
        assert_eq!(config.server.port, 8080);
        assert_eq!(config.log_level, LogLevel::Info);
        assert_eq!(config.kobo.backend, BackendKind::Kobo);
        assert!(config.kobo.custom_backend_url.is_none());
    }

    #[test]
    fn rejects_custom_backend_without_url() {
        let err = Config::from_yaml(
            r#"
readeck:
  host: "https://readeck.example.com"
kobo:
  backend: custom
users:
  - token: "device-token"
    readeck_access_token: "readeck-token"
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("custom_backend_url is required"));
    }

    #[test]
    fn accepts_legacy_store_api_url() {
        let config = Config::from_yaml(
            r#"
readeck:
  host: "https://readeck.example.com"
kobo:
  store_api_url: "https://library.example.com/api/kobo/token"
users:
  - token: "device-token"
    readeck_access_token: "readeck-token"
"#,
        )
        .unwrap();
        let backend = Backend::from_config(&config.kobo).unwrap();
        assert_eq!(
            backend.base().as_str(),
            "https://library.example.com/api/kobo/token"
        );
    }

    #[test]
    fn rejects_both_custom_backend_url_and_store_api_url() {
        let err = Config::from_yaml(
            r#"
readeck:
  host: "https://readeck.example.com"
kobo:
  custom_backend_url: "https://library.example.com/api/kobo/one"
  store_api_url: "https://library.example.com/api/kobo/two"
users:
  - token: "device-token"
    readeck_access_token: "readeck-token"
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("set only one"));
    }

    #[test]
    fn rejects_duplicate_tokens() {
        let err = Config::from_yaml(
            r#"
readeck:
  host: "https://readeck.example.com"
users:
  - token: "device-token"
    readeck_access_token: "one"
  - token: "device-token"
    readeck_access_token: "two"
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("duplicated"));
    }

    #[test]
    fn rejects_non_http_readeck_host() {
        let err = Config::from_yaml(
            r#"
readeck:
  host: "ftp://readeck.example.com"
users:
  - token: "device-token"
    readeck_access_token: "readeck-token"
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("must use http or https"));
    }
}
