//! `labstream calibrate`: measure fixed device-to-LSL delay against a
//! reference trigger or marker stream (`docs/roadmap.md`, step 15).

use labstream_net::calibrate::{calibrate_streams, CalibrationOptions};
use labstream_net::{build_property_query, resolve};
use std::process::ExitCode;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct CalibrateArgs {
    pub device: String,
    pub reference: String,
    pub session: Option<String>,
    pub opts: CalibrationOptions,
    pub json: bool,
    pub write_xml: Option<String>,
}

pub fn calibrate_usage() -> &'static str {
    "Usage:
  labstream calibrate --device NAME --reference NAME [OPTIONS]

Options:
  --device NAME       Name of the numeric LSL device stream to calibrate.
  --reference NAME    Name of the reference marker or trigger stream.
  --channel N         Zero-based device channel index (default: 0).
  --pulses N          Number of pulses to match (default: 20).
  --threshold VAL     Fixed rising-edge threshold (default: auto midpoint).
  --min-delay SEC     Minimum delay window in seconds (default: -0.05).
  --max-delay SEC     Maximum delay window in seconds (default: 0.50).
  --timeout SEC       Maximum wait time in seconds (default: 30).
  --session ID        Resolve streams in session ID instead of the default.
  --write-xml PATH    Write the <synchronization> XML snippet to PATH.
  --json              Emit the result as JSON.
  -h, --help          Show this help text."
}

