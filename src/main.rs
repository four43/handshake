//! handshake binary: loads config and secrets, then serves.
//! The server itself lives in `lib.rs`.

use std::{
    env,
    error::Error,
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    sync::Arc,
    time::Duration,
};

use handshake::{default_listen, serve, App, Config};
use tracing::{info, warn};

/// `handshake --healthcheck`: distroless has no curl, so the binary probes itself.
fn healthcheck(listen: &str) -> i32 {
    let port = listen.rsplit(':').next().unwrap_or("8080");
    let Ok(addr) = format!("127.0.0.1:{port}").parse::<SocketAddr>() else {
        return 1;
    };
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_secs(2)) else {
        return 1;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    if stream.write_all(b"GET /healthz HTTP/1.0\r\nHost: localhost\r\n\r\n").is_err() {
        return 1;
    }
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    if response.split_whitespace().nth(1) == Some("200") {
        0
    } else {
        1
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }
    info!("shutting down");
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let config_path = env::var("CONFIG").unwrap_or_else(|_| "/etc/handshake/config.toml".into());

    if env::args().any(|a| a == "--healthcheck") {
        let listen = fs::read_to_string(&config_path)
            .ok()
            .and_then(|t| toml::from_str::<Config>(&t).ok())
            .map(|c| c.listen)
            .unwrap_or_else(default_listen);
        std::process::exit(healthcheck(&listen));
    }

    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cfg: Config = toml::from_str(&fs::read_to_string(&config_path)?)?;
    if cfg.trust_proxy.is_some() {
        warn!("`trust_proxy` is no longer used: X-Forwarded-For is read only from `trusted_proxies` (default: loopback and private networks)");
    }
    if cfg.apps.is_empty() {
        warn!("no apps configured; every request will be rejected");
    }

    let current = env::var("SESSION_SECRET").map_err(|_| "SESSION_SECRET is required")?;
    if current.len() < 32 {
        return Err("SESSION_SECRET must be at least 32 characters".into());
    }
    let mut session_keys = vec![current.into_bytes()];
    if let Ok(prev) = env::var("SESSION_SECRET_PREV") {
        if !prev.is_empty() {
            session_keys.push(prev.into_bytes());
        }
    }
    let builtin_turn = cfg.turn.as_ref().is_some_and(|t| t.builtin());
    let mut turn_key = env::var("TURN_SECRET").ok().filter(|s| !s.is_empty()).map(String::into_bytes);
    if builtin_turn && turn_key.is_none() {
        // Nothing outside this process checks TURN credentials, so a fresh secret per run is enough.
        let mut key = vec![0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut key);
        turn_key = Some(key);
    }
    if cfg.turn.is_some() && turn_key.is_none() {
        warn!("[turn] is configured but TURN_SECRET is not set; /turn will return 503");
    }

    let listen: SocketAddr = cfg.listen.parse()?;
    let state = Arc::new(App::new(cfg, session_keys, turn_key));

    info!(%listen, apps = state.config().apps.len(), "handshake listening");
    let listener = tokio::net::TcpListener::bind(listen).await?;
    serve(listener, state, shutdown_signal()).await?;
    Ok(())
}
