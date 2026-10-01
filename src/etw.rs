//! Kernel timestamps for mouse reports from Event Tracing for Windows (needs administrator rights).
//!
//! Raw Input stamps a report when this process receives it, which includes scheduling delay. The
//! USB stack can tell us much earlier when the same report arrived:
//!
//! - **USBHUB3** describes every USB device at session start (event 6): its device handle, its
//!   configuration descriptor and its physical device object name. The PDO name ties it to the
//!   Raw Input device we are recording, and the descriptor gives the HID interrupt IN endpoint.
//! - **UCX** describes the pipes (event 6) and reports every completed transfer (event 27). A
//!   completed interrupt IN transfer on the mouse's pipe is one mouse report.
//! - **USBXHCI** marks the controller's interrupt service routine (event 42) and its DPC (43, 44).
//!   A completion inside a DPC on the same CPU was delivered by that interrupt, whose timestamp is
//!   earlier still.
//!
//! Completions are then paired with Raw Input reports in order (see [`pair`]). Reports that cannot
//! be paired safely keep their Raw Input time, so the result is never worse than without ETW.
//!
//! The provider IDs, event IDs, payload layouts and pairing method follow MousePlotter's Windows
//! logger (XBAB Tech, MIT licence; see THIRD_PARTY_NOTICES). This is a Rust implementation of that
//! method, not a copy of its source.

use std::sync::Mutex;
use std::thread::JoinHandle;
use windows_sys::core::GUID;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_DevNode_Registry_PropertyW, CM_Get_Device_IDW, CM_Get_Device_Interface_PropertyW, CM_Get_Parent, CM_Locate_DevNodeW,
    CM_DRP_PHYSICAL_DEVICE_OBJECT_NAME, CM_LOCATE_DEVNODE_NORMAL, CR_SUCCESS, MAX_DEVICE_ID_LEN,
};
use windows_sys::Win32::Devices::Properties::{DEVPKEY_Device_InstanceId, DEVPROP_TYPE_STRING};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0};
use windows_sys::Win32::System::Diagnostics::Etw::{
    CloseTrace, ControlTraceW, EnableTraceEx2, OpenTraceW, ProcessTrace, StartTraceW, CONTROLTRACE_HANDLE, ENABLE_TRACE_PARAMETERS,
    EVENT_FILTER_DESCRIPTOR, EVENT_FILTER_TYPE_EVENT_ID, EVENT_HEADER_FLAG_32_BIT_HEADER, EVENT_HEADER_FLAG_PROCESSOR_INDEX, EVENT_RECORD,
    EVENT_TRACE_LOGFILEW, EVENT_TRACE_PROPERTIES, EVENT_TRACE_REAL_TIME_MODE, PROCESSTRACE_HANDLE, PROCESS_TRACE_MODE_EVENT_RECORD,
    PROCESS_TRACE_MODE_RAW_TIMESTAMP, PROCESS_TRACE_MODE_REAL_TIME, WNODE_FLAG_TRACED_GUID,
};
use windows_sys::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};

const UCX: u128 = 0x36da592d_e43a_4e28_af6f_4bc57c5a11e8;
const XHCI: u128 = 0x30e1d284_5d88_459c_83fd_6345b39b19ec;
const HUB: u128 = 0xac52ad17_cc01_4f85_8df5_4dce4333c99b;
const SESSION_NAME: &str = "MiaUsbTimestamps";
const MAX_CPUS: usize = 4096;
const MAX_EVENTS: usize = 50_000_000;

fn guid(u: u128) -> GUID {
    GUID::from_u128(u)
}

