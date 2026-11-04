// du for Windows: GNU coreutils du semantics on top of NTFS handles (std only).
// Disk usage = NTFS AllocationSize (the st_blocks analogue); hard links are
// identified by (volume serial, file index).
// shortcut: 32-bit volume serial + 64-bit index (no FileIdInfo/ReFS 128-bit ids), upgrade if ReFS matters.
// shortcut: --exclude patterns have no backslash escapes ('\' is a path separator), '*' crosses '/'.
// shortcut: %Z in --time-style prints the UTC offset, not the zone name.

use std::collections::HashSet;
use std::ffi::{c_void, OsStr, OsString};
use std::io::{self, BufWriter, Read, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::process::exit;

// ---------------------------------------------------------------- Win32 FFI

type Handle = isize;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct FileTime {
    lo: u32,
    hi: u32,
}

#[repr(C)]
#[derive(Default)]
struct ByHandleInfo {
    attrs: u32,
    creation: FileTime,
    access: FileTime,
    write: FileTime,
    vol: u32,
    size_hi: u32,
    size_lo: u32,
    links: u32,
    idx_hi: u32,
    idx_lo: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct SystemTime {
    year: u16,
    month: u16,
    dow: u16,
    day: u16,
    hour: u16,
    min: u16,
    sec: u16,
    ms: u16,
}

#[repr(C)]
#[derive(Default)]
struct StdInfo {
    alloc: i64,
    eof: i64,
    links: u32,
    del: u8,
    dir: u8,
    pad: [u8; 2],
}

#[repr(C)]
#[derive(Default)]
struct TagInfo {
    attrs: u32,
    tag: u32,
}

#[repr(C)]
#[derive(Default)]
struct BasicInfo {
    creation: i64,
    access: i64,
    write: i64,
    change: i64,
    attrs: u32,
    pad: u32,
}

#[link(name = "kernel32")]
extern "system" {
    fn CreateFileW(name: *const u16, access: u32, share: u32, sa: *const c_void, disp: u32, flags: u32, tmpl: Handle) -> Handle;
    fn CloseHandle(h: Handle) -> i32;
    fn GetFileInformationByHandle(h: Handle, out: *mut ByHandleInfo) -> i32;
    fn GetFileInformationByHandleEx(h: Handle, class: i32, out: *mut c_void, size: u32) -> i32;
    fn FileTimeToSystemTime(ft: *const FileTime, st: *mut SystemTime) -> i32;
    fn SystemTimeToTzSpecificLocalTime(tz: *const c_void, utc: *const SystemTime, local: *mut SystemTime) -> i32;
}

const ATTR_DIR: u32 = 0x10;
const ATTR_REPARSE: u32 = 0x400;
const TAG_SYMLINK: u32 = 0xA000_000C;
const TAG_MOUNT_POINT: u32 = 0xA000_0003;
const SEP_CHARS: [u16; 3] = [b'\\' as u16, b'/' as u16, b':' as u16];

// ---------------------------------------------------------------- file info

#[derive(Clone, Copy, PartialEq)]
enum TimeKind {
    Mtime,
    Atime,
    Ctime,
    Birth,
}

struct Info {
    is_dir: bool,
    vol: u32,
    idx: u64,
    links: u32,
    size: u64,
    alloc: u64,
    time: u64, // FILETIME of the requested kind
}

// `\\?\` form so paths over MAX_PATH work with CreateFileW.
fn long_path(p: &Path) -> PathBuf {
    // Judge the absolute length: a short relative path can still resolve past MAX_PATH.
    let a = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    if a.as_os_str().len() < 248 {
        return p.to_path_buf();
    }
    match a.to_str() {
        Some(s) if s.starts_with(r"\\?\") => a,
        Some(s) if s.starts_with(r"\\") => PathBuf::from(format!(r"\\?\UNC\{}", &s[2..])),
        Some(s) => PathBuf::from(format!(r"\\?\{}", s)),
        None => a,
    }
}

fn stat(path: &Path, follow: bool, kind: TimeKind) -> io::Result<Info> {
    let w: Vec<u16> = long_path(path).as_os_str().encode_wide().chain(Some(0)).collect();
    // FILE_READ_ATTRIBUTES only: works on locked files and ACL-restricted ones.
    let mut flags = 0x0200_0000; // BACKUP_SEMANTICS (directories)
    if !follow {
        flags |= 0x0020_0000; // OPEN_REPARSE_POINT
    }
    unsafe {
        let h = CreateFileW(w.as_ptr(), 0x80, 7, std::ptr::null(), 3, flags, 0);
        if h == -1 {
            return Err(io::Error::last_os_error());
        }
        let r = stat_handle(h, follow, kind);
        CloseHandle(h);
        r
    }
}

unsafe fn stat_handle(h: Handle, follow: bool, kind: TimeKind) -> io::Result<Info> {
    let mut d = ByHandleInfo::default();
    if GetFileInformationByHandle(h, &mut d) == 0 {
        return Err(io::Error::last_os_error());
    }
    let ft = |f: FileTime| (f.hi as u64) << 32 | f.lo as u64;
    let size = (d.size_hi as u64) << 32 | d.size_lo as u64;

    let mut link = false;
    if !follow && d.attrs & ATTR_REPARSE != 0 {
        let mut t = TagInfo::default();
        if GetFileInformationByHandleEx(h, 9, &mut t as *mut _ as *mut c_void, 8) != 0 {
            link = t.tag == TAG_SYMLINK || t.tag == TAG_MOUNT_POINT;
        }
    }
    let mut s = StdInfo::default();
    let alloc = if GetFileInformationByHandleEx(h, 1, &mut s as *mut _ as *mut c_void, 24) != 0 {
        s.alloc as u64
    } else {
        size
    };
    let time = match kind {
        TimeKind::Mtime => ft(d.write),
        TimeKind::Atime => ft(d.access),
        TimeKind::Birth => ft(d.creation),
        TimeKind::Ctime => {
            let mut b = BasicInfo::default();
            if GetFileInformationByHandleEx(h, 0, &mut b as *mut _ as *mut c_void, 40) != 0 {
                b.change as u64
            } else {
                ft(d.write)
            }
        }
    };
    Ok(Info {
        is_dir: d.attrs & ATTR_DIR != 0 && !link,
        vol: d.vol,
        idx: (d.idx_hi as u64) << 32 | d.idx_lo as u64,
        links: d.links,
        size,
        alloc,
        time,
    })
}

fn err_text(e: &io::Error) -> String {
    match e.raw_os_error() {
        Some(2) | Some(3) => "No such file or directory".into(),
        Some(5) => "Permission denied".into(),
        Some(267) => "Not a directory".into(),
        _ => {
            let s = e.to_string();
            match s.rfind(" (os error") {
                Some(i) => s[..i].to_string(),
                None => s,
            }
        }
    }
}

// ---------------------------------------------------------------- sizes

// Parses "[N][KMGTPEZY][iB|B]". Returns (multiplier, suffix to print).
fn parse_size(s: &str) -> Option<(u64, String)> {
    let digits = s.bytes().take_while(u8::is_ascii_digit).count();
    let mut mult: u64 = if digits > 0 { s[..digits].parse().ok()? } else { 1 };
    let rest = &s[digits..];
    if digits == 0 && rest.is_empty() {
        return None;
    }
    if rest.is_empty() {
        return Some((mult, String::new()));
    }
    let letter = rest.chars().next()?.to_ascii_uppercase();
    let exp = "KMGTPEZY".find(letter)? + 1;
    let tail = &rest[1..];
    let base: u64 = match tail {
        "" | "iB" => 1024,
        "B" => 1000,
        _ => return None,
    };
    for _ in 0..exp {
        mult = mult.checked_mul(base)?;
    }
    // GNU prints a suffix only for a bare unit ("-BM"), and spells SI kilo as "kB".
    if digits > 0 {
        return Some((mult, String::new()));
    }
    let letter = if base == 1000 && letter == 'K' { 'k' } else { letter };
    Some((mult, format!("{}{}", letter, tail)))
}

// Round-up scaled size like human_ceiling: 1.0K..9.9K with one decimal, else integer.
fn humanize(n: u64, base: u64, units: &[u8]) -> String {
    if n < base {
        return n.to_string();
    }
    let (mut unit, mut i) = (1u128, usize::MAX);
    let n = n as u128;
    let b = base as u128;
    while i.wrapping_add(1) < units.len() && n / unit >= b {
        unit *= b;
        i = i.wrapping_add(1);
    }
    let whole = n.div_ceil(unit);
    let tenths = (n * 10).div_ceil(unit);
    if n / unit < 10 && tenths < 100 {
        format!("{}.{}{}", tenths / 10, tenths % 10, units[i] as char)
    } else if whole >= b && i + 1 < units.len() {
        format!("1.0{}", units[i + 1] as char)
    } else {
        format!("{}{}", whole, units[i] as char)
    }
}

// ---------------------------------------------------------------- time

#[derive(Clone)]
enum Style {
    FullIso,
    LongIso,
    Iso,
    Custom(String),
}

struct Tm {
    year: i64,
    mon: u16,
    day: u16,
    hour: u16,
    min: u16,
    sec: u16,
    dow: u16,
    yday: i64,
    nanos: u64,
    off: i64,
    unix: i64,
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn local_tm(ft: u64) -> Tm {
    let f = FileTime { lo: ft as u32, hi: (ft >> 32) as u32 };
    let (mut utc, mut loc) = (SystemTime::default(), SystemTime::default());
    unsafe {
        if FileTimeToSystemTime(&f, &mut utc) == 0 {
            utc = SystemTime { year: 1601, month: 1, dow: 1, day: 1, ..Default::default() };
        }
        if SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut loc) == 0 {
            loc = utc;
        }
    }
    let secs = |s: &SystemTime| {
        days_from_civil(s.year as i64, s.month as i64, s.day as i64) * 86400
            + s.hour as i64 * 3600
            + s.min as i64 * 60
            + s.sec as i64
    };
    Tm {
        year: loc.year as i64,
        mon: loc.month,
        day: loc.day,
        hour: loc.hour,
        min: loc.min,
        sec: loc.sec,
        dow: loc.dow,
        yday: days_from_civil(loc.year as i64, loc.month as i64, loc.day as i64) - days_from_civil(loc.year as i64, 1, 1) + 1,
        nanos: (ft % 10_000_000) * 100,
        off: secs(&loc) - secs(&utc),
        unix: (ft / 10_000_000) as i64 - 11_644_473_600,
    }
}

const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
const DAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];

fn strftime(f: &str, t: &Tm) -> String {
    let mut o = String::new();
    let mut it = f.chars();
    while let Some(c) = it.next() {
        if c != '%' {
            o.push(c);
            continue;
        }
        let Some(k) = it.next() else {
            o.push('%');
            break;
        };
        let h12 = if t.hour % 12 == 0 { 12 } else { t.hour % 12 };
        let off = t.off.abs();
        match k {
            'Y' => o += &t.year.to_string(),
            'y' => o += &format!("{:02}", t.year % 100),
            'm' => o += &format!("{:02}", t.mon),
            'd' => o += &format!("{:02}", t.day),
            'e' => o += &format!("{:2}", t.day),
            'H' => o += &format!("{:02}", t.hour),
            'I' => o += &format!("{:02}", h12),
            'M' => o += &format!("{:02}", t.min),
            'S' => o += &format!("{:02}", t.sec),
            'p' => o += if t.hour < 12 { "AM" } else { "PM" },
            'a' => o += &DAYS[t.dow as usize % 7][..3],
            'A' => o += DAYS[t.dow as usize % 7],
            'b' | 'h' => o += &MONTHS[(t.mon as usize + 11) % 12][..3],
            'B' => o += MONTHS[(t.mon as usize + 11) % 12],
            'j' => o += &format!("{:03}", t.yday),
            'F' => o += &format!("{}-{:02}-{:02}", t.year, t.mon, t.day),
            'T' => o += &format!("{:02}:{:02}:{:02}", t.hour, t.min, t.sec),
            'R' => o += &format!("{:02}:{:02}", t.hour, t.min),
            'D' => o += &format!("{:02}/{:02}/{:02}", t.mon, t.day, t.year % 100),
            'N' => o += &format!("{:09}", t.nanos),
            's' => o += &t.unix.to_string(),
            'z' => o += &format!("{}{:02}{:02}", if t.off < 0 { '-' } else { '+' }, off / 3600, off % 3600 / 60),
            'Z' => o += &format!("{}{:02}", if t.off < 0 { '-' } else { '+' }, off / 3600),
            'n' => o.push('\n'),
            't' => o.push('\t'),
            '%' => o.push('%'),
            _ => {
                o.push('%');
                o.push(k);
            }
        }
    }
    o
}

fn fmt_time(ft: u64, style: &Style) -> String {
    let t = local_tm(ft);
    strftime(
        match style {
            Style::FullIso => "%F %T.%N %z",
            Style::LongIso => "%F %R",
            Style::Iso => "%F",
            Style::Custom(s) => s,
        },
        &t,
    )
}

// ---------------------------------------------------------------- glob

// fnmatch without FNM_PATHNAME: '*' crosses '/'.
fn fnmatch(p: &[char], s: &[char]) -> bool {
    match p.first() {
        None => s.is_empty(),
        Some('*') => (0..=s.len()).any(|i| fnmatch(&p[1..], &s[i..])),
        Some('?') => !s.is_empty() && fnmatch(&p[1..], &s[1..]),
        Some('[') => {
            let Some(&c) = s.first() else { return false };
            match class(p, c) {
                Some((hit, used)) => hit && fnmatch(&p[used..], &s[1..]),
                None => c == '[' && fnmatch(&p[1..], &s[1..]), // unterminated: literal '['
            }
        }
        Some(&x) => s.first() == Some(&x) && fnmatch(&p[1..], &s[1..]),
    }
}

// p[0] == '['. Returns (c matches, chars consumed).
fn class(p: &[char], c: char) -> Option<(bool, usize)> {
    let mut i = 1;
    let neg = matches!(p.get(i), Some('!') | Some('^'));
    if neg {
        i += 1;
    }
    let mut hit = false;
    let start = i;
    loop {
        let &lo = p.get(i)?;
        if lo == ']' && i > start {
            return Some((hit != neg, i + 1));
        }
        if p.get(i + 1) == Some(&'-') && p.get(i + 2).is_some_and(|&x| x != ']') {
            hit |= lo <= c && c <= p[i + 2];
            i += 3;
        } else {
            hit |= lo == c;
            i += 1;
        }
    }
}

fn norm(s: &str) -> Vec<char> {
    s.replace('\\', "/").to_lowercase().chars().collect()
}

// ---------------------------------------------------------------- config

struct Cfg {
    all: bool,
    apparent: bool,
    total: bool,
    count_links: bool,
    deref: bool,
    deref_args: bool,
    one_fs: bool,
    separate: bool,
    summarize: bool,
    human: bool,
    si: bool,
    inodes: bool,
    show_time: bool,
    nul: bool,
    max_depth: usize,
    max_depth_set: bool,
    block: u64,
    suffix: String,
    thresh: u64,
    thresh_neg: bool,
    time_kind: TimeKind,
    style: Style,
    excl: Vec<Vec<char>>,
    files0: Option<String>,
}

fn die(msg: &str) -> ! {
    eprintln!("du: {}", msg);
    exit(1)
}

fn usage_err(msg: &str) -> ! {
    eprintln!("du: {}\nTry 'du --help' for more information.", msg);
    exit(1)
}

fn parse_style(s: &str) -> Style {
    let s = s.strip_prefix("posix-").unwrap_or(s);
    match s {
        "full-iso" => Style::FullIso,
        "long-iso" | "locale" => Style::LongIso,
        "iso" => Style::Iso,
        _ => match s.strip_prefix('+') {
            Some(f) => Style::Custom(f.to_string()),
            None => die(&format!(
                "invalid argument '{}' for 'time style'\nValid arguments are:\n  - [posix-]full-iso\n  - [posix-]long-iso\n  - [posix-]iso\n  - +FORMAT (e.g., +%H:%M) for a 'date'-style format\nTry 'du --help' for more information.",
                s
            )),
        },
    }
}

impl Cfg {
    fn new() -> Cfg {
        let mut block = if std::env::var_os("POSIXLY_CORRECT").is_some() { 512 } else { 1024 };
        let mut suffix = String::new();
        for v in ["DU_BLOCK_SIZE", "BLOCK_SIZE", "BLOCKSIZE"] {
            if let Some(s) = std::env::var(v).ok().filter(|s| !s.is_empty()) {
                if let Some((m, sfx)) = parse_size(&s).filter(|x| x.0 > 0) {
                    (block, suffix) = (m, sfx);
                }
                break;
            }
        }
        let style = match std::env::var("TIME_STYLE") {
            Ok(s) if !s.is_empty() => parse_style(&s),
            _ => Style::LongIso,
        };
        Cfg {
            all: false,
            apparent: false,
            total: false,
            count_links: false,
            deref: false,
            deref_args: false,
            one_fs: false,
            separate: false,
            summarize: false,
            human: false,
            si: false,
            inodes: false,
            show_time: false,
            nul: false,
            max_depth: usize::MAX,
            max_depth_set: false,
            block,
            suffix,
            thresh: 0,
            thresh_neg: false,
            time_kind: TimeKind::Mtime,
            style,
            excl: Vec::new(),
            files0: None,
        }
    }

    fn set_block(&mut self, m: u64, sfx: &str) {
        self.human = false;
        self.si = false;
        self.block = m;
        self.suffix = sfx.to_string();
    }

    fn apply(&mut self, key: &str, v: &str) {
        match key {
            "a" => self.all = true,
            "apparent-size" => self.apparent = true,
            "b" => {
                self.apparent = true;
                self.set_block(1, "");
            }
            "B" => match parse_size(v).filter(|x| x.0 > 0) {
                Some((m, s)) => self.set_block(m, &s),
                None => die(&format!("invalid --block-size argument '{}'", v)),
            },
            "c" => self.total = true,
            "d" => match v.parse::<usize>() {
                Ok(n) => {
                    self.max_depth = n;
                    self.max_depth_set = true;
                }
                Err(_) => die(&format!("invalid maximum depth '{}'", v)),
            },
            "D" => {
                self.deref = false;
                self.deref_args = true;
            }
            "L" => self.deref = true,
            "P" => {
                self.deref = false;
                self.deref_args = false;
            }
            "h" => {
                self.human = true;
                self.si = false;
            }
            "si" => {
                self.human = true;
                self.si = true;
            }
            "k" => self.set_block(1024, ""),
            "m" => self.set_block(1 << 20, ""),
            "l" => self.count_links = true,
            "s" => self.summarize = true,
            "S" => self.separate = true,
            "x" => self.one_fs = true,
            "0" => self.nul = true,
            "inodes" => self.inodes = true,
            "t" => {
                let (neg, body) = match v.strip_prefix('-') {
                    Some(r) => (true, r),
                    None => (false, v.strip_prefix('+').unwrap_or(v)),
                };
                match parse_size(body) {
                    Some((m, _)) if !(neg && m == 0) => {
                        self.thresh = m;
                        self.thresh_neg = neg;
                    }
                    _ => die(&format!("invalid -t argument '{}'", v)),
                }
            }
            "exclude" => self.excl.push(norm(v)),
            "X" => match std::fs::read(v) {
                Ok(b) => {
                    for l in String::from_utf8_lossy(&b).lines().filter(|l| !l.is_empty()) {
                        self.excl.push(norm(l));
                    }
                }
                Err(e) => die(&format!("{}: {}", v, err_text(&e))),
            },
            "files0-from" => self.files0 = Some(v.to_string()),
            "time" => {
                self.show_time = true;
                self.time_kind = match v {
                    "" => TimeKind::Mtime,
                    "atime" | "access" | "use" => TimeKind::Atime,
                    "ctime" | "status" => TimeKind::Ctime,
                    "birth" | "creation" => TimeKind::Birth,
                    _ => usage_err(&format!("invalid argument '{}' for '--time'", v)),
                };
            }
            "time-style" => self.style = parse_style(v),
            "help" => {
                print!("{}", HELP);
                exit(0)
            }
            "version" => {
                println!("du (GNU coreutils-compatible, Windows port) 1.0");
                exit(0)
            }
            _ => unreachable!(),
        }
    }
}

// (name, takes arg: 0 no / 1 required / 2 optional via '=', key)
const LONGS: &[(&str, u8, &str)] = &[
    ("all", 0, "a"),
    ("apparent-size", 0, "apparent-size"),
    ("block-size", 1, "B"),
    ("bytes", 0, "b"),
    ("count-links", 0, "l"),
    ("dereference", 0, "L"),
    ("dereference-args", 0, "D"),
    ("exclude", 1, "exclude"),
    ("exclude-from", 1, "X"),
    ("files0-from", 1, "files0-from"),
    ("help", 0, "help"),
    ("human-readable", 0, "h"),
    ("inodes", 0, "inodes"),
    ("max-depth", 1, "d"),
    ("no-dereference", 0, "P"),
    ("null", 0, "0"),
    ("one-file-system", 0, "x"),
    ("separate-dirs", 0, "S"),
    ("si", 0, "si"),
    ("summarize", 0, "s"),
    ("threshold", 1, "t"),
    ("time", 2, "time"),
    ("time-style", 1, "time-style"),
    ("total", 0, "c"),
    ("version", 0, "version"),
];

fn parse_args(cfg: &mut Cfg, args: Vec<OsString>) -> Vec<OsString> {
    let mut operands = Vec::new();
    let mut it = args.into_iter();
    while let Some(raw) = it.next() {
        let a = raw.to_string_lossy().into_owned();
        if a == "--" {
            operands.extend(it);
            break;
        } else if let Some(body) = a.strip_prefix("--") {
            let (name, val) = match body.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (body, None),
            };
            let cands: Vec<_> = LONGS.iter().filter(|l| l.0.starts_with(name)).collect();
            let o = match cands.iter().find(|l| l.0 == name) {
                Some(l) => **l,
                None if cands.len() == 1 || (!cands.is_empty() && cands.iter().all(|l| l.2 == cands[0].2)) => *cands[0],
                None if cands.is_empty() => usage_err(&format!("unrecognized option '--{}'", name)),
                None => usage_err(&format!("option '--{}' is ambiguous", name)),
            };
            let v = match (o.1, val) {
                (0, Some(_)) => usage_err(&format!("option '--{}' doesn't allow an argument", o.0)),
                (1, None) => match it.next() {
                    Some(n) => n.to_string_lossy().into_owned(),
                    None => usage_err(&format!("option '--{}' requires an argument", o.0)),
                },
                (_, v) => v.unwrap_or_default(),
            };
            cfg.apply(o.2, &v);
        } else if a.len() > 1 && a.starts_with('-') {
            let cs: Vec<char> = a[1..].chars().collect();
            let mut j = 0;
            while j < cs.len() {
                let c = cs[j];
                if "BdtX".contains(c) {
                    let mut v: String = cs[j + 1..].iter().collect();
                    if v.is_empty() {
                        match it.next() {
                            Some(n) => v = n.to_string_lossy().into_owned(),
                            None => usage_err(&format!("option requires an argument -- '{}'", c)),
                        }
                    }
                    cfg.apply(&c.to_string(), &v);
                    break;
                }
                if !"AabcDHhklLmPsSx0".contains(c) {
                    usage_err(&format!("invalid option -- '{}'", c));
                }
                cfg.apply(
                    match c {
                        'H' => "D",
                        'A' => "apparent-size",
                        _ => &a[1 + j..2 + j], // ASCII: earlier chars of the cluster were validated
                    },
                    "",
                );
                j += 1;
            }
        } else {
            operands.push(raw);
        }
    }
    operands
}

const HELP: &str = "\
Usage: du [OPTION]... [FILE]...
  or:  du [OPTION]... --files0-from=F
Summarize disk usage of the set of FILEs, recursively for directories.

Mandatory arguments to long options are mandatory for short options too.
  -0, --null            end each output line with NUL, not newline
  -a, --all             write counts for all files, not just directories
  -A, --apparent-size   print apparent sizes rather than device usage
  -B, --block-size=SIZE  scale sizes by SIZE before printing them; e.g.,
                          '-BM' prints sizes in units of 1,048,576 bytes
  -b, --bytes           equivalent to '--apparent-size --block-size=1'
  -c, --total           produce a grand total
  -D, --dereference-args  dereference only symlinks that are listed on the
                          command line
  -d, --max-depth=N     print the total for a directory (or file, with --all)
                          only if it is N or fewer levels below the command
                          line argument;  --max-depth=0 is the same as
                          --summarize
      --files0-from=F   summarize device usage of the NUL-terminated file
                          names specified in file F;
                          if F is -, then read names from standard input
  -H                    equivalent to --dereference-args (-D)
  -h, --human-readable  print sizes in human readable format (e.g., 1K 234M 2G)
      --inodes          list inode usage information instead of block usage
  -k                    like --block-size=1K
  -L, --dereference     dereference all symbolic links
  -l, --count-links     count sizes many times if hard linked
  -m                    like --block-size=1M
  -P, --no-dereference  don't follow any symbolic links (this is the default)
  -S, --separate-dirs   for directories do not include size of subdirectories
      --si              like -h, but use powers of 1000 not 1024
  -s, --summarize       display only a total for each argument
  -t, --threshold=SIZE  exclude entries smaller than SIZE if positive,
                          or entries greater than SIZE if negative
      --time            show time of the last modification of any file in the
                          directory, or any of its subdirectories
      --time=WORD       show time as WORD instead of modification time:
                          atime, access, use, ctime, status, birth, creation
      --time-style=STYLE  show times using STYLE, which can be:
                            full-iso, long-iso, iso, or +FORMAT;
                            FORMAT is interpreted like in 'date'
  -X, --exclude-from=FILE  exclude files that match any pattern in FILE
      --exclude=PATTERN    exclude files that match PATTERN
  -x, --one-file-system    skip directories on different file systems
      --help        display this help and exit
      --version     output version information and exit

Display values are in units of the first available SIZE from --block-size,
and the DU_BLOCK_SIZE, BLOCK_SIZE and BLOCKSIZE environment variables.
Otherwise, units default to 1024 bytes (or 512 if POSIXLY_CORRECT is set).

The SIZE argument is an integer and optional unit (example: 10K is 10*1024).
Units are K,M,G,T,P,E (powers of 1024) or KB,MB,... (powers of 1000).
Binary prefixes can be used, too: KiB=K, MiB=M, and so on.

Paths may use '\\' or '/', drive letters and UNC names (\\\\server\\share).
Wildcards (* ?) in the last path component of an operand are expanded.
Disk usage is the NTFS allocation size of each file.
";

// ---------------------------------------------------------------- du

#[derive(Clone, Copy, Default)]
struct Usage {
    size: u64,
    inodes: u64,
    time: u64,
}

impl Usage {
    fn add(&mut self, o: Usage) {
        self.size += o.size;
        self.inodes += o.inodes;
        self.time = self.time.max(o.time);
    }
}

type Key = (u32, u64);

struct Du {
    cfg: Cfg,
    out: BufWriter<io::Stdout>,
    seen: HashSet<Key>,
    grand: Usage,
    status: i32,
    sep: &'static str,
    root_vol: u32,
}

fn quote(p: &OsStr) -> String {
    format!("'{}'", p.to_string_lossy())
}

impl Du {
    fn warn(&mut self, msg: String) {
        let _ = self.out.flush();
        eprintln!("du: {}", msg);
        self.status = 1;
    }

    fn excluded(&self, path: &OsStr) -> bool {
        if self.cfg.excl.is_empty() {
            return false;
        }
        let p = norm(&path.to_string_lossy());
        // a pattern matches the whole path or any trailing run of components
        let mut starts = vec![0];
        starts.extend(p.iter().enumerate().filter(|(_, &c)| c == '/').map(|(i, _)| i + 1));
        self.cfg.excl.iter().any(|pat| starts.iter().any(|&i| fnmatch(pat, &p[i..])))
    }

    fn join(&self, dir: &OsStr, name: &OsStr) -> OsString {
        let mut c = dir.to_os_string();
        if dir.encode_wide().last().is_none_or(|l| !SEP_CHARS.contains(&l)) {
            c.push(self.sep);
        }
        c.push(name);
        c
    }

    fn fmt_size(&self, n: u64) -> String {
        let c = &self.cfg;
        if c.human {
            return if c.si { humanize(n, 1000, b"kMGTPEZY") } else { humanize(n, 1024, b"KMGTPEZY") };
        }
        format!("{}{}", n.div_ceil(c.block), c.suffix)
    }

    fn print(&mut self, u: Usage, name: &OsStr) {
        let v = if self.cfg.inodes { u.inodes } else { u.size };
        if self.cfg.thresh_neg && v > self.cfg.thresh || !self.cfg.thresh_neg && v < self.cfg.thresh {
            return;
        }
        let mut line = self.fmt_size(v);
        if self.cfg.show_time {
            line.push('\t');
            line += &fmt_time(u.time, &self.cfg.style);
        }
        line.push('\t');
        line += &name.to_string_lossy();
        line.push(if self.cfg.nul { '\0' } else { '\n' });
        let _ = self.out.write_all(line.as_bytes());
    }

    fn visit(&mut self, path: &OsString, level: usize, anc: &mut Vec<Key>) -> Usage {
        let none = Usage::default();
        if self.excluded(path) {
            return none;
        }
        let follow = self.cfg.deref || (self.cfg.deref_args && level == 0);
        let fi = match stat(Path::new(path), follow, self.cfg.time_kind) {
            Ok(f) => f,
            Err(e) => {
                self.warn(format!("cannot access {}: {}", quote(path), err_text(&e)));
                return none;
            }
        };
        if level == 0 {
            self.root_vol = fi.vol;
        } else if self.cfg.one_fs && fi.is_dir && fi.vol != self.root_vol {
            return none;
        }
        let key = (fi.vol, fi.idx);
        if fi.is_dir {
            if anc.contains(&key) {
                self.warn(format!("{}: directory causes a cycle", quote(path)));
                return none;
            }
        } else if fi.links > 1 && !self.cfg.count_links && !self.seen.insert(key) {
            return none;
        }

        // st_blocks is in 512-byte units; tiny NTFS files (resident in the MFT) report
        // an AllocationSize below that, so round up like a POSIX du sees them.
        let mut u = Usage {
            // NTFS directories have no meaningful byte size (index size only): count 0, as Explorer does.
            size: match (self.cfg.apparent, fi.is_dir) {
                (true, true) => 0,
                (true, false) => fi.size,
                _ => fi.alloc.div_ceil(512) * 512,
            },
            inodes: 1,
            time: fi.time,
        };
        self.grand.add(u);

        if fi.is_dir {
            match std::fs::read_dir(path) {
                Ok(rd) => {
                    anc.push(key);
                    for ent in rd {
                        match ent {
                            Ok(e) => {
                                let child = self.join(path, &e.file_name());
                                let cu = self.visit(&child, level + 1, anc);
                                u.add(cu);
                            }
                            Err(e) => self.warn(format!("cannot read directory {}: {}", quote(path), err_text(&e))),
                        }
                    }
                    anc.pop();
                }
                Err(e) => self.warn(format!("cannot read directory {}: {}", quote(path), err_text(&e))),
            }
        }

        if level == 0 || level <= self.cfg.max_depth && (fi.is_dir || self.cfg.all) {
            self.print(u, path);
        }
        if self.cfg.separate && fi.is_dir {
            none
        } else {
            u
        }
    }

    fn run_arg(&mut self, arg: &OsString) {
        let s = arg.to_string_lossy();
        self.sep = if s.contains('/') && !s.contains('\\') { "/" } else { "\\" };
        self.visit(arg, 0, &mut Vec::new());
    }
}

// Expand * and ? in the last component when the operand does not exist literally.
fn expand(arg: OsString) -> Vec<OsString> {
    let s = arg.to_string_lossy().into_owned();
    if !s.contains(['*', '?']) || Path::new(&arg).symlink_metadata().is_ok() {
        return vec![arg];
    }
    let cut = s.rfind(['\\', '/', ':']).map_or(0, |i| i + 1);
    let (dir, pat) = s.split_at(cut);
    let pat = norm(pat);
    let Ok(rd) = std::fs::read_dir(if dir.is_empty() { "." } else { dir }) else {
        return vec![arg];
    };
    let mut hits: Vec<OsString> = rd
        .flatten()
        .filter(|e| fnmatch(&pat, &norm(&e.file_name().to_string_lossy())))
        .map(|e| OsString::from(format!("{}{}", dir, e.file_name().to_string_lossy())))
        .collect();
    hits.sort();
    if hits.is_empty() {
        vec![arg]
    } else {
        hits
    }
}

fn run() -> i32 {
    let mut cfg = Cfg::new();
    let mut operands = parse_args(&mut cfg, std::env::args_os().skip(1).collect());

    if cfg.summarize {
        if cfg.all {
            die("cannot both summarize and show all entries");
        }
        if cfg.max_depth_set && cfg.max_depth == 0 {
            eprintln!("du: warning: summarizing is the same as using --max-depth=0");
        } else if cfg.max_depth_set {
            die(&format!("summarizing conflicts with --max-depth={}", cfg.max_depth));
        }
        cfg.max_depth = 0;
    }
    if cfg.inodes {
        if cfg.apparent || cfg.block == 1 {
            eprintln!("du: warning: options --apparent-size and -b are ineffective with --inodes");
        }
        cfg.block = 1;
        cfg.suffix.clear();
    }

    let mut du = Du {
        cfg,
        out: BufWriter::with_capacity(1 << 16, io::stdout()),
        seen: HashSet::new(),
        grand: Usage::default(),
        status: 0,
        sep: "\\",
        root_vol: 0,
    };

    if let Some(f) = du.cfg.files0.clone() {
        if let Some(extra) = operands.first() {
            usage_err(&format!("extra operand {}\nfile operands cannot be combined with --files0-from", quote(extra)));
        }
        let mut buf = Vec::new();
        let r = if f == "-" { io::stdin().read_to_end(&mut buf).map(drop) } else { std::fs::read(&f).map(|b| buf = b) };
        if let Err(e) = r {
            die(&format!("cannot open '{}' for reading: {}", f, err_text(&e)));
        }
        let mut names: Vec<&[u8]> = buf.split(|&b| b == 0).collect();
        if names.last().is_some_and(|n| n.is_empty()) {
            names.pop();
        }
        for (i, n) in names.iter().enumerate() {
            if n.is_empty() {
                du.warn(format!("{}:{}: invalid zero-length file name", f, i + 1));
            } else {
                let name = OsString::from_wide(&String::from_utf8_lossy(n).encode_utf16().collect::<Vec<_>>());
                du.run_arg(&name);
            }
        }
    } else {
        if operands.is_empty() {
            operands.push(".".into());
        }
        for a in operands.into_iter().flat_map(expand) {
            du.run_arg(&a);
        }
    }

    if du.cfg.total {
        let g = du.grand;
        du.print(g, OsStr::new("total"));
    }
    let _ = du.out.flush();
    du.status
}

fn main() {
    // Deep trees recurse; the default 1 MiB main stack is too small for them.
    let code = std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(run)
        .and_then(|h| h.join().map_err(|_| io::Error::other("worker panicked")))
        .unwrap_or_else(|e| die(&e.to_string()));
    exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(n: u64) -> String {
        humanize(n, 1024, b"KMGTPEZY")
    }

    #[test]
    fn human() {
        assert_eq!(h(0), "0");
        assert_eq!(h(1023), "1023");
        assert_eq!(h(1024), "1.0K");
        assert_eq!(h(1025), "1.1K");
        assert_eq!(h(10240), "10K");
        assert_eq!(h(10241), "11K");
        assert_eq!(h(1048000), "1.0M");
        assert_eq!(h(1 << 20), "1.0M");
        assert_eq!(humanize(1100, 1000, b"kMGTPEZY"), "1.1k");
        assert_eq!(humanize(1_000_000, 1000, b"kMGTPEZY"), "1.0M");
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("M"), Some((1 << 20, "M".into())));
        assert_eq!(parse_size("1M"), Some((1 << 20, "".into())));
        assert_eq!(parse_size("KB"), Some((1000, "kB".into())));
        assert_eq!(parse_size("KiB"), Some((1024, "KiB".into())));
        assert_eq!(parse_size("2KiB"), Some((2048, "".into())));
        assert_eq!(parse_size("512"), Some((512, "".into())));
        assert_eq!(parse_size("x"), None);
        assert_eq!(parse_size(""), None);
    }

    #[test]
    fn glob() {
        let m = |p: &str, s: &str| fnmatch(&norm(p), &norm(s));
        assert!(m("*.o", "a/b.o"));
        assert!(m("a?c", "abc"));
        assert!(m("[a-c]x", "bx"));
        assert!(m("[!a-c]x", "dx"));
        assert!(!m("[!a-c]x", "bx"));
        assert!(!m("*.o", "a.c"));
    }

    #[test]
    fn time() {
        // 2000-01-01 00:00:00 UTC, as FILETIME
        let ft = (946_684_800u64 + 11_644_473_600) * 10_000_000;
        let t = local_tm(ft);
        assert_eq!(t.unix, 946_684_800);
        assert_eq!(strftime("%s", &t), "946684800");
        assert_eq!(days_from_civil(1970, 1, 1), 0);
    }
}
