//! Synthetic mouse signal for self-testing the recorder without hardware.
//!
//! Sends one relative move per period via SendInput, alternating +1 and -1 counts so the cursor
//! does not drift. Optionally drops every Nth report to create known gaps.
//!
//!   cargo run --release --example inject -- --hz 500 --seconds 5 [--drop-every 100]
//!   mia record --device any --seconds 4
//!
//! SendInput reports carry no device handle, which is why the recorder has `--device any`.

#[cfg(windows)]
fn main() {
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_MOVE, MOUSEINPUT,
    };

    let args: Vec<String> = std::env::args().collect();
    let get = |flag: &str, default: f64| {
        args.windows(2)
            .find(|w| w[0] == flag)
            .and_then(|w| w[1].parse().ok())
            .unwrap_or(default)
    };
    let hz = get("--hz", 500.0);
    let seconds = get("--seconds", 5.0);
    let drop_every = get("--drop-every", 0.0) as u64;
    let start_delay = get("--delay", 1.0);

    let mut freq = 0i64;
    unsafe { QueryPerformanceFrequency(&mut freq) };
    let now = || {
        let mut t = 0i64;
        unsafe { QueryPerformanceCounter(&mut t) };
        t
    };
    let secs = |t: i64| t as f64 / freq as f64;

    std::thread::sleep(std::time::Duration::from_secs_f64(start_delay));
    let t0 = now();
    let period = 1.0 / hz;
    let mut i = 0u64;
    let mut worst_late = 0.0f64;
    let mut late_sum = 0.0f64;
    let mut sent_n = 0u64;
    let mut worst_call = 0.0f64;
    loop {
        let due = i as f64 * period;
        if due > seconds {
            break;
        }
        // Sleep most of the wait, spin the rest, for sub-millisecond accuracy.
        loop {
            let left = due - secs(now() - t0);
            if left <= 0.0 {
                break;
            }
            if left > 0.002 {
                std::thread::sleep(std::time::Duration::from_micros(500));
            } else {
                std::hint::spin_loop();
            }
        }
        let late = secs(now() - t0) - due;
        worst_late = worst_late.max(late);
        late_sum += late;
        i += 1;
        if drop_every != 0 && i.is_multiple_of(drop_every) {
            continue;
        }
        let dx = if i.is_multiple_of(2) { 1 } else { -1 };
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT { dx, dy: 0, mouseData: 0, dwFlags: MOUSEEVENTF_MOVE, time: 0, dwExtraInfo: 0 },
            },
        };
        sent_n += 1;
        let before = now();
        let sent = unsafe { SendInput(1, &input, std::mem::size_of::<INPUT>() as i32) };
        worst_call = worst_call.max(secs(now() - before));
        if sent != 1 {
            eprintln!("SendInput failed: {}", unsafe { GetLastError() });
            std::process::exit(1);
        }
    }
    eprintln!(
        "sent {sent_n} moves at {hz} Hz, send lateness mean {:.3} ms, worst {:.3} ms, slowest SendInput {:.3} ms",
        late_sum / i.max(1) as f64 * 1000.0,
        worst_late * 1000.0,
        worst_call * 1000.0
    );
}

#[cfg(not(windows))]
fn main() {
    eprintln!("Windows only");
}
