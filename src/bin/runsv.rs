use anyhow::Context;
use tracing::{debug, error, info};

use std::thread;
use std::time::Duration;

use nix::sys::signal::{self, SigSet, Signal};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::Pid;

use runit::{Service, Status, launch_service};

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let mut service = Service::build()?;

    let mut mask = SigSet::empty();
    mask.add(Signal::SIGCHLD);
    mask.add(Signal::SIGINT);
    mask.add(Signal::SIGTERM);

    debug!("Blocking signals");
    signal::pthread_sigmask(signal::SigmaskHow::SIG_BLOCK, Some(&mask), None)
        .context("failed to block SIGCHLD signal")?;

    service.child_pid = Some(launch_service(service.path.as_str())?);
    service.status = Status::Alive;

    loop {
        let caught_signal = mask.wait()?;
        match caught_signal {
            Signal::SIGINT | Signal::SIGTERM => {
                info!(
                    "Received {:?}. Stopping service and exiting.",
                    caught_signal
                );

                // Forward the termination signal to the child so it doesn't become a zombie
                if let Some(child_pid) = service.child_pid {
                    let _ = nix::sys::signal::kill(child_pid, caught_signal);
                    let _ = waitpid(child_pid, None);
                }
                std::process::exit(0);
            }

            Signal::SIGCHLD => loop {
                match waitpid(
                    Pid::from_raw(-1),
                    Some(nix::sys::wait::WaitPidFlag::WNOHANG),
                ) {
                    Ok(WaitStatus::Exited(pid, code)) => {
                        info!("Child process {} exited with code {}", pid, code);
                        if let Some(child_pid) = service.child_pid {
                            if pid == child_pid {
                                service.status = Status::Dead;
                            }
                        }
                    }
                    Ok(WaitStatus::Signaled(pid, signal, _)) => {
                        info!("Child process {} exited with signal {}", pid, signal);
                        if let Some(child_pid) = service.child_pid {
                            if pid == child_pid {
                                service.status = Status::Dead;
                            }
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
            _ => {}
        }

        if service.status == Status::Dead {
            if service.restart_count >= service.max_restart {
                error!(
                    "Service exceeded maximum restart limit ({}). Giving up.",
                    service.max_restart
                );
                std::process::exit(1);
            }
            if let Some(child_pid) = service.child_pid {
                info!(
                    "Service process {} died. Restarting in {} second...",
                    child_pid, service.current_delay
                );
                thread::sleep(Duration::from_secs(service.current_delay));
                service.restart_count += 1;
                service.current_delay = service.current_delay * 2;
            }

            service.child_pid = Some(launch_service(service.path.as_str())?);
            service.status = Status::Alive;
        }
    }
}
