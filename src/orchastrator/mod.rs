use anyhow::Context;
use std::collections::{HashMap, HashSet};
use std::time::Instant;
use std::{hash::Hash, os::fd::AsFd, path::PathBuf, sync::Arc};

use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::signalfd::SignalFd;
use nix::unistd::Pid;
use tracing::{error, info};

use crate::service::ManagedService;
use std::os::unix::net::UnixListener;

mod cli;
pub mod signals;

#[derive(Clone, Debug)]
pub struct Orchastrator {
    active_services: HashMap<Pid, ManagedService>,
    pending_restarts: Vec<(Instant, ManagedService)>,
    failed_services: HashSet<PathBuf>,
    config_dir: PathBuf,
    max_restart: u16,
    listener: Arc<UnixListener>,
    signal_fd: Arc<SignalFd>,
    is_shutting_down: bool,
    tick_count: i32,
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub enum BelongsTo {
    Active,
    Restarting,
}

impl Orchastrator {
    pub fn new(
        config_dir: PathBuf,
        max_restart: u16,
        listener: UnixListener,
        signal_fd: SignalFd,
    ) -> Orchastrator {
        Orchastrator {
            active_services: HashMap::new(),
            pending_restarts: Vec::new(),
            failed_services: HashSet::new(),
            listener: Arc::new(listener),
            signal_fd: Arc::new(signal_fd),
            is_shutting_down: false,
            tick_count: 0,
            config_dir,
            max_restart,
        }
    }

    pub fn run(&mut self) -> anyhow::Result<()> {
        self.scan_config_dir()
            .context("failed to scan config directory")?;
        nix::unistd::alarm::set(1);

        loop {
            let (signal_fired, command_received) =
                self.poll_events().context("failure while polling events")?;
            if signal_fired {
                self.handle_signal()?;
            }
            if command_received {
                if let Ok(mut stream) = self.listener.accept() {
                    info!("CLI connection detected. Processing command stream...");
                    if let Err(e) = self.handle_cli_command(&mut stream.0) {
                        error!("failure while handling command {e}");
                    }
                }

                // A Shutdown command completes the response before we exit,
                // so the client has already received its confirmation.
                if self.is_shutting_down {
                    info!("Shutdown request processed. Exiting event loop.");
                    break;
                }
            }
            self.check_pending_restarts()
                .context("Error checking pending restarts")?;
        }

        self.clean_up_listener();
        info!("Orchastrator exited cleanly.");
        Ok(())
    }

    /// Removes the unix socket file, so a fresh run can bind a new listener.
    fn clean_up_listener(&self) {
        if let Ok(addr) = self.listener.local_addr() {
            if let Some(path) = addr.as_pathname() {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    /// Polls the signal file descriptor and command socket for incoming events.
    fn poll_events(&self) -> anyhow::Result<(bool, bool)> {
        let mut fds = [
            PollFd::new(self.signal_fd.as_fd(), PollFlags::POLLIN), // Index 0: Signals
            PollFd::new(self.listener.as_fd(), PollFlags::POLLIN),  // Index 1: Socket Commands
        ];

        let num_events = poll(&mut fds, PollTimeout::NONE)?;

        let signal_fired = num_events > 0
            && fds[0]
                .revents()
                .unwrap_or_else(PollFlags::empty)
                .contains(PollFlags::POLLIN);

        let command_received = num_events > 0
            && fds[1]
                .revents()
                .unwrap_or_else(PollFlags::empty)
                .contains(PollFlags::POLLIN);

        Ok((signal_fired, command_received))
    }
}
