use clap::Parser;
use handover_proxy::{Config, build, router};

/// handover: keeps a session's constraints alive across model handovers.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// TOML configuration (PLAN.md §15).
    #[arg(long, short)]
    config: Option<std::path::PathBuf>,
    /// Quick start without a config file: the next hop's base URL.
    #[arg(long)]
    upstream: Option<String>,
    /// Listen address (overrides the config).
    #[arg(long)]
    listen: Option<String>,
    /// Store path (overrides the config); in-memory when neither is given.
    #[arg(long)]
    store: Option<std::path::PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,handover_proxy=info,handover_register=info".into()),
        )
        .init();
    let args = Args::parse();
    let mut config = match (&args.config, &args.upstream) {
        (Some(path), _) => Config::from_toml(&std::fs::read_to_string(path)?)?,
        (None, Some(url)) => {
            let mut c = Config::for_upstream(url);
            c.extractor.debounce_ms = 1500;
            c
        }
        (None, None) => anyhow::bail!("pass --config <file> or --upstream <url>"),
    };
    if let Some(l) = args.listen {
        config.listen.addr = l;
    }
    if let Some(s) = args.store {
        config.store.path = Some(s);
    }
    let addr = config.listen.addr.clone();
    let loopback = addr.parse::<std::net::SocketAddr>().map(|a| a.ip().is_loopback()).unwrap_or(false) || addr.starts_with("localhost:");
    if config.developers.is_empty() && !loopback && !config.allow_anonymous {
        anyhow::bail!(
            "no [[developer]] tokens are configured, so every request would be anonymous; refusing to listen on {addr}. \
             Configure developers, listen on loopback, or set allow_anonymous = true for a dev stack"
        );
    }
    let state = build(config)?;
    handover_proxy::start_background(&state);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(%addr, upstream = %state.config.upstream().base_url, store = ?state.config.store.path, "handover listening");
    axum::serve(listener, router(state.clone()))
        .with_graceful_shutdown(async {
            let ctrl_c = tokio::signal::ctrl_c();
            #[cfg(unix)]
            {
                let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal");
                tokio::select! { _ = ctrl_c => {}, _ = term.recv() => {} }
            }
            #[cfg(not(unix))]
            {
                let _ = ctrl_c.await;
            }
        })
        .await?;
    state.register.shutdown();
    Ok(())
}
