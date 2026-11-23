//! freezemon -- external freeze monitor for DarkSoulsIII*.exe (the original or any reconstructed image).
//!
//! Nothing is injected into the game: frames come from DXGI Present events, CPU time from the kernel sampled
//! profile (ETW). A freeze is a Present-to-Present gap above `--threshold` frame periods; for each freeze the
//! samples of the game's threads inside the gap are attributed to zones (game section / .pdata function /
//! module / kernel) and appended to `freezemon.log` next to the executable. Run as administrator.
//! Stops when the game exits, on Ctrl+C, or when `freezemon.stop` appears next to the executable.

mod etw;
mod image;

use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    fmt::Write as _,
    fs::{self, File, OpenOptions},
    io::{BufWriter, Write},
    path::PathBuf,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, Ordering::Relaxed},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread,
    time::{Duration, Instant},
};

use windows_sys::{
    Win32::{
        Foundation::{FILETIME, SYSTEMTIME},
        System::{
            Console::SetConsoleCtrlHandler,
            Diagnostics::Etw::{EVENT_HEADER_FLAG_64_BIT_HEADER, EVENT_RECORD},
            Performance::{QueryPerformanceCounter, QueryPerformanceFrequency},
            SystemInformation::GetSystemTimeAsFileTime,
            Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime},
        },
    },
    core::GUID,
};

use etw::Result;
use image::GameImage;

const SEC: i64 = 10_000_000;
const DXGI_PRESENT_START: u16 = 42;
const KERNEL_BASE: u64 = 0xffff_8000_0000_0000;
const THREADS_SHOWN: usize = 8;
const ZONES_SHOWN: usize = 6;
const STACK_DEPTH: usize = 24;
const TOP_STACKS: usize = 8;

enum Frame {
    Studio(u32, Option<u32>),
    Arxan(u32),
    Other(String),
}

enum Ev {
    Game { pid: u32, base: u64, size: u64, path: String, t: i64 },
    Module { pid: u32, base: u64, size: u64, path: String },
    KModule { base: u64, size: u64, path: String },
    Exit { pid: u32 },
    Sample { t: i64, raw: i64, tid: u32, ip: u64 },
    Stack { raw: i64, tid: u32, frames: Box<[u64]> },
    Present { t: i64, pid: u32, tid: u32 },
}

static TX: OnceLock<Sender<Ev>> = OnceLock::new();
static STOPPING: AtomicBool = AtomicBool::new(false);
static LOST: Mutex<Option<String>> = Mutex::new(None);
static CLOCK: OnceLock<(i64, i64, i64)> = OnceLock::new();

/// Raw QPC timestamp -> FILETIME (100 ns since 1601).
fn conv(raw: i64) -> i64 {
    let (qpc0, ft0, freq) = *CLOCK.get().unwrap();
    ft0 + ((raw - qpc0) as i128 * SEC as i128 / freq as i128) as i64
}

fn send(e: Ev) {
    let _ = TX.get().unwrap().send(e);
}

#[derive(Default)]
struct KState {
    thread_pid: HashMap<u32, u32>,
    game: HashSet<u32>,
    modules: HashMap<u32, Vec<(u64, u64, String)>>,
}

thread_local! { static K: RefCell<KState> = RefCell::new(KState::default()); }

fn same(a: &GUID, b: &GUID) -> bool {
    (a.data1, a.data2, a.data3, a.data4) == (b.data1, b.data2, b.data3, b.data4)
}

fn data(r: &EVENT_RECORD) -> &[u8] {
    unsafe { std::slice::from_raw_parts(r.UserData.cast::<u8>(), r.UserDataLength as usize) }
}

fn u32_(d: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(d.get(o..o + 4)?.try_into().ok()?))
}

fn u64_(d: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(d.get(o..o + 8)?.try_into().ok()?))
}

fn wstr(d: &[u8], o: usize) -> String {
    let w: Vec<u16> = d
        .get(o..)
        .unwrap_or(&[])
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&c| u16::from_le_bytes(c))
        .take_while(|&c| c != 0)
        .collect();
    String::from_utf16_lossy(&w)
}

