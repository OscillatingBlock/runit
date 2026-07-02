// src/lib.rs
use anyhow::Context;
use anyhow::Error;
use anyhow::anyhow;
use nix::sys::signal::{self, SigSet};
use nix::unistd::{ForkResult, Pid, execve, fork};
use std::env;
use std::ffi::CString;
use std::ffi::OsString;
use tracing::{error, instrument};

pub struct Service {
    pub child_pid: Option<Pid>,
    pub status: Status,
    pub restart_count: u16,
    pub max_restart: u16,
    pub path: String,
    pub current_delay: u64,
}

impl Service {
    pub fn build() -> anyhow::Result<Service> {
        let args: Vec<OsString> = env::args_os().collect();
        if args.len() != 2 {
            error!("usage: cargo run -- /path/to/service");
            return Err(anyhow!("Invalid arguements provided"));
        }
        let service_path = args
            .get(1)
            .expect("arguement missing")
            .to_str()
            .unwrap()
            .to_string();

        let max_restart = env::var("MAX_RESTART").context("No MAX_RESTART env variable found")?;

        Ok(Service {
            restart_count: 0,
            max_restart: max_restart.parse()?,
            child_pid: None,
            path: service_path,
            status: Status::NotStarted,
            current_delay: 1,
        })
    }
}

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
