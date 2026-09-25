//! Development-only API server for working on the dashboard in a normal browser.
//!
//! This is not a distribution: native installers remain the only supported way to run
//! `AudiobookAI`. The server binds to loopback without the desktop bootstrap, so it must only be
//! used with disposable data. Start it together with the Vite dev server, which proxies `/api`:
//!
//! ```text
//! cargo run -p audiobookai-service --example dev_server -- /tmp/audiobookai-dev
//! pnpm --dir web dev
//! ```
//!
//! The data directory argument is required so a development session can never open the real
//! desktop application data by accident. The secret store still uses the operating-system
//! keychain entry shared with installed builds: an existing master key is only read, and one is
//! created when none exists. Do not store real provider credentials in a development session.

use std::net::{Ipv4Addr, SocketAddr};

use audiobookai_service::ServiceConfig;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let data_dir = std::env::args_os()
        .nth(1)
        .map(std::path::PathBuf::from)
        .ok_or("usage: dev_server <disposable-data-directory>")?;
    std::fs::create_dir_all(&data_dir)?;
    let config = ServiceConfig {
        bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 8484)),
        data_dir,
        bundled_sidecar_dir: None,
        tls: None,
        lan_hostnames: Vec::new(),
        allow_insecure_lan: false,
        desktop_bootstrap: false,
    };
    let handle = audiobookai_service::start(config).await?;
    println!(
        "AudiobookAI development API on http://{} (Ctrl+C to stop)",
        handle.address()
    );
    tokio::signal::ctrl_c().await?;
    handle.shutdown().await?;
    Ok(())
}
