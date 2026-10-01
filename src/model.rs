//! Capture data model and the CSV format.
//!
//! The CSV layout is the one MouseTester wrote first and MousePlotter also writes: a description
//! line, a CPI line, a header line, then one row per report with `xCount,yCount,Time (ms)` as the
//! first three columns. Extra columns (button flags, kernel timestamps) are ignored on load, so
//! files from either tool can be analyzed here. A capture with kernel (ETW) timestamps is written
//! in MousePlotter's paired layout, `xCount,yCount,eventTime (ms),userTime (ms),timestampSource`,
//! and read back with both clocks.

use std::fmt;

/// One Raw Input mouse report.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    pub dx: i32,
    pub dy: i32,
    /// Milliseconds since the first report of the capture.
    pub t_ms: f64,
}

/// Kernel timing for a capture: `Sample::t_ms` is then the kernel time, and this keeps the
/// user-mode (Raw Input) time and where each report's time came from.
#[derive(Clone, Debug, PartialEq)]
pub struct Kernel {
    /// Raw Input delivery time per sample, same origin as `t_ms`.
    pub user_ms: Vec<f64>,
    /// `interrupt`, `completion` or `raw` per sample.
    pub source: Vec<String>,
}

impl Kernel {
    pub fn count(&self, source: &str) -> usize {
        self.source.iter().filter(|s| *s == source).count()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Session {
    pub description: String,
    /// Counts per inch the mouse was set to. Only used to convert counts to distance.
    pub cpi: f64,
    pub samples: Vec<Sample>,
    pub kernel: Option<Kernel>,
}

#[derive(Debug, PartialEq)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

fn parse_row(line: &str) -> Option<Sample> {
    let mut it = line.split(',');
    let dx = it.next()?.trim().parse().ok()?;
    let dy = it.next()?.trim().parse().ok()?;
    let t_ms: f64 = it.next()?.trim().parse().ok()?;
    t_ms.is_finite().then_some(Sample { dx, dy, t_ms })
}

impl Session {
    pub fn new(description: impl Into<String>, cpi: f64, samples: Vec<Sample>) -> Self {
        Session {
            description: description.into(),
            cpi,
            samples,
            kernel: None,
        }
    }

    /// One sentence on which clock the times come from.
    pub fn timestamp_note(&self) -> String {
        match &self.kernel {
            None => "Timestamps are taken when Windows delivers each report to this program, so scheduling delay is included.".into(),
            Some(k) => {
                let n = self.samples.len().max(1) as f64;
                format!(
                    "Timestamps come from the USB stack (ETW): {:.1}% at the controller interrupt, {:.1}% at the USB transfer completion, {:.1}% at Raw Input delivery (could not be paired).",
                    k.count("interrupt") as f64 * 100.0 / n,
                    k.count("completion") as f64 * 100.0 / n,
                    k.count("raw") as f64 * 100.0 / n
                )
            }
        }
    }

    pub fn to_csv(&self) -> String {
        let mut out = String::with_capacity(self.samples.len() * 48 + 128);
        out.push_str(&self.description.replace(['\r', '\n'], " "));
        match &self.kernel {
            None => {
                out.push_str(&format!("\n{}\nxCount,yCount,Time (ms)\n", self.cpi));
                for s in &self.samples {
                    out.push_str(&format!("{},{},{:.6}\n", s.dx, s.dy, s.t_ms));
                }
            }
            Some(k) => {
                out.push_str(&format!("\n{}\nxCount,yCount,eventTime (ms),userTime (ms),timestampSource\n", self.cpi));
                for (i, s) in self.samples.iter().enumerate() {
                    out.push_str(&format!("{},{},{:.6},{:.6},{}\n", s.dx, s.dy, s.t_ms, k.user_ms[i], k.source[i]));
                }
            }
        }
        out
    }

    /// Parses a capture. `default_cpi` is used when the file carries no CPI line.
    pub fn from_csv(text: &str, default_cpi: f64) -> Result<Session, ParseError> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut pre: Vec<&str> = Vec::new();
        let mut samples = Vec::new();
        let (mut user_ms, mut source): (Vec<f64>, Vec<String>) = (Vec::new(), Vec::new());
        for (i, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }
            match parse_row(line) {
                Some(s) => {
                    if let Some(prev) = samples.last() {
                        let prev: &Sample = prev;
                        if s.t_ms < prev.t_ms {
                            return Err(ParseError {
                                line: i + 1,
                                message: format!(
                                    "time goes backwards ({} after {})",
                                    s.t_ms, prev.t_ms
                                ),
                            });
                        }
                    }
                    samples.push(s);
                    // Paired files carry the user-mode time and source after the kernel time.
                    let mut rest = line.split(',').skip(3);
                    if let (Some(u), Some(src)) = (rest.next().and_then(|v| v.trim().parse::<f64>().ok()), rest.next()) {
                        user_ms.push(u);
                        source.push(src.trim().to_string());
                    }
                }
                // Anything before the first row is header text: description, CPI, column names.
                None if samples.is_empty() && pre.len() < 4 => pre.push(line),
                None => {
                    return Err(ParseError {
                        line: i + 1,
                        message: format!("expected `x,y,time`, got `{line}`"),
                    })
                }
            }
        }
        if samples.is_empty() {
            return Err(ParseError {
                line: 0,
                message: "no samples found".into(),
            });
        }
        // `pre` is [description, cpi, header], [cpi, header], [header] or [description, header].
        let cpi_line = pre.iter().find_map(|l| l.parse::<f64>().ok());
        let description = match pre.first() {
            Some(first) if pre.len() >= 2 && first.parse::<f64>().is_err() => first.to_string(),
            _ => String::new(),
        };
        let cpi = cpi_line.filter(|c| c.is_finite() && *c > 0.0).unwrap_or(default_cpi);
        let base = samples[0].t_ms;
        for s in &mut samples {
            s.t_ms -= base;
        }
        // Only a complete set of paired columns counts; a partial one is ignored.
        let kernel = (user_ms.len() == samples.len()).then(|| Kernel { user_ms: user_ms.iter().map(|u| u - base).collect(), source });
        Ok(Session {
            description,
            cpi,
            samples,
            kernel,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(dx: i32, dy: i32, t_ms: f64) -> Sample {
        Sample { dx, dy, t_ms }
    }

    #[test]
    fn round_trip() {
        let a = Session::new("test mouse", 1600.0, vec![s(1, -2, 0.0), s(3, 0, 1.000125)]);
        let b = Session::from_csv(&a.to_csv(), 800.0).unwrap();
        assert_eq!(b.description, "test mouse");
        assert_eq!(b.cpi, 1600.0);
        assert_eq!(b.samples.len(), 2);
        assert!((b.samples[1].t_ms - 1.000125).abs() < 1e-9);
    }

    #[test]
    fn reads_mousetester_file_with_button_column_and_bom() {
        let csv = "\u{feff}MouseTester\r\n400\r\nxCount,yCount,Time (ms),buttonflags\r\n1,0,10.5,0\r\n2,1,11.5,1\r\n";
        let a = Session::from_csv(csv, 800.0).unwrap();
        assert_eq!(a.cpi, 400.0);
        assert_eq!(a.samples.len(), 2);
        assert_eq!(a.samples[0].t_ms, 0.0, "time is rebased to the first report");
        assert_eq!(a.samples[1].t_ms, 1.0);
    }

    #[test]
    fn reads_mouseplotter_kernel_time_file() {
        let csv = "MousePlotter Windows logger (ETW xHCI interrupt)\n800\nxCount,yCount,eventTime (ms),userTime (ms),timestampSource\n1,0,0.000000,0.2,interrupt\n1,0,1.000000,1.3,completion\n";
        let a = Session::from_csv(csv, 400.0).unwrap();
        assert_eq!(a.samples.len(), 2);
        assert_eq!(a.description, "MousePlotter Windows logger (ETW xHCI interrupt)");
    }

    #[test]
    fn paired_kernel_columns_round_trip() {
        let mut a = Session::new("etw", 800.0, vec![s(1, 0, 0.0), s(1, 0, 1.0), s(1, 0, 2.0)]);
        a.kernel = Some(Kernel { user_ms: vec![0.3, 1.25, 2.4], source: vec!["interrupt".into(), "completion".into(), "raw".into()] });
        let b = Session::from_csv(&a.to_csv(), 400.0).unwrap();
        let k = b.kernel.unwrap();
        assert_eq!(k.source, ["interrupt", "completion", "raw"]);
        assert!((k.user_ms[1] - 1.25).abs() < 1e-9);
        assert_eq!(k.count("raw"), 1);
        assert!(Session::from_csv("d\n800\nh\n1,1,0\n1,1,1\n", 800.0).unwrap().kernel.is_none());
    }

    #[test]
    fn header_only_file_uses_default_cpi() {
        let a = Session::from_csv("xCount,yCount,Time (ms)\n1,1,0\n1,1,1\n", 1234.0).unwrap();
        assert_eq!(a.cpi, 1234.0);
        assert_eq!(a.samples.len(), 2);
    }

    #[test]
    fn rejects_garbage_and_backwards_time() {
        let bad = Session::from_csv("d\n800\nh\n1,1,0\nnope\n", 800.0).unwrap_err();
        assert_eq!(bad.line, 5);
        let back = Session::from_csv("d\n800\nh\n1,1,5\n1,1,4\n", 800.0).unwrap_err();
        assert!(back.message.contains("backwards"));
        assert!(Session::from_csv("d\n800\nh\n", 800.0).is_err());
    }
}