fn is_game(path: &str) -> bool {
    let b = image::basename(path);
    b.starts_with("darksoulsiii") && b.ends_with(".exe")
}

unsafe extern "system" fn kernel_cb(r: *mut EVENT_RECORD) {
    let r = unsafe { &*r };
    let h = &r.EventHeader;
    if h.Flags as u32 & EVENT_HEADER_FLAG_64_BIT_HEADER == 0 {
        return;
    }
    let (d, op, raw, g) = (data(r), h.EventDescriptor.Opcode, h.TimeStamp, h.ProviderId);
    let t = conv(raw);
    K.with_borrow_mut(|k| {
        if same(&g, &etw::PERFINFO) && op == 46 {
            if let (Some(ip), Some(tid)) = (u64_(d, 0), u32_(d, 8))
                && k.thread_pid.get(&tid).is_some_and(|p| k.game.contains(p))
            {
                send(Ev::Sample { t, raw, tid, ip });
            }
        } else if same(&g, &etw::STACKWALK) && op == 32 {
            if let (Some(ts), Some(pid), Some(tid)) = (u64_(d, 0), u32_(d, 8), u32_(d, 12))
                && k.game.contains(&pid)
            {
                let frames = d[16..].as_chunks::<8>().0.iter().map(|&c| u64::from_le_bytes(c)).collect();
                send(Ev::Stack { raw: ts as i64, tid, frames });
            }
        } else if same(&g, &etw::THREAD) && matches!(op, 1 | 3) {
            if let (Some(pid), Some(tid)) = (u32_(d, 0), u32_(d, 4)) {
                k.thread_pid.insert(tid, pid);
            }
        } else if same(&g, &etw::IMAGE) && matches!(op, 3 | 10) {
            let (Some(base), Some(size), Some(pid)) = (u64_(d, 0), u64_(d, 8), u32_(d, 16)) else { return };
            let path = wstr(d, 56);
            if base >= KERNEL_BASE {
                send(Ev::KModule { base, size, path });
            } else if is_game(&path) && k.game.insert(pid) {
                send(Ev::Game { pid, base, size, path, t });
                for (b, s, p) in k.modules.remove(&pid).unwrap_or_default() {
                    send(Ev::Module { pid, base: b, size: s, path: p });
                }
            } else if k.game.contains(&pid) {
                send(Ev::Module { pid, base, size, path });
            } else {
                k.modules.entry(pid).or_default().push((base, size, path));
            }
        } else if same(&g, &etw::PROCESS)
            && matches!(op, 2 | 4)
            && let Some(pid) = u32_(d, 8)
        {
            k.modules.remove(&pid);
            if k.game.remove(&pid) {
                send(Ev::Exit { pid });
            }
        }
    });
}

unsafe extern "system" fn user_cb(r: *mut EVENT_RECORD) {
    let h = unsafe { &(*r).EventHeader };
    if same(&h.ProviderId, &etw::DXGI) && h.EventDescriptor.Id == DXGI_PRESENT_START {
        send(Ev::Present { t: conv(h.TimeStamp), pid: h.ProcessId, tid: h.ThreadId });
    }
}

fn shutdown() {
    if !STOPPING.swap(true, Relaxed) {
        let fmt = |p: Option<etw::Props>| {
            p.map_or("not running".into(), |mut p| {
                let (e, b) = etw::lost(&mut p);
                format!("events {e}, buffers {b}")
            })
        };
        let u = fmt(etw::stop(etw::USER));
        let k = fmt(etw::stop(etw::KERNEL));
        *LOST.lock().unwrap() = Some(format!("kernel session: {k}; DXGI session: {u}"));
    }
}

unsafe extern "system" fn ctrl(_: u32) -> i32 {
    shutdown();
    1
}

fn now() -> i64 {
    let mut ft = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    unsafe { GetSystemTimeAsFileTime(&mut ft) };
    (ft.dwHighDateTime as i64) << 32 | ft.dwLowDateTime as i64
}

