use anyhow::Context;
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use std::io::BufReader;
use tracing::{debug, error, info, warn};

use runit::protocol::{CliCommand, CliResponse, read_message, write_message};

use crate::service::{ManagedService, ServiceStatus, launch_service};
use std::os::unix::net::UnixStream;

use super::Orchastrator;

//Cli Command handling methods for Orchastrator
impl Orchastrator {
    /// Entry point for processing an incoming client command over a Unix stream.
    /// Clones the stream for writing responses, parses the request, and dispatches it
    /// to the corresponding command handler.
    pub fn handle_cli_command(&mut self, stream: &mut UnixStream) -> anyhow::Result<()> {
        let mut writer = stream
            .try_clone()
            .context("failed to clone stream for writer")?;

        let command = match self.parse_command(&mut BufReader::new(stream)) {
            Ok(cmd) => {
                debug!(?cmd, "Successfully parsed incoming client command");
                cmd
            }
            Err(e) => {
                warn!(error = %e, "Failed to parse command from client; sending error response");
                let response = CliResponse::Error {
                    error: format!("{e}"),
                };
                self.respond_client(response, &mut writer)
                    .context("failed to respond invalid command to client")?;
                return Ok(());
            }
        };

        match command {
            CliCommand::Status => {
                info!("Handling Status command");
                let response =
                    CliResponse::StatusResponse { status: self.format_status_response() };
                self.respond_client(response, &mut writer)
                    .context("failed to handle status command")?;
                info!("Successfully sent status response to client");
            }
            CliCommand::Reload { pid } => {
                info!(pid, "Handling Reload command");
                self.handle_reload_cmd(pid, &mut writer)
                    .context("failed to handle reload service command")?;
            }
            CliCommand::Restart => {
                info!("Handling Restart command");
                self.handle_restart_cmd(&mut writer)
                    .context("failed to handle restart command")?;
            }
            CliCommand::Stop { pid } => {
                info!(pid, "Handling Stop command");
                self.handle_stop_cmd(&mut writer, pid)
                    .context("failed to handle stop command")?;
            }
            CliCommand::Shutdown => {
                info!("Handling Shutdown command");
                self.handle_shutdown_cmd(&mut writer)
                    .context("failed to handle shutdown command")?;
            }
        }

        Ok(())
    }

    /// Reads a length-prefixed `CliCommand` from the stream.
    fn parse_command(&self, reader: &mut BufReader<&mut UnixStream>) -> anyhow::Result<CliCommand> {
        let command: CliCommand = read_message(reader)?;
        debug!(?command, "Successfully parsed incoming client command");
        Ok(command)
    }

    /// Serializes a `CliResponse` into JSON and writes it to the client stream,
    /// prefixed with its 4-byte big-endian length.
    fn respond_client(&self, response: CliResponse, writer: &mut UnixStream) -> anyhow::Result<()> {
        write_message(writer, &response).context("failed to write client response")?;
        Ok(())
    }

    /// Aggregates all alive and dead services tracked by the orchestrator
    /// and formats them into a clean multi-line string status summary.
    fn format_status_response(&self) -> String {
        let mut alive_services = vec![];
        let mut dead_services = vec![];
        for service in self.active_services.iter() {
            match service.1.status {
                ServiceStatus::Alive => {
                    alive_services.push((service.0.clone(), service.1.path.clone()));
                }
                ServiceStatus::Dead => {
                    dead_services.push((service.0.clone(), service.1.path.clone()));
                }
            }
        }

        let total_services = alive_services.len() + dead_services.len();
        let mut status = String::from(format!("State: running\nServices: {}\n", total_services));
        status.push_str("\nAlive Services: \n");

        for sv in alive_services {
            let line = format!("PID: {:?} | Path: {:?}\n", sv.0, sv.1);
            status.push_str(&line);
        }

        status.push_str("\nDead Services: \n");
        for sv in dead_services {
            let line = format!("PID: {:?} | Path: {:?}\n", sv.0, sv.1);
            status.push_str(&line);
        }

        status
    }

    /// Handles the `Restart` command by raising a `SIGALRM` signal on the process
    /// to trigger global service restart logic.
    fn handle_restart_cmd(&self, writer: &mut UnixStream) -> anyhow::Result<()> {
        match signal::raise(Signal::SIGALRM) {
            Ok(_) => {
                info!("Successfully raised SIGALRM signal for restart");
                let response = CliResponse::RestartSuccess;
                self.respond_client(response, writer)
                    .context("failed to send Restart Success response to client")?;
            }
            Err(e) => {
                error!(error = %e, "Failed to raise SIGALRM signal");
                let response = CliResponse::Error {
                    error: format!("failed to restart services {e}"),
                };
                self.respond_client(response, writer)
                    .context("failed to send Restart Failure response to client")?;
                anyhow::bail!("failed to raise SIGALRM signal {e}")
            }
        };
        Ok(())
    }