fn guid_eq(g: &GUID, u: u128) -> bool {
    let o = guid(u);
    g.data1 == o.data1 && g.data2 == o.data2 && g.data3 == o.data3 && g.data4 == o.data4
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn from_wide(b: &[u16]) -> String {
    let end = b.iter().position(|c| *c == 0).unwrap_or(b.len());
    String::from_utf16_lossy(&b[..end])
}

/// The USB identity of a Raw Input mouse.
#[derive(Clone, Debug, PartialEq)]
pub struct Identity {
    /// Physical device object name of the USB device, for example `\Device\USBPDO-7`.
    pub pdo: String,
    /// HID interface number on a composite device, or `None` when the device is the interface.
    pub interface: Option<u8>,
}

/// Walks from the Raw Input device interface up the device tree to the USB device node.
/// Fails for mice that are not USB (Bluetooth, PS/2, virtual), which ETW cannot time.
pub fn identify(device_path: &str) -> Result<Identity, String> {
    unsafe {
        let path = wide(device_path);
        let mut id = [0u16; MAX_DEVICE_ID_LEN as usize];
        let mut ty = 0u32;
        let mut bytes = std::mem::size_of_val(&id) as u32;
        if CM_Get_Device_Interface_PropertyW(path.as_ptr(), &DEVPKEY_Device_InstanceId, &mut ty, id.as_mut_ptr().cast(), &mut bytes, 0) != CR_SUCCESS || ty != DEVPROP_TYPE_STRING {
            return Err("the device instance could not be read".into());
        }
        let mut node = 0u32;
        if CM_Locate_DevNodeW(&mut node, id.as_ptr(), CM_LOCATE_DEVNODE_NORMAL) != CR_SUCCESS {
            return Err("the device node could not be found".into());
        }
        let mut interface = None;
        for _ in 0..16 {
            if CM_Get_Device_IDW(node, id.as_mut_ptr(), id.len() as u32, 0) != CR_SUCCESS {
                break;
            }
            let s = from_wide(&id).to_ascii_uppercase();
            // Do not walk from a Bluetooth mouse up to its USB radio and time that instead.
            if !s.starts_with("HID\\") && !s.starts_with("USB\\") {
                break;
            }
            if s.starts_with("USB\\VID_") {
                if let Some(i) = s.find("&MI_") {
                    let n = u8::from_str_radix(s.get(i + 4..i + 6).unwrap_or(""), 16).map_err(|_| format!("unexpected interface id in {s}"))?;
                    interface = Some(n);
                } else {
                    let mut pdo = [0u16; 256];
                    let mut len = std::mem::size_of_val(&pdo) as u32;
                    let mut reg = 0u32;
                    if CM_Get_DevNode_Registry_PropertyW(node, CM_DRP_PHYSICAL_DEVICE_OBJECT_NAME, &mut reg, pdo.as_mut_ptr().cast(), &mut len, 0) != CR_SUCCESS || reg != 1 {
                        return Err("the USB device object name could not be read".into());
                    }
                    let pdo = from_wide(&pdo);
                    if pdo.is_empty() {
                        return Err("the USB device has no device object name".into());
                    }
                    return Ok(Identity { pdo, interface });
                }
            }
            let mut parent = 0u32;
            if CM_Get_Parent(&mut parent, node, 0) != CR_SUCCESS {
                break;
            }
            node = parent;
        }
        Err("not a USB mouse".into())
    }
}

/// Bounds-checked reader for ETW payloads, which are packed without alignment.
struct Payload<'a> {
    d: &'a [u8],
    ptr: usize,
    ok: bool,
}

impl<'a> Payload<'a> {
    fn take(&mut self, n: usize) -> &'a [u8] {
        if !self.ok || n > self.d.len() {
            self.ok = false;
            return &[];
        }
        let (a, b) = self.d.split_at(n);
        self.d = b;
        a
    }

    fn num(&mut self, n: usize) -> u64 {
        let b = self.take(n);
        let mut v = [0u8; 8];
        v[..b.len().min(8)].copy_from_slice(&b[..b.len().min(8)]);
        u64::from_le_bytes(v)
    }

    fn pointer(&mut self) -> u64 {
        self.num(self.ptr)
    }

    /// A NUL-terminated UTF-16 string.
    fn wstr(&mut self) -> Option<String> {
        let mut out = Vec::new();
        while self.ok {
            let c = self.num(2) as u16;
            if !self.ok {
                break;
            }
            if c == 0 {
                return Some(String::from_utf16_lossy(&out));
            }
            if out.len() > 4096 {
                self.ok = false;
                break;
            }
            out.push(c);
        }
        None
    }
}

/// Finds the single interrupt IN endpoint of the HID interface (class 3, alternate setting 0)
/// in a configuration descriptor. Zero when there is none or more than one.
pub fn hid_interrupt_endpoint(config: &[u8], interface: Option<u8>) -> u8 {
    let (mut endpoint, mut selected, mut i) = (0u8, false, 0usize);
    while i < config.len() {
        let len = config[i] as usize;
        if config.len() - i < 2 || len < 2 || len > config.len() - i {
            return 0;
        }
        let ty = config[i + 1];
        if ty == 4 {
            if len < 9 {
                return 0;
            }
            selected = config[i + 5] == 3 && config[i + 3] == 0 && interface.is_none_or(|n| config[i + 2] == n);
        } else if ty == 5 && selected {
            if len < 7 {
                return 0;
            }
            let address = config[i + 2];
            if address & 0x80 != 0 && config[i + 3] & 3 == 3 {
                if endpoint != 0 {
                    return 0;
                }
                endpoint = address;
            }
        }
        i += len;
    }
    endpoint
}