fn local(t: i64) -> String {
    let ft = FILETIME { dwLowDateTime: t as u32, dwHighDateTime: (t >> 32) as u32 };
    let (mut u, mut l): (SYSTEMTIME, SYSTEMTIME) = unsafe { std::mem::zeroed() };
    unsafe {
        FileTimeToSystemTime(&ft, &mut u);
        SystemTimeToTzSpecificLocalTime(std::ptr::null(), &u, &mut l);
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        l.wYear, l.wMonth, l.wDay, l.wHour, l.wMinute, l.wSecond, l.wMilliseconds
    )
}

struct Config {
    fps: f64,
    threshold: f64,
    interval: u32,
    top: usize,
    log: PathBuf,
    stop_file: PathBuf,
}

impl Config {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Self> {
        let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
        let mut c = Self {
            fps: 60.0,
            threshold: 1.5,
            interval: 1250,
            top: 40,
            log: exe.with_extension("log"),
            stop_file: exe.with_extension("stop"),
        };
        let mut a = args.into_iter();
        while let Some(k) = a.next() {
            let mut v = || a.next().ok_or(format!("{k}: value expected"));
            match k.as_str() {
                "--fps" => c.fps = v()?.parse().map_err(|e| format!("--fps: {e}"))?,
                "--threshold" => c.threshold = v()?.parse().map_err(|e| format!("--threshold: {e}"))?,
                "--interval-us" => {
                    let us: f64 = v()?.parse().map_err(|e| format!("--interval-us: {e}"))?;
                    c.interval = (us * 10.0).round() as u32;
                }
                "--top" => c.top = v()?.parse().map_err(|e| format!("--top: {e}"))?,
                _ => {
                    return Err(format!(
                        "unknown argument {k}\nusage: freezemon [--fps 60] [--threshold 1.5] [--interval-us 125] [--top 40]"
                    ));
                }
            }
        }
        Ok(c)
    }

    fn period(&self) -> i64 {
        (SEC as f64 / self.fps).round() as i64
    }
}

/// Modules (base, size, path) with lazily loaded export tables for `name!export+off` keys.
#[derive(Default)]
struct Modules {
    list: Vec<(u64, u64, String)>,
    exports: HashMap<String, Vec<(u32, String)>>,
}

impl Modules {
    fn insert(&mut self, base: u64, size: u64, path: String) {
        let i = self.list.partition_point(|m| m.0 < base);
        if self.list.get(i).is_some_and(|m| m.0 == base) {
            self.list[i] = (base, size, path);
        } else {
            self.list.insert(i, (base, size, path));
        }
    }

    fn find(&self, ip: u64) -> Option<&(u64, u64, String)> {
        let i = self.list.partition_point(|m| m.0 <= ip);
        (i > 0 && ip < self.list[i - 1].0 + self.list[i - 1].1).then(|| &self.list[i - 1])
    }

    fn symbol(&mut self, ip: u64) -> Option<String> {
        let (base, _, path) = self.find(ip)?.clone();
        let name = image::basename(&path);
        let ex = self
            .exports
            .entry(path.clone())
            .or_insert_with(|| image::disk_path(&path).and_then(|p| image::exports(&p).ok()).unwrap_or_default());
        let rva = (ip - base) as u32;
        let i = ex.partition_point(|e| e.0 <= rva);
        Some(if i > 0 {
            format!("{name}!{}+{:#x}", ex[i - 1].1, rva - ex[i - 1].0)
        } else {
            format!("{name}+{rva:#x}")
        })
    }
}

struct Game {
    pid: u32,
    base: u64,
    size: u64,
    img: GameImage,
    modules: Modules,
}

impl Game {
    fn zone(&self, ip: u64) -> String {
        if ip >= KERNEL_BASE {
            "kernel".into()
        } else if ip >= self.base && ip < self.base + self.size {
            self.img.zone((ip - self.base) as u32).0
        } else {
            self.modules.find(ip).map_or("anonymous".into(), |m| image::basename(&m.2))
        }
    }
}

struct Smp {
    t: i64,
    raw: i64,
    tid: u32,
    ip: u64,
    stack: Option<Box<[u64]>>,
}

struct Freeze {
    start: i64,
    end: i64,
    tid: u32,
}

