use std::{collections::HashMap, fs, path::PathBuf};

use sha2::{Digest, Sha256};
use windows_sys::Win32::Storage::FileSystem::QueryDosDeviceW;

use crate::etw::{Result, wide};

pub struct Section {
    pub name: String,
    pub rva: u32,
    pub size: u32,
}

/// The game executable as laid out on disk: section table (duplicate names numbered `#n`) and `.pdata`.
pub struct GameImage {
    pub path: PathBuf,
    pub sha256: String,
    pub sections: Vec<Section>,
    pub pdata: Vec<(u32, u32)>,
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap())
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

impl GameImage {
    pub fn load(path: PathBuf) -> Result<Self> {
        let b = fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let pe = u32_at(&b, 0x3c) as usize;
        if &b[pe..pe + 4] != b"PE\0\0" || u16_at(&b, pe + 0x18) != 0x20b {
            return Err(format!("{}: not a PE32+ image", path.display()));
        }
        let nsec = u16_at(&b, pe + 6) as usize;
        let opt = pe + 0x18;
        let sec0 = opt + u16_at(&b, pe + 0x14) as usize;
        let (exc_rva, exc_size) = (u32_at(&b, opt + 0x70 + 3 * 8), u32_at(&b, opt + 0x70 + 3 * 8 + 4));
        let mut seen: HashMap<String, u32> = HashMap::new();
        let mut raw = Vec::new();
        let mut sections = Vec::new();
        for i in 0..nsec {
            let s = sec0 + i * 40;
            let base = String::from_utf8_lossy(&b[s..s + 8]).trim_end_matches('\0').to_string();
            let n = seen.entry(base.clone()).and_modify(|n| *n += 1).or_insert(1);
            let (vsize, rva, rsize, roff) =
                (u32_at(&b, s + 8), u32_at(&b, s + 12), u32_at(&b, s + 16), u32_at(&b, s + 20));
            raw.push((rva, rsize, roff));
            sections.push(Section { name: format!("{base}#{n}"), rva, size: vsize.max(rsize) });
        }
        for s in &mut sections {
            if seen[s.name.rsplit_once('#').unwrap().0] == 1 {
                s.name.truncate(s.name.rfind('#').unwrap());
            }
        }
        let off = raw
            .iter()
            .find(|&&(rva, rsize, _)| exc_rva >= rva && exc_rva < rva + rsize)
            .map(|&(rva, _, roff)| (exc_rva - rva + roff) as usize)
            .ok_or("exception directory outside raw sections")?;
        let mut pdata: Vec<(u32, u32)> =
            (0..exc_size as usize / 12).map(|i| (u32_at(&b, off + i * 12), u32_at(&b, off + i * 12 + 4))).collect();
        pdata.sort_unstable();
        let sha256 = Sha256::digest(&b).iter().map(|x| format!("{x:02x}")).collect();
        Ok(Self { path, sha256, sections, pdata })
    }

    pub fn arxan(&self) -> bool {
        self.sections.iter().any(|s| s.name == ".text#2")
    }

    pub fn section(&self, rva: u32) -> Option<&str> {
        self.sections.iter().find(|s| rva >= s.rva && rva < s.rva + s.size).map(|s| s.name.as_str())
    }

    /// Begin RVA of the `.pdata` function containing `rva`.
    pub fn function(&self, rva: u32) -> Option<u32> {
        let i = self.pdata.partition_point(|&(b, _)| b <= rva);
        (i > 0 && rva < self.pdata[i - 1].1).then(|| self.pdata[i - 1].0)
    }

    /// Zone of an image-relative address and a finer key for the top list.
    pub fn zone(&self, rva: u32) -> (String, String) {
        let Some(s) = self.sections.iter().find(|s| rva >= s.rva && rva < s.rva + s.size) else {
            return ("game:headers".into(), "headers".into());
        };
        if s.name == ".text#1" || s.name == ".text" {
            let i = self.pdata.partition_point(|&(b, _)| b <= rva);
            if i > 0 && rva < self.pdata[i - 1].1 {
                return (format!("game:{} func", s.name), format!("fn {:#x}", self.pdata[i - 1].0));
            }
            return (format!("game:{} gap", s.name), format!("{} gap {rva:#x}", s.name));
        }
        (format!("game:{}", s.name), format!("{}:{rva:#x}", s.name))
    }
}