/// One completed interrupt transfer: its completion time and, when attributable, the time of the
/// controller interrupt that delivered it (0 otherwise). QPC ticks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Completion {
    pub t_completion: i64,
    pub t_interrupt: i64,
}

#[derive(Clone, Copy)]
struct Raw {
    t_cmp: i64,
    t_int: i64,
    device: u64,
    pipe: u64,
}

struct Irq {
    controller: u64,
    number: u32,
    t: i64,
    pending: u8,
}

struct State {
    identity: Identity,
    usb_device: u64,
    endpoint: u8,
    pipes: Vec<(u64, u64, u8)>,
    irqs: Vec<Irq>,
    dpc: Vec<(u64, i64)>,
    events: Vec<Raw>,
    failure: Option<String>,
}

impl State {
    fn new(identity: Identity) -> State {
        State { identity, usb_device: 0, endpoint: 0, pipes: Vec::new(), irqs: Vec::new(), dpc: vec![(0, 0); MAX_CPUS], events: Vec::new(), failure: None }
    }

    fn fail(&mut self, why: &str) {
        self.failure.get_or_insert_with(|| why.to_string());
    }

    fn hub_device(&mut self, mut p: Payload) {
        p.pointer(); // hub
        let device = p.pointer();
        p.num(4); // port
        p.wstr(); // description
        p.wstr(); // interface path
        p.take(18); // device descriptor
        let n = p.num(2) as usize;
        let config = p.take(n);
        let Some(pdo) = p.wstr() else { return };
        if !p.ok || !pdo.eq_ignore_ascii_case(&self.identity.pdo) {
            return;
        }
        let endpoint = hid_interrupt_endpoint(config, self.identity.interface);
        if device == 0 || endpoint == 0 || (self.usb_device != 0 && self.usb_device != device) {
            self.fail("the mouse's USB description did not contain exactly one HID interrupt endpoint");
            return;
        }
        self.usb_device = device;
        self.endpoint = endpoint;
    }

    fn pipe(&mut self, mut p: Payload) {
        let device = p.pointer();
        p.pointer(); // endpoint object
        let pipe = p.pointer();
        let d = p.take(7);
        // An interrupt IN endpoint descriptor: length 7, type 5, IN, interrupt.
        if d.len() < 7 || d[0] < 7 || d[1] != 5 || d[2] & 0x80 == 0 || d[3] & 3 != 3 {
            return;
        }
        if let Some(x) = self.pipes.iter_mut().find(|x| x.1 == pipe) {
            *x = (device, pipe, d[2]);
        } else if self.pipes.len() < 4096 {
            self.pipes.push((device, pipe, d[2]));
        } else {
            self.fail("too many USB pipes");
        }
    }

    fn xhci(&mut self, mut p: Payload, id: u16, cpu: usize, t: i64) {
        if id != 42 && cpu < MAX_CPUS {
            self.dpc[cpu] = (0, 0);
        }
        if id == 44 {
            return;
        }
        let controller = p.pointer();
        let number = p.num(4) as u32;
        if !p.ok || controller == 0 {
            return;
        }
        let idx = match self.irqs.iter().position(|i| i.controller == controller && i.number == number) {
            Some(i) => i,
            None if self.irqs.len() < 256 => {
                self.irqs.push(Irq { controller, number, t: 0, pending: 0 });
                self.irqs.len() - 1
            }
            None => return,
        };
        let irq = &mut self.irqs[idx];
        if id == 42 {
            // Two interrupts before one DPC make attribution ambiguous; mark it so.
            irq.pending = if irq.pending > 0 { 2 } else { 1 };
            irq.t = t;
        } else if id == 43 {
            if cpu < MAX_CPUS {
                self.dpc[cpu] = (controller, if irq.pending == 1 && irq.t <= t { irq.t } else { 0 });
            }
            irq.pending = 0;
        }
    }

