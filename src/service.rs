use anyhow::{Error, anyhow};

use nix::sys::signal::{self, SigSet};
use nix::unistd::{ForkResult, Pid, execve, fork};

use std::ffi::CString;
use std::path::PathBuf;

use tracing::instrument;

#[derive(Clone, Debug)]
pub struct ManagedService {
    pub path: PathBuf,
    pub restart_count: u16,
    pub current_backoff_delay: u64,
    pub status: ServiceStatus,
    pub pid: Pid,
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum ServiceStatus {
    Alive,
    Dead,
}

#[instrument]
pub fn launch_service(path: &str) -> anyhow::Result<Pid, Error> {
    match unsafe { fork() } {
        Ok(ForkResult::Parent { child, .. }) => Ok(child),
        Ok(ForkResult::Child) => {
            //Reset the signal mask so the child can actually receive signals
            let empty_mask = SigSet::empty();
            if let Err(e) =
                signal::pthread_sigmask(signal::SigmaskHow::SIG_SETMASK, Some(&empty_mask), None)
            {
                eprintln!("Child failed to reset signal mask: {}", e);
            }

            //execute the child program
            let path = CString::new(path).unwrap();
            let args = [CString::new("run").unwrap()];
            let env = [CString::new("PATH=/bin").unwrap()];

            //If execve fails we MUST terminate the child here and never return to
            //the caller's code: otherwise the child keeps running a full copy of
            //the init event loop (still holding the socket listener).
            match execve(&path, &args, &env) {
                Err(exec_error) => {
                    eprintln!("Child failed to execve {:?}: {}", path, exec_error);
                    std::process::exit(1);
                }
            }
        }
        Err(_) => Err(anyhow!("Fork failed")),
    }
}
