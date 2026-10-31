use anyhow::Context;
use nix::unistd::Pid;
use runit::launch_service;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use nix::sys::signal::{self, SigSet, Signal};
use nix::sys::wait::{WaitStatus, waitpid};
use tracing::{debug, error, info, instrument};

#[derive(Clone, Debug)]
struct Orchastrator {
    active_services: HashMap<Pid, ManagedService>,
    pending_restarts: Vec<(Instant, ManagedService)>,
    config_dir: PathBuf,
    max_restart: u16,
}

#[derive(Clone, Debug)]
struct ManagedService {
    path: PathBuf,
    restart_count: u16,
    current_backoff_delay: u64,
    status: ServiceStatus,
    pid: Pid,
}

#[derive(Debug, PartialEq, Clone, Copy)]
enum ServiceStatus {
    Alive,
    Dead,
}

impl Orchastrator {
    fn build() -> anyhow::Result<Orchastrator> {
        let config_dir: PathBuf = std::env::var("CONFIG_DIR")
            .context("No CONFIG_DIR env variable found")?
            .into();
        if !config_dir.is_dir() {
            anyhow::bail!("CONFIG_DIR is not a valid directory: {:?}", config_dir);
        }

        let max_restart = std::env::var("MAX_RESTART")
            .context("No MAX_RESTART env variable found")?
            .parse()?;

        Ok(Orchastrator {
            active_services: HashMap::new(),
            pending_restarts: Vec::new(),
            config_dir,
            max_restart,
        })
    }

    fn get_current_configs(&self) -> Result<std::collections::HashSet<PathBuf>, anyhow::Error> {
        let mut configs = std::collections::HashSet::new();
        for entry in
            fs::read_dir(self.config_dir.clone()).context("Failed to read config directory")?
        {
            let entry = entry?;
            let path = entry.path();

            if path.is_dir() {
                let run_path = path.join("run");

                if let Err(e) = fs::File::open(&run_path) {
                    error!(
                        "Skipping service folder {:?}: Could not open 'run' file: {}",
                        path, e
                    );
                    continue;
                }
                configs.insert(run_path);
            }
        }
        Ok(configs)
    }

    //scans for run file in each sub folder of config dir
    //and launches the service if it exists
    fn scan_config_dir(&mut self) -> anyhow::Result<()> {
        let configs = self.get_current_configs()?;
        for path in configs {
            info!("Starting service at path {:?}", path);

            match launch_service(path.to_str().unwrap()) {
                Ok(child_pid) => {
                    self.active_services.insert(
                        child_pid,
                        ManagedService {
                            pid: child_pid,
                            path: path,
                            status: ServiceStatus::Alive,
                            restart_count: 0,
                            current_backoff_delay: 1,
                        },
                    );
                }
                Err(e) => {
                    error!("Failed to launch service at {:?}: {}", path, e);
                }
            }
        }
        Ok(())
    }

    #[instrument]
    fn sync_config_dir(&mut self) -> anyhow::Result<()> {
        #[derive(Debug, PartialEq, Eq, Hash)]
        enum BelongsTo {
            Active,
            Restarting,
        }
        let mut live_paths = std::collections::HashMap::new();

        for service in self.active_services.values() {
            live_paths.insert(service.path.clone(), (BelongsTo::Active, service.pid));
        }
        for (_, service) in &self.pending_restarts {
            live_paths.insert(service.path.clone(), (BelongsTo::Restarting, service.pid));
        }

        let on_disk_paths = self.get_current_configs()?;

        for path in on_disk_paths.iter() {
            if !live_paths.contains_key(path) {
                info!(
                    "Hot-loading new service configuration discovered at: {:?}",
                    path
                );

                match launch_service(path.to_str().unwrap()) {
                    Ok(child_pid) => {
                        // This is now perfectly safe because `live_paths` holds no references to `self`!
                        self.active_services.insert(
                            child_pid,
                            ManagedService {
                                pid: child_pid,
                                path: path.clone(),
                                status: ServiceStatus::Alive,
                                restart_count: 0,
                                current_backoff_delay: 1,
                            },
                        );
                    }
                    Err(e) => {
                        error!("Failed to launch hot-loaded service at {:?}: {}", path, e);
                    }
                }
            }
        }

        for path in live_paths.iter() {
            if !on_disk_paths.contains(path.0) {
                match path.1.0 {
                    BelongsTo::Active => {
                        info!(
                            "Service config {:?} removed from disk. Terminating live PID: {:?}",
                            path.0, path.1.1
                        );
                        nix::sys::signal::kill(path.1.1, signal::SIGTERM)
                            .context("Failed to kill deleted service")?;
                    }
                    BelongsTo::Restarting => {
                        info!(
                            "Service config {:?} removed from disk. Draining from backoff queue.",
                            path.0
                        );
                        self.pending_restarts.retain(|(_, s)| s.path != *path.0);
                    }
                }
            }
        }
        Ok(())
    }

    //push a service to the pending_restarts vector with a backoff delay
    //if restart_count exceeds max_restart then log an error and do not push to pending_restarts
    fn process_service_death(&mut self, mut service: ManagedService) {
        if service.restart_count >= self.max_restart {
            error!(
                "Service at path {:?} exceeded max restart limit. Giving up.",
                service.path
            );
        } else {
            info!(
                "Service {:?} scheduled to restart in {} seconds.",
                service.path, service.current_backoff_delay
            );

            let wake_time = Instant::now() + Duration::from_secs(service.current_backoff_delay);
            service.restart_count += 1;
            service.current_backoff_delay *= 2;

            self.pending_restarts.push((wake_time, service));
        }
    }

