//! `handover-mock` binary: one scripted OpenAI-compatible worker.

use clap::Parser;
use handover_mock::{MockConfig, router};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "handover-mock", version, about = "Scripted, deterministic OpenAI-compatible mock worker")]
struct Args {
    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:9001")]
    listen: String,
    /// Worker name recorded in the request log.
    #[arg(long, default_value = "mock-0")]
    worker: String,
    /// Served model name (repeatable). Without any, every model is accepted.
    #[arg(long = "model", value_name = "NAME")]
    models: Vec<String>,
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt().with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))).init();
    let args = Args::parse();
    let listener = tokio::net::TcpListener::bind(&args.listen).await?;
    tracing::info!(
        addr = %listener.local_addr()?,
        worker = %args.worker,
        models = ?args.models,
        "handover-mock listening"
    );
    let app = router(MockConfig { worker: args.worker, models: args.models });
    axum::serve(listener, app).with_graceful_shutdown(shutdown_signal()).await
}

/// Resolve on Ctrl-C or (on Unix) SIGTERM, so the binary stops promptly as
/// PID 1 in a container.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}