    fn completion(&mut self, mut p: Payload, cpu: usize, t: i64) {
        let controller = p.pointer();
        let device = p.pointer();
        let pipe = p.pointer();
        if self.usb_device != 0 && device != self.usb_device {
            return;
        }
        p.take(2 * p.ptr); // IRP, URB
        p.num(2); // URB header length
        let function = p.num(2);
        let status = p.num(4);
        p.take(3 * p.ptr); // device handle, flags pointer, pipe handle
        let flags = p.num(4);
        let length = p.num(4);
        p.take(11 * p.ptr); // buffer, MDL and reserved pointers
        let nt_status = p.num(4);
        if !p.ok {
            self.fail("an unexpected UCX completion layout");
            return;
        }
        // URB_FUNCTION_BULK_OR_INTERRUPT_TRANSFER, successful, direction IN, with data.
        if function != 9 || status != 0 || nt_status & 0x8000_0000 != 0 || flags & 1 == 0 || length == 0 {
            return;
        }
        let anchor = match self.dpc.get(cpu) {
            Some(&(c, ti)) if c == controller && ti <= t => ti,
            _ => 0,
        };
        if self.events.len() < MAX_EVENTS {
            self.events.push(Raw { t_cmp: t, t_int: anchor, device, pipe });
        } else {
            self.fail("too many USB events");
        }
    }
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

unsafe extern "system" fn on_event(ev: *mut EVENT_RECORD) {
    let ev = &*ev;
    let h = &ev.EventHeader;
    let (id, version, t) = (h.EventDescriptor.Id, h.EventDescriptor.Version, h.TimeStamp);
    let flags = h.Flags as u32;
    let cpu = if flags & EVENT_HEADER_FLAG_PROCESSOR_INDEX != 0 {
        ev.BufferContext.Anonymous.ProcessorIndex as usize
    } else {
        ev.BufferContext.Anonymous.Anonymous.ProcessorNumber as usize
    };
    let data: &[u8] = if ev.UserData.is_null() { &[] } else { std::slice::from_raw_parts(ev.UserData as *const u8, ev.UserDataLength as usize) };
    let p = Payload { d: data, ptr: if flags & EVENT_HEADER_FLAG_32_BIT_HEADER != 0 { 4 } else { 8 }, ok: !ev.UserData.is_null() };
    let Ok(mut guard) = STATE.lock() else { return };
    let Some(st) = guard.as_mut() else { return };
    if guid_eq(&h.ProviderId, HUB) {
        if id == 6 && version <= 2 {
            st.hub_device(p);
        }
    } else if guid_eq(&h.ProviderId, XHCI) {
        if (42..=44).contains(&id) {
            // Older manifest versions lack the controller identity; fall back to UCX times.
            if version == 2 {
                st.xhci(p, id, cpu, t);
            } else {
                st.irqs.clear();
                if cpu < MAX_CPUS {
                    st.dpc[cpu] = (0, 0);
                }
            }
        }
    } else if guid_eq(&h.ProviderId, UCX) {
        if id == 6 && version == 0 {
            st.pipe(p);
        } else if id == 27 && version <= 1 {
            st.completion(p, cpu, t);
        }
    }
}

#[repr(C)]
struct Props {
    p: EVENT_TRACE_PROPERTIES,
    name: [u16; 128],
}

fn props() -> Box<Props> {
    let mut b: Box<Props> = Box::new(unsafe { std::mem::zeroed() });
    b.p.Wnode.BufferSize = std::mem::size_of::<Props>() as u32;
    b.p.Wnode.Flags = WNODE_FLAG_TRACED_GUID;
    b.p.Wnode.ClientContext = 1; // QueryPerformanceCounter timestamps, the clock Raw Input uses
    b.p.LogFileMode = EVENT_TRACE_REAL_TIME_MODE;
    b.p.LoggerNameOffset = std::mem::offset_of!(Props, name) as u32;
    b.p.BufferSize = 64;
    b.p.MinimumBuffers = 64;
    b.p.MaximumBuffers = 256;
    b.p.FlushTimer = 1;
    b
}

/// Enables a provider for only the listed event IDs, falling back to all events if the filter is
/// not supported.
unsafe fn enable(session: u64, provider: u128, keywords: u64, ids: &[u16]) -> u32 {
    #[repr(C)]
    struct IdFilter {
        include: u8,
        reserved: u8,
        count: u16,
        ids: [u16; 4],
    }
    let mut f = IdFilter { include: 1, reserved: 0, count: ids.len() as u16, ids: [0; 4] };
    f.ids[..ids.len()].copy_from_slice(ids);
    let fd = EVENT_FILTER_DESCRIPTOR { Ptr: &f as *const IdFilter as u64, Size: 4 + 2 * ids.len() as u32, Type: EVENT_FILTER_TYPE_EVENT_ID };
    let mut ep: ENABLE_TRACE_PARAMETERS = std::mem::zeroed();
    ep.Version = 2;
    ep.EnableFilterDesc = &fd as *const _ as *mut _;
    ep.FilterDescCount = 1;
    let g = guid(provider);
    let h = CONTROLTRACE_HANDLE { Value: session };
    let st = EnableTraceEx2(h, &g, 1, 4, keywords, 0, 0, &ep);
    if st == 87 || st == 50 {
        EnableTraceEx2(h, &g, 1, 4, keywords, 0, 0, std::ptr::null())
    } else {
        st
    }
}

/// A running trace. Dropping it without [`Session::stop`] stops the trace and discards it.
pub struct Session {
    handle: u64,
    consumer: u64,
    thread: Option<JoinHandle<u32>>,
    owner: HANDLE,
}

// The raw handles are only used from the thread that owns the Session.
unsafe impl Send for Session {}

/// What the trace saw for the recorded mouse.
#[derive(Debug, Default)]
pub struct Capture {
    pub completions: Vec<Completion>,
    pub usb_device: u64,
    pub endpoint: u8,
    /// Events the kernel dropped; any value above zero makes the pairing untrustworthy.
    pub lost: u32,
}

fn stop_session(handle: u64) -> (u32, u32) {
    let mut p = props();
    let name = wide(SESSION_NAME);
    let st = unsafe { ControlTraceW(CONTROLTRACE_HANDLE { Value: handle }, if handle == 0 { name.as_ptr() } else { std::ptr::null() }, &mut p.p, 1) };
    (st, p.p.EventsLost + p.p.RealTimeBuffersLost + p.p.LogBuffersLost)
}

impl Session {
    /// Starts tracing the USB stack for the mouse behind `device_path` (its Raw Input name).
    pub fn start(device_path: &str) -> Result<Session, String> {
        let identity = identify(device_path)?;
        unsafe {
            // Only one recording may own the trace; an abandoned mutex means a crashed owner.
            let mutex_name = wide("Global\\MiaUsbTimestampsOwner");
            let owner = CreateMutexW(std::ptr::null(), 0, mutex_name.as_ptr());
            if owner.is_null() {
                return Err("could not create the trace ownership mutex".into());
            }
            let w = WaitForSingleObject(owner, 0);
            if w != WAIT_OBJECT_0 && w != WAIT_ABANDONED {
                CloseHandle(owner);
                return Err("another recording is using the USB trace".into());
            }
            *STATE.lock().unwrap() = Some(State::new(identity));
            let mut s = Session { handle: 0, consumer: u64::MAX, thread: None, owner };
            stop_session(0); // a session left behind by a crash
            let mut p = props();
            let name = wide(SESSION_NAME);
            let mut h = CONTROLTRACE_HANDLE { Value: 0 };
            let st = StartTraceW(&mut h, name.as_ptr(), &mut p.p);
            if st != 0 {
                drop(s);
                return Err(if st == 5 { "starting a trace needs administrator rights".into() } else { format!("StartTrace failed with error {st}") });
            }
            s.handle = h.Value;
            let mut logger = name.clone();
            let mut lf: EVENT_TRACE_LOGFILEW = std::mem::zeroed();
            lf.LoggerName = logger.as_mut_ptr();
            lf.Anonymous1.ProcessTraceMode = PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD | PROCESS_TRACE_MODE_RAW_TIMESTAMP;
            lf.Anonymous2.EventRecordCallback = Some(on_event);
            let consumer = OpenTraceW(&mut lf);
            if consumer.Value == u64::MAX {
                drop(s);
                return Err("OpenTrace failed".into());
            }
            s.consumer = consumer.Value;
            let c = consumer.Value;
            s.thread = Some(std::thread::spawn(move || ProcessTrace(&PROCESSTRACE_HANDLE { Value: c }, 1, std::ptr::null(), std::ptr::null())));
            if enable(s.handle, HUB, 0x8000, &[6]) != 0 || enable(s.handle, UCX, 0x8040, &[6, 27]) != 0 {
                drop(s);
                return Err("the USB trace providers could not be enabled".into());
            }
            // Optional: without interrupt events, completion times are still used.
            enable(s.handle, XHCI, 0x20, &[42, 43, 44]);
            Ok(s)
        }
    }

