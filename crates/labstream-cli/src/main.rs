//! Command-line tools for labstream.
//!
//! Commands:
//! - `labstream doctor`: diagnose LSL discovery, multicast, broadcast,
//!   interfaces, and remote peers (`docs/discovery.md`).
//! - `labstream top`: live terminal monitor using `Inlet::health`
//!   (`docs/tools.md`).
//! - `labstream play`: replay an XDF 1.0 recording as live LSL outlets
//!   (`docs/tools.md`).

#![deny(unsafe_code)]

mod play;
mod top;
mod xdf;

use labstream_net::doctor::{self, Options};
use std::process::ExitCode;
use std::time::Duration;

fn usage() -> &'static str {
    "Usage:
  labstream <COMMAND> [OPTIONS]
  labstream --help
  labstream --version

Commands:
  doctor    Run network and discovery diagnostics.
  top       Monitor active LSL streams using the health API.
  play      Replay an XDF 1.0 recording as live LSL outlets."
}

fn doctor_usage() -> &'static str {
    "Usage:
  labstream doctor [--peer IP[,IP]] [--timeout SECONDS] [--json]

Options:
  --peer IP[,IP]     Test UDP and TCP discovery to one or more remote peers.
                     Repeat the flag or separate addresses with commas.
  --timeout SECONDS  Wait time for live discovery and peer probes (default: 1.5).
  --json             Write the report as JSON instead of plain text.
  -h, --help         Show this help text."
}

fn parse_doctor_args(args: &[String]) -> Result<(Options, bool, bool), String> {
    let mut opts = Options::default();
    let mut json = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => return Ok((opts, json, true)),
            "--json" => json = true,
            "--peer" => {
                i += 1;
                let value = args
                    .get(i)
                    .ok_or_else(|| "missing value after --peer".to_string())?;
                for peer in value.split(',') {
                    let trimmed = peer.trim();
                    if !trimmed.is_empty() {
                        opts.peers.push(trimmed.to_string());
                    }
                }
            }
            "--timeout" => {
                i += 1;
                let value = args
                    .get(i)
                    .ok_or_else(|| "missing value after --timeout".to_string())?;
                let secs: f64 = value
                    .parse()
                    .map_err(|_| format!("invalid timeout value: {value}"))?;
                if !secs.is_finite() || secs <= 0.0 {
                    return Err(format!("timeout must be a positive number: {value}"));
                }
                opts.timeout = Duration::from_secs_f64(secs);
            }
            other => {
                if let Some(value) = other.strip_prefix("--peer=") {
                    for peer in value.split(',') {
                        let trimmed = peer.trim();
                        if !trimmed.is_empty() {
                            opts.peers.push(trimmed.to_string());
                        }
                    }
                } else if let Some(value) = other.strip_prefix("--timeout=") {
                    let secs: f64 = value
                        .parse()
                        .map_err(|_| format!("invalid timeout value: {value}"))?;
                    if !secs.is_finite() || secs <= 0.0 {
                        return Err(format!("timeout must be a positive number: {value}"));
                    }
                    opts.timeout = Duration::from_secs_f64(secs);
                } else {
                    return Err(format!("unknown argument: {other}"));
                }
            }
        }
        i += 1;
    }
    Ok((opts, json, false))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first().map(|s| s.as_str()) else {
        eprintln!("{}", usage());
        return ExitCode::from(2);
    };

    match cmd {
        "-h" | "--help" | "help" => {
            println!("{}", usage());
            ExitCode::SUCCESS
        }
        "-V" | "--version" => {
            println!("labstream {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        "doctor" => match parse_doctor_args(&args[1..]) {
            Ok((_, _, true)) => {
                println!("{}", doctor_usage());
                ExitCode::SUCCESS
            }
            Ok((opts, json, false)) => {
                let report = doctor::diagnose(&opts);
                if json {
                    println!("{}", report.to_json());
                } else {
                    print!("{}", report.format_text());
                }
                if report.has_failures() {
                    ExitCode::from(1)
                } else {
                    ExitCode::SUCCESS
                }
            }
            Err(err) => {
                eprintln!("error: {err}\n\n{}", doctor_usage());
                ExitCode::from(2)
            }
        },
        "top" => top::run_top(&args[1..]),
        "play" => play::run_play(&args[1..]),
        other => {
            eprintln!("error: unknown command '{other}'\n\n{}", usage());
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_doctor_accepts_peers_timeout_and_json() {
        let args: Vec<String> = [
            "--peer",
            "192.168.1.10,192.168.1.11",
            "--peer=10.0.0.5",
            "--timeout",
            "2.5",
            "--json",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let (opts, json, help) = parse_doctor_args(&args).unwrap();
        assert!(!help);
        assert!(json);
        assert_eq!(opts.peers, vec!["192.168.1.10", "192.168.1.11", "10.0.0.5"]);
        assert_eq!(opts.timeout, Duration::from_secs_f64(2.5));
    }

    #[test]
    fn parse_doctor_rejects_bad_timeout_and_unknown_flag() {
        let bad_timeout = vec!["--timeout".to_string(), "0".to_string()];
        assert!(parse_doctor_args(&bad_timeout).is_err());

        let bad_flag = vec!["--bogus".to_string()];
        assert!(parse_doctor_args(&bad_flag).is_err());
    }
}
