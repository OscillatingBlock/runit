use anyhow::Context;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use std::{fs, path::PathBuf};

use nix::sys::signal::{self, Signal};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::Pid;
use tracing::{debug, error, info, instrument, warn};

use crate::service::{ManagedService, ServiceStatus, launch_service};

use super::Orchastrator;
use crate::orchastrator::BelongsTo;

//signal handling methods for Orchastrator
impl Orchastrator {
    /// Reads the pending signal from the SignalFd and handles it accordingly.
    pub fn handle_signal(&mut self) -> anyhow::Result<()> {
        if let Ok(Some(sig_info)) = self.signal_fd.read_signal() {
            let caught_signal = Signal::try_from(sig_info.ssi_signo as libc::c_int)?;

            match caught_signal {
                //parent process received signal to stop, forward it to child processes , then exit
                Signal::SIGINT | Signal::SIGTERM => {
                    info!("Termination signal received. Exiting.");
                    self.is_shutting_down = true;
                    self.kill_all_services(caught_signal)
                        .context("Error killing all services")?;
                    std::process::exit(0);
                }
                //child process changed state
                Signal::SIGCHLD => loop {
                    if !self.handle_child_state_change()? {
                        break;
                    }
                },
                //perform hot reload
                Signal::SIGALRM => {
                    self.hot_reload()?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    #[instrument(skip(self))]
    fn handle_child_state_change(&mut self) -> anyhow::Result<bool> {
        info!("Checking for child process state changes via waitpid");

        match waitpid(
            Pid::from_raw(-1),
            Some(nix::sys::wait::WaitPidFlag::WNOHANG),
        ) {
            Ok(WaitStatus::Exited(child_pid, exit_code)) => {
                debug!(?child_pid, exit_code, "Reaped exited child process");
                self.handle_child_exit(child_pid, exit_code);
            }
            Ok(WaitStatus::Signaled(pid, signal, core_dumped)) => {
                debug!(
                    ?pid,
                    ?signal,
                    core_dumped,
                    "Reaped child process killed by signal"
                );
                self.handle_child_killed_by_signal(pid, signal);
            }
            Ok(WaitStatus::StillAlive) => {
                info!("No terminated children found (StillAlive)");
                return Ok(false);
            }
            Err(e) => {
                if e != nix::errno::Errno::ECHILD {
                    error!(error = %e, "Unexpected waitpid error encountered");
                } else {
                    info!("No child processes left to reap (ECHILD)");
                }
                return Ok(false);
            }
            other => {
                debug!(?other, "Encountered unhandled waitpid status variant");
            }
        }
        Ok(true)
    }

    /// Scans the config directory to discover valid service configuration directories containing a 'run' file,
    /// filtering out any services that have previously failed and hit their max restart limit.
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
                if !self.failed_services.contains(&run_path) {
                    configs.insert(run_path);
                }
            }
        }
        Ok(configs)
    }

    //scans for run file in each sub folder of config dir
    //and launches the service if it exists
    pub fn scan_config_dir(&mut self) -> anyhow::Result<()> {
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
        let on_disk_paths = self.get_current_configs()?;
        let live_paths = self.catalog_services();

        for path in on_disk_paths.iter() {
            if !live_paths.contains_key(path) {
                self.hot_load_service(path);
            }
        }

        for path in live_paths.iter() {
            if !on_disk_paths.contains(path.0) {
                self.hot_unload_service(path.0, &path.1.0, &path.1.1)?;
            }
        }
        Ok(())
    }

    /// Catalogs all currently active and pending restart services into a map,
    /// associating each configuration path with its tracker type and process ID.
    fn catalog_services(&self) -> HashMap<PathBuf, (BelongsTo, Pid)> {
        let mut live_paths = std::collections::HashMap::new();

        for service in self.active_services.values() {
            live_paths.insert(service.path.clone(), (BelongsTo::Active, service.pid));
        }
        for (_, service) in &self.pending_restarts {
            live_paths.insert(service.path.clone(), (BelongsTo::Restarting, service.pid));
        }
        live_paths
    }

    /// Hot-loads a newly discovered service configuration file path by launching it
    /// and registering the new child process into active tracking.
    fn hot_load_service(&mut self, path: &PathBuf) {
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

    /// Handles a service configuration that has been removed from disk by either
    /// terminating the active live process or draining it from the pending restart backoff queue.
    fn hot_unload_service(
        &mut self,
        path: &PathBuf,
        belongs_to: &BelongsTo,
        pid: &Pid,
    ) -> anyhow::Result<()> {
        match belongs_to {
            BelongsTo::Active => {
                info!(
                    "Service config {:?} removed from disk. Terminating live PID: {:?}",
                    path, pid
                );
                // stop_service also removes the pid from active tracking, so the
                // SIGTERM's SIGCHLD will not schedule the service for a restart.
                self.stop_service(pid.as_raw())?;
            }
            BelongsTo::Restarting => {
                info!(
                    "Service config {:?} removed from disk. Draining from backoff queue.",
                    path
                );
                self.pending_restarts.retain(|(_, s)| s.path != *path);
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
            self.failed_services.insert(service.path.clone());
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
    pub fn check_pending_restarts(&mut self) -> anyhow::Result<()> {
        if self.is_shutting_down {
            //Never respawn services while a shutdown is in progress
            return Ok(());
        }

        let ready_services = self.collect_valid_pending_services();
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

    /// Collects and removes all services from the pending restarts queue whose scheduled
    /// backoff delay timers have expired and are ready to be launched again.
    fn collect_valid_pending_services(&mut self) -> Vec<ManagedService> {
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
        ready_services
    }

    /// forawrds the signal (SIGINT or SIGTERM) to all child process,
    /// uses a timeoue of 5 seconds , if child process exit within that timeout we reap them,
    /// if child process do not exit within that timeout we forcefully kill them using SIGKILL
    pub fn kill_all_services(&mut self, signal: Signal) -> anyhow::Result<()> {
        info!(
            "Sending signal {:?} to all active services concurrently...",
            signal
        );

        for (pid, _) in self.active_services.iter() {
            nix::sys::signal::kill(*pid, signal).context("failed to kill process")?;
        }
        self.kill_services();

        self.pending_restarts.clear();
        info!("All services harvested. Clean shutdown complete.");

        Ok(())
    }

    /// Waits for all active child processes to terminate within a timeout threshold,
    /// reaping them as they exit or escalating to forceful termination if they do not respond.
    fn kill_services(&mut self) {
        let timeout = Duration::from_secs(5);
        let start_time = Instant::now();
        let mut escalated = false;
        info!("Harvesting remaining child processes...");
        while !self.active_services.is_empty() {
            self.force_shutdown_if_timeout(start_time, timeout, &mut escalated);

            //check if any child process did exit
            //
            //-1 to wait for ANY child process not just a specific one
            //WNOHANG to return waitpid call instantly if no child process did exit
            match waitpid(
                Pid::from_raw(-1),
                Some(nix::sys::wait::WaitPidFlag::WNOHANG),
            ) {
                Ok(WaitStatus::Exited(pid, _)) | Ok(WaitStatus::Signaled(pid, _, _)) => {
                    self.active_services.remove(&pid);
                }
                //if no child process has exited wait for some time before checking again
                Ok(WaitStatus::StillAlive) => {
                    std::thread::sleep(Duration::from_millis(50));
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
    }

    /// Checks if the shutdown timeout has been exceeded; if so, escalates
    /// by sending a `SIGKILL` signal to all remaining active child processes.
    fn force_shutdown_if_timeout(
        &self,
        start_time: Instant,
        timeout: Duration,
        escalated: &mut bool,
    ) {
        if !*escalated && start_time.elapsed() > timeout {
            warn!(
                "Timeout reached! Some services failed to stop gracefully. Escalating to SIGKILL..."
            );

            // Send SIGKILL to whatever services are still remaining in active_services
            for pid in self.active_services.keys() {
                // ignore error if it died just now
                let _ = signal::kill(*pid, Signal::SIGKILL);
            }
            *escalated = true;
        }
    }

    /// Handles a child process exit event by marking it as dead and scheduling
    /// a restart if it crashed (non-zero exit status).
    fn handle_child_exit(&mut self, child_pid: Pid, exit_code: i32) {
        if let Some(mut service) = self.active_services.remove(&child_pid) {
            service.status = ServiceStatus::Dead;
            //code 0 means process exited successfully, any other code means it
            //crashed, restart only crashed processes
            if exit_code == 0 {
                info!(
                    "Service {:?} exited successfully (code 0). Marking as finished.",
                    service.path
                );
            } else {
                error!(
                    "Service {:?} crashed with code {}. Scheduling restart...",
                    service.path, exit_code
                );
                self.process_service_death(service);
            }
        }
    }

    /// Handles a child process that was terminated by a signal, scheduling a restart
    /// if the orchestrator is not currently shutting down.
    fn handle_child_killed_by_signal(&mut self, pid: Pid, signal: Signal) {
        if let Some(mut service) = self.active_services.remove(&pid) {
            service.status = ServiceStatus::Dead;

            // ONLY process service death (restart) if we are NOT shutting down
            if !self.is_shutting_down {
                info!(
                    "Service {:?} running at PID {pid} dropped out {signal}.",
                    service.path
                );
                self.process_service_death(service);
            } else {
                info!(
                    "Service {:?} terminated during shutdown. Not restarting.",
                    service.path
                );
            }
        }
    }

    /// Performs a periodic hot-reload tick check, resetting the alarm timer and
    /// synchronizing the configuration directory after a configured number of ticks.
    fn hot_reload(&mut self) -> anyhow::Result<()> {
        // Heartbeat tick unblocks mask.wait(). No operation needed here.
        debug!("Timer heartbeat tick received.");
        nix::unistd::alarm::set(1);

        self.tick_count += 1;
        if self.tick_count >= 5 {
            self.sync_config_dir()
                .context("failed to sync config directory")?;
            self.tick_count = 0;
        }
        Ok(())
    }
}

use nix::sys::signal::SigSet;

pub fn block_signals() -> anyhow::Result<SigSet> {
    let mut mask = SigSet::empty();
    mask.add(Signal::SIGCHLD);
    mask.add(Signal::SIGINT);
    mask.add(Signal::SIGTERM);
    mask.add(Signal::SIGALRM);

    debug!("Blocking signals");
    signal::pthread_sigmask(signal::SigmaskHow::SIG_BLOCK, Some(&mask), None)
        .context("parent process failed to mask signals")?;
    Ok(mask)
}