    /// True once the hub has described the mouse, which happens right after the trace starts.
    pub fn device_found(&self) -> bool {
        STATE.lock().ok().and_then(|g| g.as_ref().map(|s| s.usb_device != 0)).unwrap_or(false)
    }

    fn shutdown(&mut self) -> u32 {
        let mut lost = 0;
        if self.handle != 0 {
            let (st, l) = stop_session(self.handle);
            lost = l;
            if st != 0 && self.consumer != u64::MAX {
                unsafe { CloseTrace(PROCESSTRACE_HANDLE { Value: self.consumer }) };
                self.consumer = u64::MAX;
            }
            self.handle = 0;
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if self.consumer != u64::MAX {
            unsafe { CloseTrace(PROCESSTRACE_HANDLE { Value: self.consumer }) };
            self.consumer = u64::MAX;
        }
        if !self.owner.is_null() {
            unsafe {
                ReleaseMutex(self.owner);
                CloseHandle(self.owner);
            }
            self.owner = std::ptr::null_mut();
        }
        lost
    }

    /// Stops the trace and returns the completions on the mouse's interrupt pipe, in order.
    pub fn stop(mut self) -> Result<Capture, String> {
        let lost = self.shutdown();
        let st = STATE.lock().unwrap().take().ok_or("trace state missing")?;
        if let Some(f) = st.failure {
            return Err(f);
        }
        if st.usb_device == 0 || st.endpoint == 0 {
            return Err("the USB hub never described this mouse".into());
        }
        let pipes: Vec<u64> = st.pipes.iter().filter(|p| p.0 == st.usb_device && p.2 == st.endpoint).map(|p| p.1).collect();
        let pipe = match pipes.as_slice() {
            [p] => *p,
            [] => return Err("no pipe found for the mouse's interrupt endpoint".into()),
            _ if pipes.iter().all(|p| *p == pipes[0]) => pipes[0],
            _ => return Err("more than one pipe matched the mouse's endpoint".into()),
        };
        let mut completions: Vec<Completion> = st
            .events
            .iter()
            .filter(|e| e.device == st.usb_device && e.pipe == pipe)
            .map(|e| Completion { t_completion: e.t_cmp, t_interrupt: e.t_int })
            .collect();
        // Real-time ETW orders events per CPU buffer, not globally.
        completions.sort_by_key(|c| c.t_completion);
        Ok(Capture { completions, usb_device: st.usb_device, endpoint: st.endpoint, lost })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.shutdown();
        if let Ok(mut g) = STATE.lock() {
            *g = None;
        }
    }
}

/// Where a report's final timestamp came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Delivery to this process (Raw Input), as without ETW.
    Raw,
    /// The USB transfer completion.
    Completion,
    /// The controller interrupt that delivered the completion.
    Interrupt,
}

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Source::Raw => "raw",
            Source::Completion => "completion",
            Source::Interrupt => "interrupt",
        }
    }
}

