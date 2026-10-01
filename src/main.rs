use clap::{Parser, Subcommand};
use mia::model::Session;
use mia::stats::{self, Config, Stats};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(version, about = "Measure a mouse's polling rate, jitter, gaps and motion")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List the mice Windows reports
    Devices,
    /// Check that USB timestamps work for a mouse (needs administrator rights, no movement needed)
    UsbCheck {
        /// Handle from `mia devices`; default: every mouse
        device: Option<String>,
    },
    /// Record a capture from a mouse (Windows only)
    Record {
        /// Seconds to record after the first report; omit to stop with Ctrl+C
        #[arg(short, long, default_value_t = 10.0)]
        seconds: f64,
        /// Mouse CPI/DPI, used only to convert counts to distance
        #[arg(long, default_value_t = 800.0)]
        cpi: f64,
        /// Mouse to test: "auto" (first to move), "any", or a handle from `mia devices`
        #[arg(long, default_value = "auto")]
        device: String,
        /// Seconds to wait for the first movement
        #[arg(long, default_value_t = 30.0)]
        wait: f64,
        /// Write the capture as CSV
        #[arg(short, long)]
        out: Option<PathBuf>,
        /// Write an HTML report
        #[arg(long)]
        html: Option<PathBuf>,
        /// Label stored in the CSV
        #[arg(long, default_value = "mia capture")]
        name: String,
        /// USB timestamps from ETW: auto (when elevated and the mouse is USB), require, or off
        #[arg(long, default_value = "auto", value_parser = ["auto", "require", "off"])]
        usb: String,
        #[command(flatten)]
        tuning: Tuning,
    },
    /// Analyze a CSV capture (this tool, MouseTester or MousePlotter)
    Analyze {
        file: PathBuf,
        /// CPI to use when the file has none
        #[arg(long, default_value_t = 800.0)]
        cpi: f64,
        #[arg(long)]
        html: Option<PathBuf>,
        #[command(flatten)]
        tuning: Tuning,
    },
}

#[derive(clap::Args)]
struct Tuning {
    /// Treat intervals longer than this as pauses instead of gaps (default: max(10 periods, 20 ms))
    #[arg(long, value_name = "MS")]
    pause_ms: Option<f64>,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), String> {
    match cli.cmd {
        Cmd::Devices => devices(),
        Cmd::UsbCheck { device } => usb_check(device.as_deref()),
        Cmd::Record {
            seconds,
            cpi,
            device,
            wait,
            out,
            html,
            name,
            usb,
            tuning,
        } => record(seconds, cpi, &device, wait, out, html, &name, &usb, tuning),
        Cmd::Analyze {
            file,
            cpi,
            html,
            tuning,
        } => {
            let text = std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
            let session = Session::from_csv(&text, cpi).map_err(|e| format!("{}: {e}", file.display()))?;
            finish(&session, html.as_deref(), tuning)
        }
    }
}

fn positive(name: &str, v: f64) -> Result<f64, String> {
    if v.is_finite() && v > 0.0 {
        Ok(v)
    } else {
        Err(format!("{name} must be a positive number"))
    }
}

fn finish(session: &Session, html: Option<&Path>, tuning: Tuning) -> Result<(), String> {
    positive("cpi", session.cpi)?;
    let st = stats::analyze(session, Config { pause_ms: tuning.pause_ms }).map_err(|e| e.to_string())?;
    print_summary(session, &st);
    if let Some(path) = html {
        std::fs::write(path, mia::report::html(session, &st)).map_err(|e| format!("{}: {e}", path.display()))?;
        println!("\nreport: {}", path.display());
    }
    Ok(())
}

fn print_summary(session: &Session, st: &Stats) {
    println!("{} ({} CPI)", if session.description.is_empty() { "capture" } else { &session.description }, session.cpi);
    println!("{} reports over {:.2} s\n", st.reports, st.duration_ms / 1000.0);
    let i = &st.interval;
    println!("{:<26}{:>10.1} Hz (median interval {:.4} ms)", "polling rate", st.median_hz, st.period_ms);
    println!("{:<26}{:>10.4} ms", "interval mean", i.mean);
    println!("{:<26}{:>10.4} ms", "interval stdev", i.stdev);
    println!("{:<26}{:>10.4} / {:.4} ms", "interval min / max", i.min, i.max);
    println!("{:<26}{:>10.4} / {:.4} ms", "99th / 99.9th percentile", i.p99, i.p999);
    println!("{:<26}{:>10.0} / {:.0} Hz", "low 1% / 0.1% rate", 1000.0 / i.p99, 1000.0 / i.p999);
    println!("{:<26}{:>10.4} ms", "jitter", st.jitter_ms);
    println!("{:<26}{:>10}", "pauses excluded", st.pauses);
    println!(
        "{:<26}{:>10} ({} delayed, {} lost, worst {:.2} ms)",
        "gaps", st.gaps.count, st.gaps.delayed, st.gaps.lost, st.gaps.worst_ms
    );
    println!("{:<26}{:>10}", "bunched reports", st.bunched);
    println!("{:<26}{:>10}", "zero-motion reports", st.motion.zero_reports);
    println!(
        "{:<26}{:>10.1} cm, straightness {:.3}",
        "path", st.motion.path_cm, st.motion.straightness
    );
    println!(
        "{:<26}{:>10.1} / {:.1} cm/s (variation {:.2})",
        "speed mean / peak", st.motion.mean_speed_cm_s, st.motion.peak_speed_cm_s, st.motion.speed_cv
    );
    println!();
    for f in stats::findings(st) {
        println!("- {f}");
    }
    println!("- {}", session.timestamp_note());
}

