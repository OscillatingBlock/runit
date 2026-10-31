//! `runit-client` — tiny CLI to control and inspect the running init system.
//!
//! Usage:
//!   runit-client status
//!   runit-client restart
//!   runit-client stop <pid>
//!   runit-client reload <pid>
//!   runit-client shutdown

use anyhow::Context;
use runit::protocol::{CliCommand, CliResponse, read_message, write_message};
use std::os::unix::net::UnixStream;
use std::process::ExitCode;

const USAGE: &str = "Usage: runit-client <command>\n\nCommands:\n  status\n  restart\n  stop <pid>\n  reload <pid>\n  shutdown";

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("runit-client: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> anyhow::Result<ExitCode> {
    //Load variables from the .env file in the current directory, if present
    dotenvy::dotenv().ok();

    let command = parse_args(std::env::args().skip(1))?;

    let listener_path = std::env::var("LISTENER_PATH")
        .context("No LISTENER_PATH env variable found")?;
    let mut stream = UnixStream::connect(&listener_path)
        .with_context(|| format!("Failed to connect to init system at {listener_path:?}"))?;

    write_message(&mut stream, &command).context("Failed to send command to init system")?;

    let response: CliResponse = read_message(&mut stream)
        .context("Failed to read response from init system")?;

    handle_response(response)
}

/// Builds a `CliCommand` from the raw arguments after `runit-client`.
fn parse_args(mut args: impl Iterator<Item = String>) -> anyhow::Result<CliCommand> {
    let Some(cmd) = args.next() else {
        anyhow::bail!("{USAGE}");
    };

    match cmd.as_str() {
        "status" => Ok(CliCommand::Status),
        "restart" => Ok(CliCommand::Restart),
        "shutdown" => Ok(CliCommand::Shutdown),
        "stop" => Ok(CliCommand::Stop { pid: parse_pid(args)? }),
        "reload" => Ok(CliCommand::Reload { pid: parse_pid(args)? }),
        _ => anyhow::bail!("Unknown command \"{cmd}\"\n\n{USAGE}"),
    }
}

/// Parses the next argument as a pid value.
fn parse_pid(mut args: impl Iterator<Item = String>) -> anyhow::Result<i32> {
    let raw = args.next().context("Missing pid argument")?;
    raw.parse()
        .with_context(|| format!("\"{raw}\" is not a valid pid"))
}

/// Prints one human-readable result per response.
/// Errors go to stderr and map to a failure exit code.
fn handle_response(response: CliResponse) -> anyhow::Result<ExitCode> {
    match response {
        CliResponse::Error { error } => {
            eprintln!("error from init system: {error}");
            Ok(ExitCode::FAILURE)
        }
        CliResponse::StatusResponse { status } => {
            print!("{status}");
            Ok(ExitCode::SUCCESS)
        }
        CliResponse::RestartSuccess => {
            println!("All services are being restarted");
            Ok(ExitCode::SUCCESS)
        }
        CliResponse::KillSuccess { pid } => {
            println!("Service with pid {pid} stopped");
            Ok(ExitCode::SUCCESS)
        }
        CliResponse::ReloadSuccess { old_pid, new_pid } => {
            println!("Service {old_pid} reloaded, now running with new pid {new_pid}");
            Ok(ExitCode::SUCCESS)
        }
        CliResponse::ShutdownSuccess => {
            println!("All services stopped, init system shutting down");
            Ok(ExitCode::SUCCESS)
        }
    }
}