    //restart pending services whose backoff timers have finished
    #[instrument]
    fn check_pending_restarts(&mut self) -> anyhow::Result<()> {
        let mut ready_services = Vec::new();
        let mut i = 0;

        // Drain services whose backoff timers have finished
        //
        // cannot use for loop while removing items from vector
        while i < self.pending_restarts.len() {
            if Instant::now() >= self.pending_restarts[i].0 {
                let (_, service) = self.pending_restarts.remove(i);
                ready_services.push(service);
            } else {
                i += 1;
            }
        }

        for mut service in ready_services {
            match launch_service(service.path.to_str().unwrap()) {
                Ok(new_pid) => {
                    info!(
                        "Successfully restarted service {:?}. New PID: {:?}",
                        service.path, new_pid
                    );
                    service.status = ServiceStatus::Alive;
                    self.active_services.insert(new_pid, service);
                }
                Err(e) => {
                    error!(
                        "Failed to restart service {:?}: {}. Scheduling another restart...",
                        service.path, e
                    );
                    self.process_service_death(service);
                }
            }
        }
        Ok(())
    }

    fn kill_all_services(&mut self, signal: Signal) -> anyhow::Result<()> {
        info!(
            "Sending signal {:?} to all active services concurrently...",
            signal
        );

        for (pid, _) in self.active_services.iter() {
            nix::sys::signal::kill(*pid, signal).context("failed to kill process")?;
        }

        info!("Harvesting remaining child processes...");
        while !self.active_services.is_empty() {
            match waitpid(
                Pid::from_raw(-1),
                Some(nix::sys::wait::WaitPidFlag::WNOHANG),
            ) {
                Ok(WaitStatus::Exited(pid, _)) | Ok(WaitStatus::Signaled(pid, _, _)) => {
                    self.active_services.remove(&pid);
                }
                Ok(WaitStatus::StillAlive) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(nix::errno::Errno::ECHILD) => {
                    break;
                }
                Err(e) => {
                    error!("Unexpected error while harvesting services: {}", e);
                    break;
                }
                _ => {}
            }
        }

        self.pending_restarts.clear();
        info!("All services harvested. Clean shutdown complete.");

        Ok(())
    }
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let mut oc = Orchastrator::build().context("failed to build Orchastrator")?;
    let mut mask = SigSet::empty();
    mask.add(Signal::SIGCHLD);
    mask.add(Signal::SIGINT);
    mask.add(Signal::SIGTERM);
    mask.add(Signal::SIGALRM);

    debug!("Blocking signals");
    signal::pthread_sigmask(signal::SigmaskHow::SIG_BLOCK, Some(&mask), None)
        .context("failed to block SIGCHLD signal")?;

    oc.scan_config_dir()
        .context("failed to scan config directory")?;

    nix::unistd::alarm::set(1);
    let mut tick_count = 0;

    loop {
        let caught_signal = mask.wait()?;
        match caught_signal {
            Signal::SIGCHLD => loop {
                match waitpid(
                    Pid::from_raw(-1),
                    Some(nix::sys::wait::WaitPidFlag::WNOHANG),
                ) {
                    Ok(WaitStatus::Exited(pid, code)) => {
                        if let Some(mut service) = oc.active_services.remove(&pid) {
                            service.status = ServiceStatus::Dead;
                            //code 0 means process exited successfully, any other code means it
                            //crashed, restart only crashed processes
                            if code == 0 {
                                info!(
                                    "Service {:?} exited successfully (code 0). Marking as finished.",
                                    service.path
                                );
                            } else {
                                error!(
                                    "Service {:?} crashed with code {}. Scheduling restart...",
                                    service.path, code
                                );
                                oc.process_service_death(service);
                            }
                        }
                    }

                    Ok(WaitStatus::Signaled(pid, signal, _)) => {
                        if let Some(service) = oc.active_services.get_mut(&pid) {
                            service.status = ServiceStatus::Dead;
                            let path = service.path.clone();
                            info!("Service {path:?} running at PID {pid} dropped out {signal}.");
                        }

                        if let Some(mut service) = oc.active_services.remove(&pid) {
                            service.status = ServiceStatus::Dead;
                            oc.process_service_death(service);
                        }
                    }

                    Ok(WaitStatus::StillAlive) => break,
                    Err(e) => {
                        if e != nix::errno::Errno::ECHILD {
                            error!("Unexpected waitpid error: {}", e);
                        }
                        break;
                    }
                    _ => {}
                }
            },

            Signal::SIGALRM => {
                // Heartbeat tick unblocks mask.wait(). No operation needed here.
                info!("Timer heartbeat tick received.");
                nix::unistd::alarm::set(1);

                tick_count += 1;
                if tick_count >= 5 {
                    oc.sync_config_dir()
                        .context("failed to sync config directory")?;
                    tick_count = 0; // Resets safely after a true 5-second window
                }
            }
            Signal::SIGINT | Signal::SIGTERM => {
                info!("Termination signal received. Exiting.");
                oc.kill_all_services(caught_signal)
                    .context("Error killing all services")?;
                std::process::exit(0);
            }

            _ => {}
        }
        oc.check_pending_restarts()
            .context("Error checking pending restarts")?;

        // Tiny sleep to prevent high CPU usage when no signals are firing
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