#[cfg(windows)]
fn devices() -> Result<(), String> {
    let list = mia::capture::list_devices();
    if list.is_empty() {
        println!("no mice reported");
    }
    for d in list {
        println!("{:#x}  {}", d.handle, d.name);
    }
    Ok(())
}

#[cfg(not(windows))]
fn devices() -> Result<(), String> {
    Err("device capture is only available on Windows".into())
}

fn parse_handle(h: &str) -> Result<isize, String> {
    isize::from_str_radix(h.trim_start_matches("0x"), 16).map_err(|_| format!("expected a hex handle from `mia devices`, got {h}"))
}

#[cfg(windows)]
fn usb_check(device: Option<&str>) -> Result<(), String> {
    let handles: Vec<(isize, String)> = match device {
        Some(h) => vec![(parse_handle(h)?, mia::capture::device_name(parse_handle(h)?))],
        None => mia::capture::list_devices().into_iter().map(|d| (d.handle, d.name)).collect(),
    };
    let mut any = false;
    for (h, name) in handles {
        match mia::capture::usb_check(h) {
            Ok(msg) => {
                any = true;
                println!("{h:#x}  ok: {msg}");
            }
            Err(e) => println!("{h:#x}  not available: {e}  ({name})"),
        }
    }
    if any { Ok(()) } else { Err("no mouse can use USB timestamps".into()) }
}

#[cfg(not(windows))]
fn usb_check(_device: Option<&str>) -> Result<(), String> {
    Err("USB timestamps are only available on Windows".into())
}

#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
fn record(
    seconds: f64,
    cpi: f64,
    device: &str,
    wait: f64,
    out: Option<PathBuf>,
    html: Option<PathBuf>,
    name: &str,
    usb: &str,
    tuning: Tuning,
) -> Result<(), String> {
    use mia::capture::{self, Event, Options, Select, Stop, UsbTimes};
    let select = match device {
        "auto" => Select::Auto,
        "any" => Select::Any,
        h => Select::Handle(parse_handle(h)?),
    };
    let opts = Options {
        select,
        usb_times: match usb {
            "require" => UsbTimes::Require,
            "off" => UsbTimes::Off,
            _ => UsbTimes::Auto,
        },
        seconds: Some(positive("seconds", seconds)?),
        wait_seconds: positive("wait", wait)?,
    };
    positive("cpi", cpi)?;
    let rec = capture::record(&opts, &mut |e| match e {
        Event::Waiting => eprintln!("Move the mouse you want to test (waiting up to {wait:.0} s)..."),
        Event::Started { device } => eprintln!("Recording {device}"),
        Event::Usb(msg) => eprintln!("{msg}"),
        Event::Progress { reports, seconds } => eprintln!("  {seconds:.0} s, {reports} reports"),
    })?;
    if let Some(s) = &rec.usb_summary {
        eprintln!("{s}");
    }
    match rec.stop {
        Stop::DeviceRemoved => eprintln!("warning: the mouse was unplugged, capture is partial"),
        Stop::SampleCap => eprintln!("warning: sample limit reached, capture is partial"),
        Stop::CtrlC | Stop::Duration => {}
    }
    let mut session = Session::new(format!("{name} ({})", rec.device), cpi, rec.samples);
    session.kernel = rec.kernel;
    if let Some(path) = &out {
        std::fs::write(path, session.to_csv()).map_err(|e| format!("{}: {e}", path.display()))?;
        eprintln!("saved {}", path.display());
    }
    finish(&session, html.as_deref(), tuning)
}

#[cfg(not(windows))]
#[allow(clippy::too_many_arguments)]
fn record(
    _seconds: f64,
    _cpi: f64,
    _device: &str,
    _wait: f64,
    _out: Option<PathBuf>,
    _html: Option<PathBuf>,
    _name: &str,
    _usb: &str,
    _tuning: Tuning,
) -> Result<(), String> {
    Err("recording is only available on Windows; use `mia analyze` on a CSV".into())
}
