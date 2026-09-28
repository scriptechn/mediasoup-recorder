//! Meeting recorder for SFU egress (`docs/architecture.md`). One binary, two roles picked by `RECORDER_ROLE`:
//! capture (a control socket for the controller, per-track GStreamer pipelines writing the spool, optional
//! upload to S3, optional status over Redis) and compose (the offline worker that renders the composite).
//!
//! `recorder`                       the service, roles from the environment
//! `recorder compose-local <dir>`   compose a spool folder in place with no Redis or bucket
//! `recorder health`                the container healthcheck: GET /health on this recorder, exit 0 on 200

mod capture;
mod compose;
mod config;
mod control;
mod orphans;
mod pipeline;
#[cfg(test)]
mod pipeline_test;
mod policy;
mod ports;
mod raster;
mod registry;
mod scene;
mod spool;
mod storage;
mod wire;
mod worker;

use anyhow::{Context, Result};
use axum::routing::get;
use std::path::PathBuf;
use std::time::Duration;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("health") {
        // The healthcheck: GET /health on our own port; exit 0 on 200. No HTTP client crate and no GStreamer
        // init for a probe that runs every 30 s.
        use std::io::{Read, Write};
        let port: u16 = std::env::var("RECORDER_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(3100);
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        let mut s = std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(3))
            .with_context(|| format!("health: connect {addr}"))?;
        s.set_read_timeout(Some(std::time::Duration::from_secs(3)))?;
        s.write_all(b"GET /health HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n")?;
        let mut reply = String::new();
        let _ = s.read_to_string(&mut reply);
        let status = reply.lines().next().unwrap_or("").to_string();
        if status.contains(" 200 ") {
            return Ok(());
        }
        anyhow::bail!("health: {status}");
    }
    gstreamer::init().context("GStreamer init")?;

    if args.first().map(String::as_str) == Some("compose-local") {
        let dir = PathBuf::from(
            args.get(1)
                .context("usage: recorder compose-local <spool dir>")?,
        );
        let policy_dir = PathBuf::from(
            std::env::var("RECORDER_POLICY_DIR")
                .unwrap_or_else(|_| config::DEFAULT_POLICY_DIR.into()),
        );
        let font = PathBuf::from(
            std::env::var("RECORDER_FONT").unwrap_or_else(|_| config::DEFAULT_FONT.into()),
        );
        let out = worker::compose_local(&dir, &policy_dir, &font)?;
        println!("{}", out.display());
        return Ok(());
    }

    let config = config::Config::from_env()?;
    std::fs::create_dir_all(&config.spool_dir)
        .with_context(|| format!("spool dir {}", config.spool_dir.display()))?;
    tracing::info!(id = %config.id, host = %config.host, port = config.port, role = ?config.role,
        rtc = format!("{}:{}-{}", config.announced_ip, config.rtc_port_min, config.rtc_port_max),
        spool = %config.spool_dir.display(),
        redis = config.redis.is_some(), bucket = config.s3.as_ref().map(|s| s.bucket.as_str()).unwrap_or("none"),
        "recorder starting");

    let storage = match &config.s3 {
        Some(s3) => Some(storage::Storage::new(s3)?),
        None => None,
    };

    if !config.role.captures() {
        // Compose-only: nothing to serve; run until told to stop.
        let worker = worker::Worker::new(config.clone(), storage, None)?;
        tokio::spawn(worker.run());
        wait_for_shutdown().await;
        return Ok(());
    }

    let registry = match &config.redis {
        Some(_) => Some(registry::Registry::connect(&config).await?),
        None => None,
    };
    let capture = capture::Capture::new(config.clone(), registry, storage.clone());
    // Captures a previous run left unfinished are finalised and uploaded before the worker looks at the spool,
    // so it never composes a folder the upload is still reading.
    orphans::sweep(&capture).await;
    if config.role.composes() {
        let worker = worker::Worker::new(config.clone(), storage, Some(capture.clone()))?;
        tokio::spawn(worker.run());
    }

    let (layer, _io) = control::layer(capture.clone());
    let app = axum::Router::new()
        .route("/health", get(|| async { "ok" }))
        .layer(layer);
    let listener = tokio::net::TcpListener::bind((config.listen_host.as_str(), config.port))
        .await
        .with_context(|| format!("bind {}:{}", config.listen_host, config.port))?;

    if let Some(registry) = &capture.registry {
        registry
            .lock()
            .await
            .register(capture.load())
            .await
            .context("register in redis")?;

        // Load announcements every 10 s, so controllers can pick the least busy recorder.
        let capture = capture.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(10));
            loop {
                tick.tick().await;
                let Some(registry) = &capture.registry else {
                    return;
                };
                if let Err(e) = registry.lock().await.publish_load(capture.load()).await {
                    tracing::warn!(error = %e, "load publish failed");
                }
            }
        });
    }

    let shutdown = {
        let capture = capture.clone();
        async move {
            wait_for_shutdown().await;
            tracing::info!("shutdown requested");
            if let Some(registry) = &capture.registry {
                if let Err(e) = registry.lock().await.unregister().await {
                    tracing::warn!(error = %e, "unregister failed");
                }
            }
        }
    };

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}

async fn wait_for_shutdown() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! { _ = ctrl_c => {}, _ = term.recv() => {} }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
}