#[derive(Debug, Default)]
pub struct Pairing {
    /// One per Raw Input report, in report order: (QPC ticks, source).
    pub times: Vec<(i64, Option<Source>)>,
    pub interrupts: usize,
    pub completions: usize,
    pub raw: usize,
    /// Reports that shared an interrupt time with the previous one (batched by the controller).
    pub shared: usize,
    /// Completions that could not be assigned to any report.
    pub unmatched: usize,
    /// Mean and spread of Raw Input time minus kernel time, microseconds.
    pub lag_us: f64,
    pub lag_spread_us: f64,
}

/// Assigns completions (sorted) to user-mode report times (sorted). Returns, per user report, the
/// index of its completion or `usize::MAX`, and the number of completions left over.
///
/// A completion can only belong to a report delivered after it. Where three consecutive reports
/// each see exactly one new completion before them, the middle one is a reliable boundary: that
/// completion is its report. Between two boundaries the reports are paired in order, but only when
/// both sides hold the same number; otherwise dropped and batched reports cannot be told apart
/// and those reports keep their Raw Input times.
pub fn match_samples(k: &[i64], u: &[i64]) -> (Vec<usize>, usize) {
    const NONE: usize = usize::MAX;
    let mut map = vec![NONE; u.len()];
    let span = |map: &mut Vec<usize>, kk: usize, uu: usize, n: usize| -> bool {
        if (0..n).any(|i| k[kk + i] > u[uu + i]) {
            return false;
        }
        for i in 0..n {
            map[uu + i] = kk + i;
        }
        true
    };
    let (mut next, mut prev, mut prev2) = (0usize, NONE, NONE);
    let (mut ak, mut au, mut unmatched) = (0usize, 0usize, 0usize);
    let (mut first_k, mut first_u, mut have) = (0usize, 0usize, false);
    for (ui, &ut) in u.iter().enumerate() {
        while next < k.len() && k[next] <= ut {
            next += 1;
        }
        let kk = if next > 0 { next - 1 } else { NONE };
        if ui >= 2 && prev2 != NONE && prev == prev2 + 1 && kk == prev + 1 {
            let (bu, bk) = (ui - 1, prev);
            if !have {
                first_k = bk;
                first_u = bu;
                if bk >= bu {
                    span(&mut map, bk - bu, 0, bu);
                }
            } else {
                let (nc, ns) = (bk - ak - 1, bu - au - 1);
                if nc == ns {
                    span(&mut map, ak + 1, au + 1, ns);
                } else if nc > ns {
                    unmatched += nc - ns;
                }
            }
            map[bu] = bk;
            ak = bk;
            au = bu;
            have = true;
        }
        prev2 = prev;
        prev = kk;
    }
    if have {
        // When the outer boundaries agree, uninterrupted FIFO order beats local boundaries: a long
        // steady delivery delay can look like many clean intervals shifted by one report.
        if ak - first_k == au - first_u && span(&mut map, first_k, first_u, au - first_u + 1) {
            unmatched = 0;
        }
        let (nc, ns) = (next - ak - 1, u.len() - au - 1);
        if nc == ns {
            span(&mut map, ak + 1, au + 1, ns);
        } else if nc > ns {
            unmatched += nc - ns;
        }
    }
    (map, unmatched)
}