#[derive(Default)]
struct Totals {
    frames: u64,
    freezes: u64,
    missed: BTreeMap<u64, u64>,
    zones_all: BTreeMap<String, u64>,
    zones_freeze: BTreeMap<String, u64>,
}

struct Monitor {
    cfg: Config,
    interval: u32,
    game: Option<Game>,
    kmods: Modules,
    t0: Option<i64>,
    last: Option<i64>,
    samples: VecDeque<Smp>,
    newest: i64,
    pending: Vec<Freeze>,
    totals: Totals,
    log: BufWriter<File>,
    span: (Option<i64>, i64),
}

fn pct(c: u64, n: u64) -> f64 {
    c as f64 * 100.0 / n.max(1) as f64
}

fn zones_line(m: &BTreeMap<String, u64>, limit: usize) -> String {
    let n: u64 = m.values().sum();
    let mut v: Vec<_> = m.iter().collect();
    v.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    let mut s: Vec<String> = v.iter().take(limit).map(|(z, c)| format!("{z} {:.1}%", pct(**c, n))).collect();
    if v.len() > limit {
        s.push(format!("other {:.1}%", pct(v[limit..].iter().map(|e| *e.1).sum(), n)));
    }
    if s.is_empty() { "-".into() } else { s.join(" | ") }
}

