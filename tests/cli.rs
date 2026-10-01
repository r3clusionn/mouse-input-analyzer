//! Runs the real binary against a capture recorded by `mia record` from a 125 Hz synthetic
//! signal (examples/inject.rs). The signal is steady, so the report must show 125 Hz and no gaps.

use std::process::Command;

fn mia(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_mia")).args(args).output().unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn fixture() -> String {
    format!("{}/tests/injected_125hz.csv", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn analyze_reports_125hz_and_writes_html() {
    let html = std::env::temp_dir().join("mia_cli_test.html");
    let (ok, out, err) = mia(&["analyze", &fixture(), "--html", html.to_str().unwrap()]);
    assert!(ok, "{err}");
    assert!(out.contains("looks like 125 Hz"), "{out}");
    assert!(out.contains("gaps"), "{out}");
    let page = std::fs::read_to_string(&html).unwrap();
    assert_eq!(page.matches("<svg").count(), 5);
    let _ = std::fs::remove_file(html);
}

#[test]
fn analyze_missing_file_fails_cleanly() {
    let (ok, _, err) = mia(&["analyze", "does-not-exist.csv"]);
    assert!(!ok);
    assert!(err.starts_with("error:"), "{err}");
}

#[test]
fn analyze_rejects_bad_csv_with_a_line_number() {
    let path = std::env::temp_dir().join("mia_bad.csv");
    std::fs::write(&path, "d\n800\nh\n1,1,0\noops\n").unwrap();
    let (ok, _, err) = mia(&["analyze", path.to_str().unwrap()]);
    let _ = std::fs::remove_file(path);
    assert!(!ok);
    assert!(err.contains("line 5"), "{err}");
}

#[test]
fn cpi_flag_applies_only_when_the_file_has_none_and_must_be_positive() {
    let path = std::env::temp_dir().join("mia_nocpi.csv");
    let rows: String = (0..50).map(|i| format!("1,0,{i}.0\n")).collect();
    std::fs::write(&path, format!("xCount,yCount,Time (ms)\n{rows}")).unwrap();
    let p = path.to_str().unwrap();
    let (ok, out, _) = mia(&["analyze", p, "--cpi", "1600"]);
    assert!(ok && out.contains("1600 CPI"), "{out}");
    let (ok, _, err) = mia(&["analyze", p, "--cpi", "0"]);
    let _ = std::fs::remove_file(path);
    assert!(!ok && err.contains("cpi"), "{err}");
}

#[cfg(windows)]
#[test]
fn devices_runs() {
    let (ok, _, err) = mia(&["devices"]);
    assert!(ok, "{err}");
}