pub fn parse_calibrate_args(args: &[String]) -> Result<(CalibrateArgs, bool), String> {
    let mut device = String::new();
    let mut reference = String::new();
    let mut session = None;
    let mut opts = CalibrationOptions::default();
    let mut json = false;
    let mut write_xml = None;

    let mut i = 0;
    while i < args.len() {
        let cur = args[i].as_str();
        let next_val = |idx: &mut usize, flag: &str| -> Result<String, String> {
            *idx += 1;
            args.get(*idx)
                .cloned()
                .ok_or_else(|| format!("missing value after {flag}"))
        };
        match cur {
            "-h" | "--help" => {
                return Ok((
                    CalibrateArgs {
                        device,
                        reference,
                        session,
                        opts,
                        json,
                        write_xml,
                    },
                    true,
                ))
            }
            "--json" => json = true,
            "--device" => device = next_val(&mut i, "--device")?,
            "--reference" => reference = next_val(&mut i, "--reference")?,
            "--session" => session = Some(next_val(&mut i, "--session")?),
            "--write-xml" => write_xml = Some(next_val(&mut i, "--write-xml")?),
            "--channel" => {
                let v = next_val(&mut i, "--channel")?;
                opts.channel = v
                    .parse()
                    .map_err(|_| format!("invalid --channel value: {v}"))?;
            }
            "--pulses" => {
                let v = next_val(&mut i, "--pulses")?;
                let n: usize = v
                    .parse()
                    .map_err(|_| format!("invalid --pulses value: {v}"))?;
                if n == 0 {
                    return Err("--pulses must be at least 1".to_string());
                }
                opts.target_pulses = n;
            }
            "--threshold" => {
                let v = next_val(&mut i, "--threshold")?;
                let f: f64 = v
                    .parse()
                    .map_err(|_| format!("invalid --threshold value: {v}"))?;
                if !f.is_finite() {
                    return Err("--threshold must be finite".to_string());
                }
                opts.threshold = Some(f);
            }
            "--min-delay" => {
                let v = next_val(&mut i, "--min-delay")?;
                opts.min_delay_s = v
                    .parse()
                    .map_err(|_| format!("invalid --min-delay value: {v}"))?;
            }
            "--max-delay" => {
                let v = next_val(&mut i, "--max-delay")?;
                opts.max_delay_s = v
                    .parse()
                    .map_err(|_| format!("invalid --max-delay value: {v}"))?;
            }
            "--timeout" => {
                let v = next_val(&mut i, "--timeout")?;
                let secs: f64 = v
                    .parse()
                    .map_err(|_| format!("invalid --timeout value: {v}"))?;
                if !secs.is_finite() || secs <= 0.0 {
                    return Err("--timeout must be positive".to_string());
                }
                opts.timeout = Duration::from_secs_f64(secs);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
        i += 1;
    }

    if device.is_empty() || reference.is_empty() {
        return Err("both --device and --reference are required".to_string());
    }
    Ok((
        CalibrateArgs {
            device,
            reference,
            session,
            opts,
            json,
            write_xml,
        },
        false,
    ))
}

pub fn run_calibrate(args: &[String]) -> ExitCode {
    let (cfg, help) = match parse_calibrate_args(args) {
        Ok(x) => x,
        Err(err) => {
            eprintln!("error: {err}\n\n{}", calibrate_usage());
            return ExitCode::from(2);
        }
    };
    if help {
        println!("{}", calibrate_usage());
        return ExitCode::SUCCESS;
    }

    let session_id = cfg
        .session
        .unwrap_or_else(|| labstream_net::config::get().session_id.clone());
    let dev_q = build_property_query(&session_id, "name", &cfg.device);
    let ref_q = build_property_query(&session_id, "name", &cfg.reference);

    let dev_found = match resolve(&dev_q, 1, Duration::from_secs(5)) {
        Ok(list) if !list.is_empty() => list[0].clone(),
        Ok(_) => {
            eprintln!("error: device stream '{}' not found", cfg.device);
            return ExitCode::from(1);
        }
        Err(e) => {
            eprintln!("error: failed to resolve device stream: {e}");
            return ExitCode::from(1);
        }
    };
    let ref_found = match resolve(&ref_q, 1, Duration::from_secs(5)) {
        Ok(list) if !list.is_empty() => list[0].clone(),
        Ok(_) => {
            eprintln!("error: reference stream '{}' not found", cfg.reference);
            return ExitCode::from(1);
        }
        Err(e) => {
            eprintln!("error: failed to resolve reference stream: {e}");
            return ExitCode::from(1);
        }
    };

    let res = match calibrate_streams(&dev_found, &ref_found, &cfg.opts) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: calibration failed: {e}");
            return ExitCode::from(1);
        }
    };

    if let Some(path) = cfg.write_xml {
        if let Err(e) = std::fs::write(&path, res.to_xml_snippet()) {
            eprintln!("error: failed to write {path}: {e}");
            return ExitCode::from(1);
        }
    }

    if cfg.json {
        println!("{}", res.to_json());
    } else {
        println!(
            "Device delay ({} ch {} vs {}):",
            cfg.device, cfg.opts.channel, cfg.reference
        );
        println!("  mean:    {:.3} ms", res.delay_mean_s * 1000.0);
        println!("  median:  {:.3} ms", res.delay_median_s * 1000.0);
        println!("  rms:     {:.3} ms", res.delay_rms_s * 1000.0);
        println!(
            "  p5..p95: {:.3} .. {:.3} ms",
            res.delay_p5_s * 1000.0,
            res.delay_p95_s * 1000.0
        );
        println!(
            "  pulses:  {} matched ({} ref, {} edges, threshold {:.4})",
            res.matched_pulses, res.reference_events, res.device_edges, res.threshold_used
        );
        println!("\nXDF <desc> snippet:\n{}", res.to_xml_snippet());
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_calibrate_accepts_flags_and_rejects_missing_streams() {
        let args: Vec<String> = [
            "--device",
            "MyEEG",
            "--reference",
            "Photodiode",
            "--channel",
            "2",
            "--pulses",
            "15",
            "--threshold",
            "0.5",
            "--json",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let (parsed, help) = parse_calibrate_args(&args).unwrap();
        assert!(!help);
        assert_eq!(parsed.device, "MyEEG");
        assert_eq!(parsed.reference, "Photodiode");
        assert_eq!(parsed.opts.channel, 2);
        assert_eq!(parsed.opts.target_pulses, 15);
        assert_eq!(parsed.opts.threshold, Some(0.5));
        assert!(parsed.json);

        let missing = vec!["--device".to_string(), "OnlyOne".to_string()];
        assert!(parse_calibrate_args(&missing).is_err());
    }
}