impl Monitor {
    fn timecode(&self, t: i64) -> String {
        let e = (t - self.t0.unwrap_or(t)).max(0);
        let s = e / SEC;
        let fr = ((e % SEC) / self.cfg.period()).min(self.cfg.fps.ceil() as i64 - 1);
        format!("{:02}:{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60, fr)
    }

    fn line(&mut self, s: &str) {
        let _ = writeln!(self.log, "{s}");
    }

    fn busy(&self, samples: u64, dt: i64) -> f64 {
        pct(samples, (dt / self.interval as i64) as u64)
    }

    fn key(&mut self, ip: u64) -> String {
        if ip >= KERNEL_BASE {
            return self.kmods.symbol(ip).unwrap_or_else(|| "kernel ?".into());
        }
        let g = self.game.as_mut().unwrap();
        if ip >= g.base && ip < g.base + g.size {
            return g.img.zone((ip - g.base) as u32).1;
        }
        g.modules.symbol(ip).unwrap_or_else(|| format!("anonymous page {:#x}", ip & !0xfff))
    }

    fn on_game(&mut self, pid: u32, base: u64, size: u64, path: String, t: i64) -> Result<()> {
        self.flush(true);
        let dos = image::disk_path(&path).ok_or(format!("cannot map {path} to a disk path"))?;
        let img = GameImage::load(dos)?;
        let kind = if img.arxan() { "original (.text#2 present)" } else { "no .text#2 (reconstructed)" };
        let head = format!("game pid {pid} started {}: {} -- {kind}", local(t), img.path.display());
        println!("[freezemon] {head}");
        self.line(&head);
        let secs = img
            .sections
            .iter()
            .map(|s| format!("{} {:#x}+{:#x}", s.name, s.rva, s.size))
            .collect::<Vec<_>>()
            .join(", ");
        self.line(&format!("  sha256 {}, base {base:#x}, .pdata {} entries", img.sha256, img.pdata.len()));
        self.line(&format!("  sections: {secs}"));
        self.line("");
        let _ = self.log.flush();
        self.game = Some(Game { pid, base, size, img, modules: Modules::default() });
        (self.t0, self.last) = (None, None);
        self.samples.clear();
        Ok(())
    }

    fn on_sample(&mut self, t: i64, raw: i64, tid: u32, ip: u64) {
        let Some(g) = &self.game else { return };
        self.newest = self.newest.max(t);
        *self.totals.zones_all.entry(g.zone(ip)).or_default() += 1;
        self.samples.push_back(Smp { t, raw, tid, ip, stack: None });
    }

    fn on_stack(&mut self, raw: i64, tid: u32, frames: Box<[u64]>) {
        if let Some(s) =
            self.samples.iter_mut().rev().take(4096).find(|s| s.tid == tid && s.raw == raw && s.stack.is_none())
        {
            s.stack = Some(frames);
        }
    }

    fn on_present(&mut self, t: i64, pid: u32, tid: u32) {
        if self.game.as_ref().is_none_or(|g| g.pid != pid) {
            return;
        }
        self.totals.frames += 1;
        self.span = (Some(self.span.0.unwrap_or(t)), self.span.1.max(t));
        self.t0.get_or_insert(t);
        if let Some(p) = self.last.filter(|&p| t > p)
            && (t - p) as f64 > self.cfg.threshold * self.cfg.period() as f64
        {
            self.pending.push(Freeze { start: p, end: t, tid });
        }
        self.last = Some(self.last.map_or(t, |p| p.max(t)));
    }

    fn emit(&mut self, f: &Freeze) {
        let dt = f.end - f.start;
        let missed = ((dt as f64 / self.cfg.period() as f64).round() as u64).saturating_sub(1);
        let inside: Vec<(u32, u64)> =
            self.samples.iter().filter(|s| s.t >= f.start && s.t < f.end).map(|s| (s.tid, s.ip)).collect();
        let before: Vec<u64> =
            self.samples.iter().filter(|s| s.t >= f.start - SEC && s.t < f.start).map(|s| s.ip).collect();
        let stacks: Vec<Box<[u64]>> = self
            .samples
            .iter()
            .filter(|s| s.tid == f.tid && s.t >= f.start && s.t < f.end)
            .filter_map(|s| s.stack.clone())
            .collect();
        let g = self.game.as_ref().unwrap();
        let (mut zones, mut base) = (BTreeMap::<String, u64>::new(), BTreeMap::<String, u64>::new());
        let mut threads: HashMap<u32, (u64, BTreeMap<String, u64>)> = HashMap::new();
        let mut ips: HashMap<u64, (u64, String)> = HashMap::new();
        for &(tid, ip) in &inside {
            let zone = g.zone(ip);
            *zones.entry(zone.clone()).or_default() += 1;
            let th = threads.entry(tid).or_default();
            th.0 += 1;
            *th.1.entry(zone.clone()).or_default() += 1;
            ips.entry(ip).or_insert((0, zone)).0 += 1;
        }
        for &ip in &before {
            *base.entry(g.zone(ip)).or_default() += 1;
        }
        let mut keys: HashMap<(String, String), u64> = HashMap::new();
        for (ip, (c, zone)) in ips {
            *keys.entry((zone, self.key(ip))).or_default() += c;
        }
        for (z, c) in &zones {
            *self.totals.zones_freeze.entry(z.clone()).or_default() += c;
        }
        self.totals.freezes += 1;
        *self.totals.missed.entry(missed).or_default() += 1;

        let n = inside.len() as u64;
        let main = threads.get(&f.tid).cloned().unwrap_or_default();
        let mut out = String::new();
        let _ = writeln!(
            out,
            "FREEZE #{}  {}  {}  {:.1} ms  {missed} frame(s) missed",
            self.totals.freezes,
            self.timecode(f.start),
            local(f.start),
            dt as f64 / 1e4
        );
        let _ = writeln!(
            out,
            "  present thread {}: busy {:.0}% ({} samples)\n    {}",
            f.tid,
            self.busy(main.0, dt),
            main.0,
            zones_line(&main.1, ZONES_SHOWN)
        );
        let _ = writeln!(out, "  all game threads: {n} samples\n    {}", zones_line(&zones, ZONES_SHOWN));
        let mut th: Vec<_> = threads.iter().collect();
        th.sort_by(|a, b| b.1.0.cmp(&a.1.0).then(a.0.cmp(b.0)));
        let _ = writeln!(out, "  busiest threads:");
        for (tid, (c, z)) in th.iter().take(THREADS_SHOWN) {
            let _ = writeln!(out, "    tid {tid:>6}  busy {:>3.0}%  {}", self.busy(*c, dt), zones_line(z, 3));
        }
        let mut top: Vec<_> = keys.into_iter().collect();
        top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let _ = writeln!(out, "  top locations (all game threads):");
        for ((zone, key), c) in top.iter().take(self.cfg.top) {
            let _ = writeln!(out, "    {c:>6}  {:>5.1}%  {zone:<22} {key}", pct(*c, n));
        }
        self.stacks_report(&mut out, &stacks, main.0);
        let _ = writeln!(out, "  1 s before the freeze (all game threads): {}", zones_line(&base, ZONES_SHOWN));
        self.line(&out);
        let _ = self.log.flush();
        let top_main = main.1.iter().max_by_key(|e| e.1).map_or("-".into(), |(z, _)| z.clone());
        println!(
            "[freezemon] freeze #{} {} {:.1} ms ({missed} missed), present thread busy {:.0}% mostly {top_main}",
            self.totals.freezes,
            self.timecode(f.start),
            dt as f64 / 1e4,
            self.busy(main.0, dt)
        );
    }

    /// Frame of a stack: game section + RVA for game code, module name, or kernel.
    fn frame(&self, ip: u64) -> Frame {
        let g = self.game.as_ref().unwrap();
        if ip >= KERNEL_BASE {
            Frame::Other("[kernel]".into())
        } else if ip >= g.base && ip < g.base + g.size {
            let rva = (ip - g.base) as u32;
            match g.img.section(rva) {
                Some(".text#2") => Frame::Arxan(rva),
                Some(".text#1" | ".text") => Frame::Studio(rva, g.img.function(rva)),
                sec => Frame::Other(format!("{}:{rva:#x}", sec.unwrap_or("game"))),
            }
        } else {
            Frame::Other(g.modules.find(ip).map_or("[anonymous]".into(), |m| image::basename(&m.2)))
        }
    }

    fn stacks_report(&self, out: &mut String, stacks: &[Box<[u64]>], samples: u64) {
        let _ = writeln!(out, "  present thread stacks: {} of {samples} samples", stacks.len());
        if stacks.is_empty() {
            return;
        }
        let n = stacks.len() as u64;
        let (mut incl, mut inner, mut outer, mut tops) = (
            HashMap::<String, u64>::new(),
            HashMap::<u32, u64>::new(),
            HashMap::<u32, u64>::new(),
            HashMap::<String, u64>::new(),
        );
        for st in stacks {
            let fr: Vec<Frame> = st.iter().map(|&ip| self.frame(ip)).collect();
            let mut seen = HashSet::new();
            for f in &fr {
                let k = match f {
                    Frame::Studio(_, Some(b)) => format!("fn {b:#x}"),
                    Frame::Studio(r, None) => format!(".text#1 gap {r:#x}"),
                    Frame::Arxan(_) => ".text#2 (any)".into(),
                    Frame::Other(_) => continue,
                };
                if seen.insert(k.clone()) {
                    *incl.entry(k).or_default() += 1;
                }
            }
            if matches!(fr.first(), Some(Frame::Arxan(_)))
                && let Some(Frame::Studio(r, _)) = fr.iter().find(|f| matches!(f, Frame::Studio(..)))
            {
                *inner.entry(*r).or_default() += 1;
            }
            if let Some(k) = fr.iter().rposition(|f| matches!(f, Frame::Arxan(_)))
                && let Some(Frame::Studio(r, _)) = fr.get(k + 1)
            {
                *outer.entry(*r).or_default() += 1;
            }
            let mut labels: Vec<String> = Vec::new();
            for f in &fr {
                let l = match f {
                    Frame::Studio(r, _) => format!(".text#1:{r:#x}"),
                    Frame::Arxan(r) => format!(".text#2:{r:#x}"),
                    Frame::Other(m) => m.clone(),
                };
                if labels.last() != Some(&l) || matches!(f, Frame::Studio(..) | Frame::Arxan(_)) {
                    labels.push(l);
                }
            }
            labels.truncate(STACK_DEPTH);
            *tops.entry(labels.join(" < ")).or_default() += 1;
        }
        let sorted = |m: HashMap<String, u64>| {
            let mut v: Vec<_> = m.into_iter().collect();
            v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            v
        };
        let rvas = |m: HashMap<u32, u64>| sorted(m.into_iter().map(|(r, c)| (format!(".text#1:{r:#x}"), c)).collect());
        let _ = writeln!(out, "  inclusive, present thread (share of stacks containing it):");
        for (k, c) in sorted(incl).iter().take(self.cfg.top) {
            let _ = writeln!(out, "    {c:>6}  {:>5.1}%  {k}", pct(*c, n));
        }
        for (title, m) in [
            ("innermost .text#1 return address under a sample in .text#2", inner),
            ("first .text#1 return address outside the outermost .text#2 frame", outer),
        ] {
            let v = rvas(m);
            if v.is_empty() {
                continue;
            }
            let _ = writeln!(out, "  {title}:");
            for (k, c) in v.iter().take(10) {
                let _ = writeln!(out, "    {c:>6}  {:>5.1}%  {k}", pct(*c, n));
            }
        }
        let _ = writeln!(out, "  top stacks, present thread (innermost first):");
        for (k, c) in sorted(tops).iter().take(TOP_STACKS) {
            let _ = writeln!(out, "    {c:>6}  {:>5.1}%  {k}", pct(*c, n));
        }
    }

    fn flush(&mut self, all: bool) {
        let ready: Vec<Freeze>;
        (ready, self.pending) =
            std::mem::take(&mut self.pending).into_iter().partition(|f| all || self.newest > f.end + SEC / 2);
        for f in &ready {
            self.emit(f);
        }
        let horizon = self.pending.iter().map(|f| f.start).min().unwrap_or(self.newest) - 2 * SEC;
        while self.samples.front().is_some_and(|s| s.t < horizon) {
            self.samples.pop_front();
        }
    }

    fn summary(&mut self, lost: String) {
        let span = (self.span.1 - self.span.0.unwrap_or(self.span.1)) as f64 / SEC as f64;
        let hist = self.totals.missed.iter().map(|(k, v)| format!("{k}: {v}")).collect::<Vec<_>>().join(", ");
        let mut out = format!(
            "SUMMARY  {} frames over {span:.1} s, {} freeze(s)\n  freezes by frames missed: {}\n",
            self.totals.frames,
            self.totals.freezes,
            if hist.is_empty() { "-" } else { &hist }
        );
        let _ = writeln!(out, "  zones, whole run:      {}", zones_line(&self.totals.zones_all, ZONES_SHOWN + 2));
        let _ = writeln!(out, "  zones, inside freezes: {}", zones_line(&self.totals.zones_freeze, ZONES_SHOWN + 2));
        let _ = writeln!(out, "  ETW lost: {lost}");
        if self.game.is_none() {
            let _ = writeln!(out, "  WARNING: no DarkSoulsIII*.exe process was seen");
        } else if self.totals.frames == 0 {
            let _ = writeln!(out, "  WARNING: no DXGI Present events for the game");
        }
        self.line(&out);
        let _ = self.log.flush();
        println!(
            "[freezemon] {} frames, {} freeze(s) over {span:.1} s -> {}",
            self.totals.frames,
            self.totals.freezes,
            self.cfg.log.display()
        );
    }

    fn run(&mut self, rx: Receiver<Ev>, done: impl Fn() -> bool) -> Result<()> {
        let mut checked = Instant::now();
        loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(Ev::Game { pid, base, size, path, t }) => self.on_game(pid, base, size, path, t)?,
                Ok(Ev::Module { pid, base, size, path }) => {
                    if let Some(g) = self.game.as_mut().filter(|g| g.pid == pid) {
                        g.modules.insert(base, size, path);
                    }
                }
                Ok(Ev::KModule { base, size, path }) => self.kmods.insert(base, size, path),
                Ok(Ev::Exit { pid }) => {
                    if self.game.as_ref().is_some_and(|g| g.pid == pid) {
                        println!("[freezemon] game pid {pid} exited");
                        shutdown();
                    }
                }
                Ok(Ev::Sample { t, raw, tid, ip }) => self.on_sample(t, raw, tid, ip),
                Ok(Ev::Stack { raw, tid, frames }) => self.on_stack(raw, tid, frames),
                Ok(Ev::Present { t, pid, tid }) => self.on_present(t, pid, tid),
                Err(RecvTimeoutError::Timeout) if done() => break,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if checked.elapsed() >= Duration::from_millis(200) {
                checked = Instant::now();
                if self.cfg.stop_file.exists() && !STOPPING.load(Relaxed) {
                    println!("[freezemon] stop file found");
                    shutdown();
                }
            }
            self.flush(false);
        }
        self.flush(true);
        Ok(())
    }
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("[freezemon] error: {e}");
        shutdown();
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let cfg = Config::parse(std::env::args().skip(1))?;
    let _ = fs::remove_file(&cfg.stop_file);
    let (mut qpc, mut freq) = (0i64, 0i64);
    unsafe {
        QueryPerformanceFrequency(&mut freq);
        QueryPerformanceCounter(&mut qpc);
    }
    CLOCK.set((qpc, now(), freq)).map_err(|_| "CLOCK already set")?;
    let (tx, rx) = mpsc::channel();
    TX.set(tx).map_err(|_| "TX already set")?;
    unsafe { SetConsoleCtrlHandler(Some(ctrl), 1) };
    let interval = etw::start_kernel(cfg.interval)?;
    etw::start_user()?;
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&cfg.log)
        .map_err(|e| format!("{}: {e}", cfg.log.display()))?;
    let mut m = Monitor {
        interval,
        game: None,
        kmods: Modules::default(),
        t0: None,
        last: None,
        samples: VecDeque::new(),
        newest: 0,
        pending: Vec::new(),
        totals: Totals::default(),
        log: BufWriter::new(file),
        span: (None, 0),
        cfg,
    };
    let head = format!(
        "=== freezemon session {}  fps {}  threshold {} frames ({:.1} ms)  sampling every {:.1} us (requested {:.1})",
        local(now()),
        m.cfg.fps,
        m.cfg.threshold,
        m.cfg.threshold * m.cfg.period() as f64 / 1e4,
        interval as f64 / 10.0,
        m.cfg.interval as f64 / 10.0
    );
    m.line(&head);
    m.line("  busy = share of the freeze the thread was on a CPU; module!Export+off = nearest preceding exported name (not necessarily the function); fn 0x.. = .pdata function begin RVA, .text#2:0x.. and gap 0x.. = exact instruction RVA");
    let _ = m.log.flush();
    let k = thread::spawn(|| etw::consume(etw::KERNEL, Some(kernel_cb)));
    let u = thread::spawn(|| etw::consume(etw::USER, Some(user_cb)));
    println!("[freezemon] tracing; waiting for DarkSoulsIII*.exe; log {}", m.cfg.log.display());
    let run = m.run(rx, || k.is_finished() && u.is_finished());
    shutdown();
    let (kr, ur) = (k.join().unwrap(), u.join().unwrap());
    let mut lost = LOST.lock().unwrap().take().unwrap_or_default();
    for (name, r) in [("kernel consumer", kr), ("DXGI consumer", ur)] {
        if let Err(e) = r {
            let _ = write!(lost, "; {name}: {e}");
        }
    }
    run?;
    m.summary(lost);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn config_defaults_and_overrides() {
        let c = Config::parse(args("")).unwrap();
        assert_eq!((c.fps, c.threshold, c.interval, c.top), (60.0, 1.5, 1250, 40));
        assert_eq!(c.period(), 166_667);
        let c = Config::parse(args("--fps 30 --threshold 2 --interval-us 250 --top 10")).unwrap();
        assert_eq!((c.fps, c.threshold, c.interval, c.top), (30.0, 2.0, 2500, 10));
        assert!(Config::parse(args("--bogus")).is_err());
        assert!(Config::parse(args("--fps")).is_err());
    }

    #[test]
    fn game_detection() {
        assert!(is_game(r"\Device\HarddiskVolume3\Games\DARK SOULS III\Game\DarkSoulsIII.exe"));
        assert!(is_game("darksoulsiii.unpacked.exe"));
        assert!(!is_game(r"C:\Windows\explorer.exe"));
    }

    #[test]
    fn utf16_field() {
        let mut d = vec![0u8, 0];
        d.extend("ab".encode_utf16().flat_map(u16::to_le_bytes));
        d.extend([0, 0, 0x41, 0]);
        assert_eq!(wstr(&d, 2), "ab");
        assert_eq!(wstr(&d, 100), "");
    }
}