fn rva_to_off(b: &[u8], rva: u32) -> Option<usize> {
    let pe = u32_at(b, 0x3c) as usize;
    let sec0 = pe + 0x18 + u16_at(b, pe + 0x14) as usize;
    (0..u16_at(b, pe + 6) as usize).map(|i| sec0 + i * 40).find_map(|s| {
        let (va, rsize, roff) = (u32_at(b, s + 12), u32_at(b, s + 16), u32_at(b, s + 20));
        (rva >= va && rva < va + rsize).then(|| (rva - va + roff) as usize)
    })
}

/// Named exports of a PE file on disk, sorted by RVA (forwarders skipped).
pub fn exports(path: &std::path::Path) -> Result<Vec<(u32, String)>> {
    let b = fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    if b.len() < 0x40 || u32_at(&b, 0x3c) as usize + 0x18 + 0x78 > b.len() {
        return Err(format!("{}: truncated PE", path.display()));
    }
    let opt = u32_at(&b, 0x3c) as usize + 0x18;
    let dirs = opt + if u16_at(&b, opt) == 0x20b { 0x70 } else { 0x60 };
    let (erva, esize) = (u32_at(&b, dirs), u32_at(&b, dirs + 4));
    let Some(e) = (erva != 0).then(|| rva_to_off(&b, erva)).flatten() else { return Ok(Vec::new()) };
    let (nnames, funcs, names, ords) =
        (u32_at(&b, e + 24) as usize, u32_at(&b, e + 28), u32_at(&b, e + 32), u32_at(&b, e + 36));
    let (Some(funcs), Some(names), Some(ords)) = (rva_to_off(&b, funcs), rva_to_off(&b, names), rva_to_off(&b, ords))
    else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(nnames);
    for i in 0..nnames {
        let ord = u16_at(&b, ords + i * 2) as usize;
        let rva = u32_at(&b, funcs + ord * 4);
        if rva >= erva && rva < erva + esize {
            continue;
        }
        let Some(n) = rva_to_off(&b, u32_at(&b, names + i * 4)) else { continue };
        let end = b[n..].iter().position(|&c| c == 0).map_or(b.len(), |z| n + z);
        out.push((rva, String::from_utf8_lossy(&b[n..end]).into_owned()));
    }
    out.sort();
    Ok(out)
}

/// Disk path of a module path as reported by ETW (`\Device\...`, `\SystemRoot\...`, `\??\C:\...`).
pub fn disk_path(nt: &str) -> Option<PathBuf> {
    let low = nt.to_ascii_lowercase();
    if low.starts_with(r"\systemroot\") {
        return Some(PathBuf::from(format!("{}{}", std::env::var("SystemRoot").ok()?, &nt[11..])));
    }
    if let Some(rest) = nt.strip_prefix(r"\??\") {
        return Some(PathBuf::from(rest));
    }
    if nt.len() > 2 && nt.as_bytes()[1] == b':' {
        return Some(PathBuf::from(nt));
    }
    dos_path(nt)
}

/// `\Device\HarddiskVolumeN\...` -> `C:\...`.
pub fn dos_path(nt: &str) -> Option<PathBuf> {
    for letter in b'A'..=b'Z' {
        let drive = format!("{}:", letter as char);
        let mut buf = [0u16; 512];
        let n = unsafe { QueryDosDeviceW(wide(&drive).as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
        if n == 0 {
            continue;
        }
        let dev = String::from_utf16_lossy(&buf[..buf.iter().position(|&c| c == 0).unwrap_or(0)]);
        if let Some(rest) = nt.strip_prefix(&dev).filter(|r| r.starts_with('\\')) {
            return Some(PathBuf::from(format!("{drive}{rest}")));
        }
    }
    None
}

pub fn basename(path: &str) -> String {
    path.rsplit(['\\', '/']).next().unwrap_or(path).to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exports_of_a_system_dll() {
        let dir = std::env::var("SystemRoot").unwrap();
        let e = exports(&PathBuf::from(dir).join(r"System32\kernel32.dll")).unwrap();
        assert!(e.windows(2).all(|w| w[0] <= w[1]));
        assert!(e.iter().any(|(_, n)| n == "CreateFileW"));
    }

    #[test]
    fn basename_is_lowercased_file_name() {
        assert_eq!(basename(r"C:\Games\DarkSoulsIII.exe"), "darksoulsiii.exe");
        assert_eq!(basename("a/b/C.DLL"), "c.dll");
        assert_eq!(basename("x"), "x");
    }

    #[test]
    fn nt_paths() {
        assert_eq!(disk_path(r"\??\C:\x.dll"), Some(PathBuf::from(r"C:\x.dll")));
        assert_eq!(disk_path(r"D:\y.dll"), Some(PathBuf::from(r"D:\y.dll")));
    }
}
