use anyhow::{bail, Result};
use nexup::{config::Config, App};
use std::{path::PathBuf, time::Duration};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nexup=info".into()),
        )
        .json()
        .init();
    let mut args = std::env::args().skip(1);
    let mut config_path = std::env::var_os("NEXUP_CONFIG").map(PathBuf::from);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "serve" => {}
            "--config" => {
                config_path = Some(
                    args.next()
                        .map(PathBuf::from)
                        .ok_or_else(|| anyhow::anyhow!("--config needs a path"))?,
                )
            }
            "--version" | "-V" => {
                println!("nexup {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--help" | "-h" => {
                println!("nexup [serve] [--config FILE]\n\nConfiguration: NEXUP_CONFIG or --config FILE; defaults otherwise.\nSee README.md for environment variables and client setup.");
                return Ok(());
            }
            _ => bail!("unknown argument: {arg}"),
        }
    }
    let config = Config::load(config_path.as_deref())?;
    let listener = tokio::net::TcpListener::bind(&config.server.listen).await?;
    let app = App::new(config).await?;
    app.cleanup().await?;
    let cleanup_app = app.clone();
    let cleanup = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(
            cleanup_app.config.cache.cleanup_interval_seconds,
        ));
        interval.tick().await;
        loop {
            interval.tick().await;
            if let Err(e) = cleanup_app.cleanup().await {
                tracing::error!(error = %e, "cache cleanup failed");
            }
        }
    });
    tracing::info!(listen = %listener.local_addr()?, "Nexup ready");
    axum::serve(listener, nexup::router(app.clone()))
        .with_graceful_shutdown(shutdown())
        .await?;
    cleanup.abort();
    let _ = cleanup.await;
    app.db.close().await;
    Ok(())
}

async fn shutdown() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
}
