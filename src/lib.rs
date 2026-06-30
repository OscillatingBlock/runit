// src/lib.rs
use anyhow::Context;
use anyhow::Error;
use anyhow::anyhow;
use nix::sys::signal::{self, SigSet};
use nix::unistd::{ForkResult, Pid, execve, fork};
use std::ffi::CString;
use tracing::instrument;

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Status {
    Alive,
    Dead,
    NotStarted,
}

#[instrument]
pub fn launch_service(path: &str) -> anyhow::Result<Pid, Error> {
    match unsafe { fork() } {
        Ok(ForkResult::Parent { child, .. }) => Ok(child),
        Ok(ForkResult::Child) => {
            //Reset the signal mask so the child can actually receive signals!
            let empty_mask = SigSet::empty();
            if let Err(e) =
                signal::pthread_sigmask(signal::SigmaskHow::SIG_SETMASK, Some(&empty_mask), None)
            {
                eprintln!("Child failed to reset signal mask: {}", e);
                std::process::exit(1);
            }

            let path = CString::new(path).unwrap();
            let args = [CString::new("run").unwrap()];
            let env = [CString::new("PATH=/bin").unwrap()];
            execve(&path, &args, &env).context("Child process failed to execve: ")?;

            Err(anyhow::Error::msg("execve failed"))
        }
        Err(_) => Err(anyhow!("Fork failed")),
    }
}