/// Chooses each report's timestamp: its interrupt time when attributable, else its completion
/// time, else its Raw Input time. Times never go backwards.
pub fn pair(completions: &[Completion], user: &[i64], qpf: i64) -> Pairing {
    let k: Vec<i64> = completions.iter().map(|c| c.t_completion).collect();
    let (map, unmatched) = match_samples(&k, user);
    let mut out = Pairing { unmatched, ..Pairing::default() };
    let (mut last, mut last_int) = (i64::MIN, 0i64);
    let (mut mean, mut m2, mut n) = (0f64, 0f64, 0usize);
    for (i, &ut) in user.iter().enumerate() {
        let mut chosen = (ut, None);
        if let Some(c) = map.get(i).filter(|m| **m != usize::MAX).map(|m| completions[*m]) {
            if c.t_interrupt > 0 && c.t_interrupt <= c.t_completion && c.t_interrupt >= last {
                chosen = (c.t_interrupt, Some(Source::Interrupt));
                out.interrupts += 1;
                if c.t_interrupt == last_int {
                    out.shared += 1;
                }
                last_int = c.t_interrupt;
            } else if c.t_completion >= last {
                chosen = (c.t_completion, Some(Source::Completion));
                out.completions += 1;
            }
        }
        if chosen.1.is_none() {
            out.raw += 1;
        } else {
            let d = (ut - chosen.0) as f64 * 1e6 / qpf as f64;
            n += 1;
            let delta = d - mean;
            mean += delta / n as f64;
            m2 += delta * (d - mean);
        }
        last = chosen.0;
        out.times.push(chosen);
    }
    if n > 0 {
        out.lag_us = mean;
        out.lag_spread_us = (m2 / n as f64).sqrt();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(t: i64) -> Completion {
        Completion { t_completion: t, t_interrupt: 0 }
    }

    #[test]
    fn steady_delivery_pairs_one_to_one() {
        // 1 kHz reports, delivered 300 ticks after completion.
        let k: Vec<i64> = (0..100).map(|i| 1000 + i * 1000).collect();
        let u: Vec<i64> = k.iter().map(|t| t + 300).collect();
        let (map, unmatched) = match_samples(&k, &u);
        assert_eq!(unmatched, 0);
        assert!(map.iter().enumerate().all(|(i, m)| *m == i));
    }

    #[test]
    fn a_batched_delivery_is_still_paired_in_order() {
        let k: Vec<i64> = (0..60).map(|i| i * 1000).collect();
        let mut u: Vec<i64> = k.iter().map(|t| t + 200).collect();
        // Reports 30..=32 are held back and delivered together.
        for (j, x) in u[30..=32].iter_mut().enumerate() {
            *x = 32_300 + j as i64;
        }
        let (map, _) = match_samples(&k, &u);
        assert!(map.iter().enumerate().all(|(i, m)| *m == i), "{map:?}");
        let p = pair(&k.iter().map(|t| c(*t)).collect::<Vec<_>>(), &u, 10_000_000);
        assert_eq!((p.completions, p.raw), (60, 0));
        // The kernel times are back on the 1000-tick grid.
        assert!(p.times.windows(2).all(|w| w[1].0 - w[0].0 == 1000));
    }

    #[test]
    fn a_report_missing_in_user_mode_leaves_its_span_unpaired() {
        let k: Vec<i64> = (0..40).map(|i| i * 1000).collect();
        let mut u: Vec<i64> = k.iter().map(|t| t + 200).collect();
        u.remove(20);
        let (map, unmatched) = match_samples(&k, &u);
        assert_eq!(unmatched, 1);
        // Pairings that were made are causal and never cross the gap wrongly.
        for (i, m) in map.iter().enumerate() {
            if *m != usize::MAX {
                assert!(k[*m] <= u[i]);
                assert_eq!(k[*m] + 200, u[i], "report {i} paired with the wrong completion");
            }
        }
        assert!(map.iter().filter(|m| **m != usize::MAX).count() >= 30);
    }

    #[test]
    fn interrupt_time_is_preferred_when_it_is_consistent() {
        let comps = vec![
            Completion { t_completion: 1000, t_interrupt: 900 },
            Completion { t_completion: 2000, t_interrupt: 1900 },
            Completion { t_completion: 3000, t_interrupt: 0 },
            Completion { t_completion: 4000, t_interrupt: 4100 }, // after its completion: rejected
            Completion { t_completion: 5000, t_interrupt: 4900 },
        ];
        let u = vec![1200, 2200, 3200, 4200, 5200];
        let p = pair(&comps, &u, 10_000_000);
        let src: Vec<Option<Source>> = p.times.iter().map(|t| t.1).collect();
        assert_eq!(src, [Some(Source::Interrupt), Some(Source::Interrupt), Some(Source::Completion), Some(Source::Completion), Some(Source::Interrupt)]);
        assert_eq!(p.times[0].0, 900);
        assert!((p.lag_us - 25.0).abs() < 10.0, "{}", p.lag_us);
    }

    #[test]
    fn no_completions_means_every_report_keeps_its_raw_time() {
        let u = vec![10, 20, 30];
        let p = pair(&[], &u, 1000);
        assert_eq!(p.raw, 3);
        assert_eq!(p.times.iter().map(|t| t.0).collect::<Vec<_>>(), u);
    }

    #[test]
    fn endpoint_selection_from_a_configuration_descriptor() {
        // Configuration (9) + HID interface 0 (9) + HID descriptor (9) + endpoint 0x81 interrupt IN (7).
        let mut cfg = vec![9, 2, 34, 0, 1, 1, 0, 0xa0, 50];
        cfg.extend([9, 4, 0, 0, 1, 3, 1, 2, 0]);
        cfg.extend([9, 0x21, 0x11, 1, 0, 1, 0x22, 50, 0]);
        cfg.extend([7, 5, 0x81, 3, 8, 0, 1]);
        assert_eq!(hid_interrupt_endpoint(&cfg, None), 0x81);
        assert_eq!(hid_interrupt_endpoint(&cfg, Some(0)), 0x81);
        assert_eq!(hid_interrupt_endpoint(&cfg, Some(1)), 0, "interface 1 does not exist");
        // A second interrupt IN endpoint on the same interface is ambiguous.
        let mut two = cfg.clone();
        two.extend([7, 5, 0x82, 3, 8, 0, 1]);
        assert_eq!(hid_interrupt_endpoint(&two, None), 0);
        // Truncated or zero-length descriptors are rejected, never panic.
        assert_eq!(hid_interrupt_endpoint(&cfg[..30], None), 0);
        assert_eq!(hid_interrupt_endpoint(&[0, 0, 0], None), 0);
        assert_eq!(hid_interrupt_endpoint(&[], None), 0);
    }

    #[test]
    fn payload_reader_is_bounds_checked() {
        let d = [1u8, 0, 0, 0, 0, 0, 0, 0, b'h', 0, b'i', 0, 0, 0];
        let mut p = Payload { d: &d, ptr: 8, ok: true };
        assert_eq!(p.pointer(), 1);
        assert_eq!(p.wstr().as_deref(), Some("hi"));
        assert_eq!(p.num(4), 0);
        assert!(!p.ok);
        let mut q = Payload { d: &[b'x', 0], ptr: 8, ok: true };
        assert_eq!(q.wstr(), None, "no terminator");
    }
}
