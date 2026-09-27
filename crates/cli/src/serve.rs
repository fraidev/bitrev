use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use bit_rev::config::Config;
use bit_rev::session::Session;
use server::ensure_password;

use crate::download::shutdown_signal;

const HTTP_DRAIN: Duration = Duration::from_secs(5);

pub async fn run(config: Config) -> anyhow::Result<()> {
    let state_dir = util::paths::expand_tilde(&config.state_dir);
    let mut server_config = config.server.clone();
    server_config.password = ensure_password(&server_config, &state_dir)?;

    let session = Arc::new(Session::open(config.session_options()).await?);
    let addr = bind_addr(&server_config)?;
    tracing::info!("bitrev serve listening on http://{addr}");

    let router = server::app(Arc::clone(&session), server_config);
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let mut server_task = tokio::spawn(async move {
        server::serve(router, addr, async move {
            let _ = rx.await;
        })
        .await
    });

    shutdown_signal().await;
    tracing::info!("shutting down");
    let _ = tx.send(());

    tokio::select! {
        result = &mut server_task => {
            result.context("http server task")??;
        }
        _ = tokio::time::sleep(HTTP_DRAIN) => {
            tracing::warn!("http drain exceeded 5s, aborting listener");
            server_task.abort();
        }
        _ = shutdown_signal() => {
            tracing::warn!("second signal, aborting listener");
            server_task.abort();
        }
    }

    eprintln!("Shutting down...");
    session.shutdown_graceful().await;
    Ok(())
}

fn bind_addr(config: &bit_rev::config::ServerConfig) -> anyhow::Result<SocketAddr> {
    let text = format!("{}:{}", config.host, config.port);
    text.parse()
        .with_context(|| format!("invalid server bind address {text}"))
}
