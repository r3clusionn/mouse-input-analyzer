//! Windows Raw Input capture.
//!
//! A message-only window receives `WM_INPUT` for every mouse report. Each report is stamped with
//! `QueryPerformanceCounter` at the top of the window procedure, before anything else runs.
//! To keep the stamps honest the capture:
//! - locks onto one physical device, so a second mouse cannot interleave reports;
//! - runs the thread at time-critical priority and requests a 1 ms system timer;
//! - subscribes with `RIDEV_INPUTSINK`, so reports arrive whether or not a window has focus.
//!
//! That stamp is the delivery time to this process, which includes scheduling delay. With
//! administrator rights and a USB mouse, [`crate::etw`] also traces the USB stack during the
//! recording and each report gets the time the controller received it instead.

use crate::etw;
use crate::model::{Kernel, Sample};
use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use windows_sys::Win32::Foundation::{BOOL, HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows_sys::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_NORMAL, THREAD_PRIORITY_TIME_CRITICAL,
};
use windows_sys::Win32::UI::Input::{
    GetRawInputData, GetRawInputDeviceInfoW, GetRawInputDeviceList, RegisterRawInputDevices,
    HRAWINPUT, MOUSE_MOVE_ABSOLUTE, RAWINPUT, RAWINPUTDEVICE, RAWINPUTDEVICELIST, RAWINPUTHEADER,
    RIDEV_DEVNOTIFY, RIDEV_INPUTSINK, RIDEV_REMOVE, RIDI_DEVICENAME, RID_INPUT, RIM_TYPEMOUSE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, MsgWaitForMultipleObjectsEx,
    PeekMessageW, RegisterClassW, TranslateMessage, HWND_MESSAGE, MSG, MWMO_INPUTAVAILABLE,
    PM_REMOVE, QS_ALLINPUT, WM_INPUT, WM_INPUT_DEVICE_CHANGE, WNDCLASSW,
};

const GIDC_REMOVAL: usize = 2;
const MAX_SAMPLES: usize = 20_000_000;

static STOP: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub handle: isize,
    pub name: String,
}

/// Which device to record.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Select {
    /// The first device that reports motion after the capture starts waiting.
    Auto,
    /// Exactly this Raw Input handle (see `mia devices`).
    Handle(isize),
    /// Every mouse, including synthetic input. For self-tests.
    Any,
}

/// Whether to trace the USB stack for kernel timestamps.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum UsbTimes {
    /// Use them when possible (elevated, USB mouse), otherwise fall back to Raw Input times.
    Auto,
    /// Fail if they cannot be used.
    Require,
    Off,
}

pub struct Options {
    pub select: Select,
    pub usb_times: UsbTimes,
    /// Stop this long after the first report. `None` records until Ctrl+C.
    pub seconds: Option<f64>,
    /// Give up if no motion arrives within this long.
    pub wait_seconds: f64,
}

