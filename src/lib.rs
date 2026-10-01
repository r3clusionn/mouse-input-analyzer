//! Mouse input analysis: capture Raw Input reports on Windows, then measure polling rate,
//! interval jitter, gaps and motion from a capture, from this tool or from MouseTester or
//! MousePlotter CSV files.

#[cfg(windows)]
pub mod capture;
#[cfg(windows)]
pub mod etw;
pub mod model;
pub mod report;
pub mod stats;