    /// Handles the `Stop` command by locating a target process by its PID,
    /// removing it from active tracking, and sending a termination signal.
    ///
    /// Sends exactly one response: either a success or an error.
    fn handle_stop_cmd(&mut self, writer: &mut UnixStream, pid: i32) -> anyhow::Result<()> {
        match self.stop_service(pid) {
            Ok(true) => {
                info!(pid, "Successfully stopped child service");
                let response = CliResponse::KillSuccess { pid };
                self.respond_client(response, writer)
                    .context("failed to send KillSuccess response to client")?;
            }
            Ok(false) => {
                warn!(
                    pid,
                    "Attempted to stop service, but PID was not found in active services"
                );
                let response = CliResponse::Error {
                    error: format!("service with pid {pid} was not found in active services."),
                };
                self.respond_client(response, writer)
                    .context("failed to send stop failure response to client")?;
            }
            Err(e) => {
                error!(pid, error = %e, "Failed to kill target child service process");
                let response = CliResponse::Error {
                    error: format!("failed to stop service {pid} : {e}"),
                };
                self.respond_client(response, writer)
                    .context("failed to send failure message to client")?;
                anyhow::bail!("failed to kill child service with pid {pid} : {e}");
            }
        }
        Ok(())
    }

    /// Removes the tracked service from the registry and terminates it.
    /// Returns `Ok(false)` when the PID is not tracked, without taking any action.
    pub(crate) fn stop_service(&mut self, pid: i32) -> anyhow::Result<bool> {
        let pid_t = Pid::from_raw(pid);
        if self.active_services.remove(&pid_t).is_none() {
            return Ok(false);
        }

        debug!(?pid_t, "Sending SIGTERM signal to process");
        signal::kill(pid_t, Signal::SIGTERM)
            .context("failed to send SIGTERM signal to child process")?;
        Ok(true)
    }

    /// Handles the `Reload` command by stopping the target service instance and
    /// launching a fresh instance of the same `run` file under a new PID.
    ///
    /// Sends exactly one response to the client.
    fn handle_reload_cmd(&mut self, pid: i32, writer: &mut UnixStream) -> anyhow::Result<()> {
        // Capture the service path BEFORE stopping: stopping removes the entry.
        let Some(service) = self.active_services.get(&Pid::from_raw(pid)) else {
            warn!(
                pid,
                "Attempted to reload service, but PID was not found in active services"
            );
            let response = CliResponse::Error {
                error: format!("service with pid {pid} is not an active service."),
            };
            return self
                .respond_client(response, writer)
                .context("failed to send reload failure response to client");
        };
        let path = service.path.clone();

        if let Err(e) = self.stop_service(pid) {
            error!(pid, error = %e, "Failed during stop phase of service reload");
            let response = CliResponse::Error {
                error: format!("failed to reload service {pid} : {e}"),
            };
            return self
                .respond_client(response, writer)
                .context("failed to send reload failure response to client");
        }

        let path_str = path.to_str().context("failed to convert service path to string")?;

        info!(pid, path = path_str, "Relaunching service for reload");
        let new_pid = match launch_service(path_str) {
            Ok(new_pid) => new_pid,
            Err(e) => {
                error!(pid, path = path_str, error = %e, "Failed to relaunch service after stopping it");
                let response = CliResponse::Error {
                    error: format!("failed to reload service {pid} : {e}"),
                };
                return self
                    .respond_client(response, writer)
                    .context("failed to send reload failure response to client");
            }
        };

        self.active_services.insert(
            new_pid,
            ManagedService {
                pid: new_pid,
                path,
                status: ServiceStatus::Alive,
                restart_count: 0,
                current_backoff_delay: 1,
            },
        );

        info!(old_pid = pid, ?new_pid, "Successfully reloaded service");
        let response = CliResponse::ReloadSuccess {
            old_pid: pid,
            new_pid: new_pid.as_raw(),
        };
        self.respond_client(response, writer)
            .context("failed to send ReloadSuccess response to client")
    }

    /// Handles the `Shutdown` command by terminating all active managed services,
    /// marking the orchestrator for shutdown, and returning a successful
    /// shutdown confirmation response.
    fn handle_shutdown_cmd(&mut self, writer: &mut UnixStream) -> anyhow::Result<()> {
        self.is_shutting_down = true;

        info!("Initiating orchestrator shutdown sequence for all services");
        if let Err(e) = self.kill_all_services(signal::SIGTERM) {
            error!(error = %e, "Errors encountered while terminating services during shutdown");
            let response = CliResponse::Error {
                error: format!("failed to shutdown : {e}"),
            };
            self.respond_client(response, writer)
                .context("failed to send shutdown failure response to client")?;
        }

        info!("All services terminated; sending ShutdownSuccess response");
        let response = CliResponse::ShutdownSuccess;
        self.respond_client(response, writer)
            .context("failed to send shutdown failure response to client")?;

        Ok(())
    }
}
