use crate::orchastrator::signals::block_signals;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;

pub mod orchastrator;
pub mod service;

use anyhow::Context;
use nix::sys::signalfd::SignalFd;

use crate::orchastrator::Orchastrator;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let config_dir: PathBuf = std::env::var("CONFIG_DIR")
        .context("No CONFIG_DIR env variable found")?
        .into();
    if !config_dir.is_dir() {
        anyhow::bail!("CONFIG_DIR is not a valid directory: {:?}", config_dir);
    }
    let max_restart = std::env::var("MAX_RESTART")
        .context("No MAX_RESTART env variable found")?
        .parse()?;

    let listener_path: PathBuf = std::env::var("LISTENER_PATH")
        .context("No LISTENER_PATH env variable found")?
        .into();

    if listener_path.exists() {
        std::fs::remove_file(&listener_path).context("Failed to remove old socket file")?;
    }

    let listener = UnixListener::bind(&listener_path)
        .with_context(|| format!("Failed to bind to socket at {:?}", listener_path))?;
    listener.set_nonblocking(true)?;

    let mask = block_signals()?;

    // This is the bridge, Converts signals into a readable file descriptor
    let signal_fd = SignalFd::new(&mask)?;

    let mut oc = Orchastrator::new(config_dir, max_restart, listener, signal_fd);
    oc.run()
}
