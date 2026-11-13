use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use readeckobo::app::App;
use readeckobo::config::{Config, LogLevel};
use readeckobo::token;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "readeckobo", version, about = "Readeck to Kobo proxy")]
struct Cli {
    /// Path to the configuration file.
    #[arg(
        short,
        long,
        env = "READECKOBO_CONFIG",
        default_value = "./config.yaml",
        global = true
    )]
    config: PathBuf,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the HTTP proxy (default).
    Serve,
    /// Generate a Kobo device token.
    Token {
        /// Kobo serial number, found under Settings -> Device Information.
        serial: Option<String>,
        /// Mounted Kobo path. Reads the serial from .kobo/version.
        #[arg(long, value_name = "PATH", conflicts_with = "serial")]
        kobo: Option<PathBuf>,
        /// Public origin to include in the printed Kobo configuration.
        #[arg(long, value_name = "URL")]
        public_url: Option<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Token {
            serial,
            kobo,
            public_url,
        }) => {
            let serial = match (serial, kobo) {
                (Some(serial), None) => serial,
                (None, Some(path)) => token::read_serial(&path)?,
                (None, None) => {
                    anyhow::bail!("provide a Kobo serial number or --kobo <mount path>")
                }
                (Some(_), Some(_)) => unreachable!("clap rejects conflicting arguments"),
            };
            token::print(&serial, public_url.as_deref())?;
            Ok(())
        }
        Some(Command::Serve) | None => run_server(cli.config).await,
    }
}

async fn run_server(config_path: PathBuf) -> anyhow::Result<()> {
    let config = Config::load(&config_path)?;
    init_tracing(config.log_level);

    let port = config.server.port;
    let app = Arc::new(App::new(config)?);

    // Do not block startup on Readeck; just surface read-only tokens.
    let permission_check = app.clone();
    tokio::spawn(async move {
        permission_check.warn_on_missing_write_permissions().await;
    });

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    tracing::info!(port, "readeckobo listening");

    axum::serve(listener, app.router())
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

fn init_tracing(level: LogLevel) {
    // Keep dependency debug logs quiet: reqwest/hyper can include full URLs,
    // and the custom backend URL may contain a token path.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(format!(
            "{},reqwest=warn,hyper=warn,hyper_util=warn,h2=warn,rustls=warn",
            level.as_str()
        ))
    });
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("shutdown signal received");
}