pub enum Event<'a> {
    Waiting,
    Started { device: &'a str },
    /// A note about USB timestamps: in use, or why not.
    Usb(&'a str),
    Progress { reports: usize, seconds: f64 },
}

#[derive(Debug, PartialEq)]
pub enum Stop {
    Duration,
    CtrlC,
    DeviceRemoved,
    SampleCap,
}

struct State {
    select: Select,
    locked: Option<isize>,
    last_abs: Option<(i32, i32)>,
    raw: Vec<(i32, i32, i64)>,
    device_removed: bool,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn qpc() -> i64 {
    let mut t = 0i64;
    // Cannot fail on Windows XP and later.
    unsafe { QueryPerformanceCounter(&mut t) };
    t
}

fn qpf() -> i64 {
    let mut f = 0i64;
    unsafe { QueryPerformanceFrequency(&mut f) };
    f.max(1)
}

pub fn list_devices() -> Vec<DeviceInfo> {
    unsafe {
        let mut count = 0u32;
        let sz = std::mem::size_of::<RAWINPUTDEVICELIST>() as u32;
        if GetRawInputDeviceList(std::ptr::null_mut(), &mut count, sz) == u32::MAX || count == 0 {
            return Vec::new();
        }
        let mut list = vec![std::mem::zeroed::<RAWINPUTDEVICELIST>(); count as usize];
        let got = GetRawInputDeviceList(list.as_mut_ptr(), &mut count, sz);
        if got == u32::MAX {
            return Vec::new();
        }
        list.truncate(got as usize);
        list.iter()
            .filter(|d| d.dwType == RIM_TYPEMOUSE)
            .map(|d| DeviceInfo {
                handle: d.hDevice as isize,
                name: device_name(d.hDevice as isize),
            })
            .collect()
    }
}

/// The Raw Input interface path of a device, the name used to find it in the device tree.
pub fn device_name(handle: isize) -> String {
    if handle == 0 {
        return "synthetic input (no device)".into();
    }
    unsafe {
        let mut chars = 0u32;
        GetRawInputDeviceInfoW(handle as HANDLE, RIDI_DEVICENAME, std::ptr::null_mut(), &mut chars);
        if chars == 0 {
            return format!("device {handle:#x}");
        }
        let mut buf = vec![0u16; chars as usize];
        let n = GetRawInputDeviceInfoW(
            handle as HANDLE,
            RIDI_DEVICENAME,
            buf.as_mut_ptr() as *mut c_void,
            &mut chars,
        );
        if n == u32::MAX {
            return format!("device {handle:#x}");
        }
        let s = String::from_utf16_lossy(&buf[..n as usize]);
        s.trim_end_matches('\0').to_string()
    }
}

unsafe extern "system" fn ctrl_handler(_kind: u32) -> BOOL {
    STOP.store(true, Ordering::SeqCst);
    1
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_INPUT {
        // Stamp first: everything after this is overhead that must not leak into the interval.
        let t = qpc();
        on_input(lp as HRAWINPUT, t);
    } else if msg == WM_INPUT_DEVICE_CHANGE && wp == GIDC_REMOVAL {
        STATE.with(|s| {
            if let Some(st) = s.borrow_mut().as_mut() {
                if st.locked == Some(lp) {
                    st.device_removed = true;
                }
            }
        });
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}

unsafe fn on_input(h: HRAWINPUT, t: i64) {
    // 8-byte aligned, large enough for the header plus one mouse record.
    let mut buf = [0u64; 16];
    let mut size = std::mem::size_of_val(&buf) as u32;
    let got = GetRawInputData(
        h,
        RID_INPUT,
        buf.as_mut_ptr() as *mut c_void,
        &mut size,
        std::mem::size_of::<RAWINPUTHEADER>() as u32,
    );
    if got == u32::MAX {
        return;
    }
    let ri = &*(buf.as_ptr() as *const RAWINPUT);
    if ri.header.dwType != RIM_TYPEMOUSE {
        return;
    }
    let device = ri.header.hDevice as isize;
    let m = ri.data.mouse;
    let (mut dx, mut dy) = (m.lLastX, m.lLastY);
    STATE.with(|s| {
        let mut guard = s.borrow_mut();
        let Some(st) = guard.as_mut() else { return };
        // Absolute devices (tablets, remote sessions) report positions; convert to deltas.
        if m.usFlags & MOUSE_MOVE_ABSOLUTE != 0 {
            let prev = st.last_abs.replace((dx, dy));
            (dx, dy) = prev.map_or((0, 0), |(px, py)| (dx - px, dy - py));
        }
        match (st.locked, st.select) {
            (Some(d), _) if d != device => return,
            (Some(_), _) => {}
            (None, Select::Handle(h)) if h != device => return,
            (None, Select::Auto) if dx == 0 && dy == 0 => return,
            (None, _) => st.locked = Some(device),
        }
        if st.raw.len() < MAX_SAMPLES {
            st.raw.push((dx, dy, t));
        }
    });
}

struct Window(HWND);

impl Window {
    fn create() -> Result<Window, String> {
        unsafe {
            let hinst = GetModuleHandleW(std::ptr::null());
            let class: Vec<u16> = "mia_capture\0".encode_utf16().collect();
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: hinst,
                lpszClassName: class.as_ptr(),
                ..std::mem::zeroed()
            };
            RegisterClassW(&wc); // Fails harmlessly if the class already exists.
            let hwnd = CreateWindowExW(
                0,
                class.as_ptr(),
                class.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                std::ptr::null_mut(),
                hinst,
                std::ptr::null(),
            );
            if hwnd.is_null() {
                return Err("could not create the input window".into());
            }
            let rid = RAWINPUTDEVICE {
                usUsagePage: 1,
                usUsage: 2,
                dwFlags: RIDEV_INPUTSINK | RIDEV_DEVNOTIFY,
                hwndTarget: hwnd,
            };
            if RegisterRawInputDevices(&rid, 1, std::mem::size_of::<RAWINPUTDEVICE>() as u32) == 0 {
                DestroyWindow(hwnd);
                return Err("RegisterRawInputDevices failed".into());
            }
            Ok(Window(hwnd))
        }
    }

    fn pump(&self) {
        unsafe {
            let mut msg: MSG = std::mem::zeroed();
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        unsafe {
            let rid = RAWINPUTDEVICE {
                usUsagePage: 1,
                usUsage: 2,
                dwFlags: RIDEV_REMOVE,
                hwndTarget: std::ptr::null_mut(),
            };
            RegisterRawInputDevices(&rid, 1, std::mem::size_of::<RAWINPUTDEVICE>() as u32);
            DestroyWindow(self.0);
        }
    }
}

/// Restores thread priority and the system timer however `record` exits.
struct Tuning;

impl Tuning {
    fn begin() -> Tuning {
        unsafe {
            SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
            timeBeginPeriod(1);
        }
        Tuning
    }
}

impl Drop for Tuning {
    fn drop(&mut self) {
        unsafe {
            timeEndPeriod(1);
            SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_NORMAL);
        }
    }
}

pub struct Recording {
    /// Times rebased to the first report, from the kernel when `kernel` is set.
    pub samples: Vec<Sample>,
    pub kernel: Option<Kernel>,
    pub device: String,
    pub stop: Stop,
    /// Summary of the USB pairing, when it was used.
    pub usb_summary: Option<String>,
}

/// Starts the USB trace for `device` and waits until the hub has described it, pumping messages
/// so Raw Input does not back up meanwhile.
fn start_usb(device: &str, window: &Window) -> Result<etw::Session, String> {
    let session = etw::Session::start(device)?;
    let t0 = qpc();
    let freq = qpf();
    while !session.device_found() {
        if (qpc() - t0) as f64 / freq as f64 > 3.0 {
            return Err("the USB hub did not describe this mouse within 3 s".into());
        }
        window.pump();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    // The pipe events arrive alongside; give the consumer a moment to drain its buffers.
    let settle = qpc();
    while ((qpc() - settle) as f64 / freq as f64) < 0.3 {
        window.pump();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    Ok(session)
}

/// Records until the duration elapses, Ctrl+C, the device is unplugged or the cap is hit.
pub fn record(opts: &Options, on_event: &mut dyn FnMut(Event)) -> Result<Recording, String> {
    STOP.store(false, Ordering::SeqCst);
    unsafe { SetConsoleCtrlHandler(Some(ctrl_handler), 1) };
    STATE.with(|s| {
        *s.borrow_mut() = Some(State {
            select: opts.select,
            locked: None,
            last_abs: None,
            raw: Vec::new(),
            device_removed: false,
        })
    });
    let _tuning = Tuning::begin();
    let window = Window::create()?;
    on_event(Event::Waiting);

    let freq = qpf();
    let wait_start = qpc();
    let mut started_at: Option<i64> = None;
    let mut device_label = String::new();
    let mut usb: Option<etw::Session> = None;
    let mut last_progress = qpc();
    let stop;
    loop {
        window.pump();
        let now = qpc();
        let (n, locked, removed) = STATE.with(|s| {
            let g = s.borrow();
            let st = g.as_ref().expect("state set above");
            (st.raw.len(), st.locked, st.device_removed)
        });
        if let (None, Some(dev)) = (started_at, locked) {
            device_label = device_name(dev);
            if opts.usb_times != UsbTimes::Off && dev != 0 {
                match start_usb(&device_label, &window) {
                    Ok(s) => {
                        usb = Some(s);
                        // Reports from before the trace was ready cannot be paired: start over.
                        STATE.with(|s| s.borrow_mut().as_mut().expect("state set above").raw.clear());
                        on_event(Event::Usb("USB timestamps on (ETW trace of the USB stack)"));
                    }
                    Err(e) if opts.usb_times == UsbTimes::Require => return Err(format!("USB timestamps: {e}")),
                    Err(e) => on_event(Event::Usb(&format!("USB timestamps off: {e}; using Raw Input times"))),
                }
            } else if opts.usb_times == UsbTimes::Require {
                return Err("USB timestamps need a physical device; --device any records synthetic input".into());
            }
            started_at = Some(qpc());
            on_event(Event::Started { device: &device_label });
        }
        if STOP.load(Ordering::SeqCst) {
            stop = Stop::CtrlC;
            break;
        }
        if removed {
            stop = Stop::DeviceRemoved;
            break;
        }
        if n >= MAX_SAMPLES {
            stop = Stop::SampleCap;
            break;
        }
        match started_at {
            None => {
                if (now - wait_start) as f64 / freq as f64 > opts.wait_seconds {
                    return Err("no mouse motion detected while waiting".into());
                }
            }
            Some(t0) => {
                let elapsed = (now - t0) as f64 / freq as f64;
                if opts.seconds.is_some_and(|d| elapsed >= d) {
                    stop = Stop::Duration;
                    break;
                }
                if (now - last_progress) as f64 / freq as f64 >= 1.0 {
                    last_progress = now;
                    on_event(Event::Progress { reports: n, seconds: elapsed });
                }
            }
        }
        unsafe {
            MsgWaitForMultipleObjectsEx(0, std::ptr::null(), 10, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
        }
    }
    window.pump();
    drop(window);
    unsafe { SetConsoleCtrlHandler(Some(ctrl_handler), 0) };

    let raw = STATE.with(|s| s.borrow_mut().take().map(|st| st.raw).unwrap_or_default());
    if raw.is_empty() {
        return Err("no reports were captured".into());
    }
    let user: Vec<i64> = raw.iter().map(|r| r.2).collect();
    let ms = |t: i64, t0: i64| (t - t0) as f64 * 1000.0 / freq as f64;

    let mut usb_summary = None;
    let mut kernel_times = None;
    if let Some(session) = usb {
        match session.stop() {
            Ok(cap) if cap.lost > 0 => usb_summary = Some(format!("USB trace lost {} events, using Raw Input times", cap.lost)),
            Ok(cap) => {
                let p = etw::pair(&cap.completions, &user, freq);
                usb_summary = Some(format!(
                    "USB endpoint {:#04x}: {} transfers traced; reports timed at interrupt {}, completion {}, Raw Input {}; {} reports shared an interrupt, {} transfers unpaired; Raw Input lagged by {:.0} us on average (spread {:.0} us)",
                    cap.endpoint,
                    cap.completions.len(),
                    p.interrupts,
                    p.completions,
                    p.raw,
                    p.shared,
                    p.unmatched,
                    p.lag_us,
                    p.lag_spread_us
                ));
                if p.raw < user.len() {
                    kernel_times = Some(p.times);
                }
            }
            Err(e) => usb_summary = Some(format!("USB timestamps unavailable: {e}; using Raw Input times")),
        }
    }

    let (samples, kernel) = match kernel_times {
        None => {
            let t0 = user[0];
            (raw.iter().map(|&(dx, dy, t)| Sample { dx, dy, t_ms: ms(t, t0) }).collect(), None)
        }
        Some(times) => {
            // One origin for both clocks keeps the delivery lag visible in the CSV.
            let t0 = times[0].0.min(user[0]);
            let samples = raw.iter().zip(&times).map(|(&(dx, dy, _), &(t, _))| Sample { dx, dy, t_ms: ms(t, t0) }).collect();
            let kernel = Kernel {
                user_ms: user.iter().map(|&t| ms(t, t0)).collect(),
                source: times.iter().map(|(_, s)| s.map_or("raw", |s| s.name()).to_string()).collect(),
            };
            (samples, Some(kernel))
        }
    };
    Ok(Recording { samples, kernel, device: device_label, stop, usb_summary })
}

/// Starts a USB trace for one device, reports what it found and stops. Needs no mouse movement.
pub fn usb_check(handle: isize) -> Result<String, String> {
    let path = device_name(handle);
    let identity = etw::identify(&path)?;
    let window = Window::create()?;
    let session = start_usb(&path, &window)?;
    let cap = session.stop()?;
    Ok(format!(
        "USB device object {}{}: interrupt IN endpoint {:#04x}, pipe found, {} transfers during the check, {} events lost",
        identity.pdo,
        identity.interface.map_or(String::new(), |i| format!(" interface {i}")),
        cap.endpoint,
        cap.completions.len(),
        cap.lost
    ))
}
