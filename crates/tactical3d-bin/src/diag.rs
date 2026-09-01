//! Diagnostic bundle export: one Settings-page click zips everything a
//! bug report needs — `crash.log` (next to the
//! exe), `fc_inject.log` (%TEMP%), `settings.json`, a `game.log` tail,
//! and optionally the newest `.hoi4` save — so beta players never have to
//! hunt across three directories. The zip lands next to the exe (falling
//! back to %TEMP% when that dir is not writable).
//!
//! The zip writer is hand-rolled (deflate via flate2, already in the
//! dependency graph through the image pipeline) — no `zip` crate for six
//! files. Entries are compressed in memory first so the local headers
//! carry real CRC/size fields (no data descriptors); anything that does
//! not shrink falls back to stored.

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::settings::AppSettings;

/// Tail caps — whole-file reads for the small ones, bounded tails for the
/// ones that grow (fc_inject.log appends per battle, game.log per HOI4
/// session, crash.log per panic).
const CRASH_TAIL: u64 = 1 << 20; // 1 MiB
const INJECT_TAIL: u64 = 8 << 20; // 8 MiB
const GAMELOG_TAIL: u64 = 512 << 10; // 512 KiB
/// A text save is 50–150 MiB; refuse outright absurd ones so the in-memory
/// zip build cannot OOM a weak laptop.
const SAVE_MAX: u64 = 512 << 20;

pub struct DiagReport {
    pub zip_path: PathBuf,
}

/// Collect the bundle and write the zip. Best-effort per file: a missing
/// piece is noted inside `diag-info.txt`, never fatal. Err only when the
/// zip itself cannot be written anywhere. `include_save` is the Settings
/// page's checkbox (default on — live/injection reports need the save).
pub fn export(settings: &AppSettings, include_save: bool) -> Result<DiagReport, String> {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let stamp = utc_stamp(secs);
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(crate::dirs::runtime_root);

    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    let mut included: Vec<String> = Vec::new();
    let mut missing: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();

    let mut add_tail = |label: &str, path: &Path, cap: u64, entries: &mut Vec<(String, Vec<u8>)>| {
        match read_tail(path, cap) {
            Ok(bytes) => {
                if let Ok(meta) = path.metadata() {
                    if meta.len() > cap {
                        notes.push(format!(
                            "{label}: truncated to the last {} KiB",
                            cap >> 10
                        ));
                    }
                }
                included.push(label.to_string());
                entries.push((label.to_string(), bytes));
            }
            Err(e) => missing.push(format!("{label} ({e})")),
        }
    };

    add_tail("crash.log", &exe_dir.join("crash.log"), CRASH_TAIL, &mut entries);
    add_tail(
        "settings.json",
        &crate::dirs::runtime_root().join("settings.json"),
        u64::MAX,
        &mut entries,
    );
    add_tail(
        "fc_inject.log",
        &std::env::temp_dir().join("fc_inject.log"),
        INJECT_TAIL,
        &mut entries,
    );
    if !settings.log_path.trim().is_empty() {
        add_tail(
            "game_log_tail.txt",
            &PathBuf::from(&settings.log_path),
            GAMELOG_TAIL,
            &mut entries,
        );
    } else {
        missing.push("game.log (path not set)".to_string());
    }

    // Newest save (the live/injection bug reports need it).
    if !include_save {
        notes.push("save: skipped (checkbox off)".to_string());
    } else if let Some(saves_dir) = settings.saves_dir() {
        match crate::settings::list_saves(&saves_dir, 1).first() {
            Some(save) => match save.metadata() {
                Ok(meta) if meta.len() > SAVE_MAX => {
                    missing.push(format!("save: {} (too large, skipped)", save.display()));
                }
                _ => match std::fs::read(save) {
                    Ok(bytes) => {
                        let name = save
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "save.hoi4".to_string());
                        included.push(format!("save/{name}"));
                        entries.push((format!("save/{name}"), bytes));
                    }
                    Err(e) => missing.push(format!("save: {} ({e})", save.display())),
                },
            },
            None => missing.push("save (no .hoi4 in saves dir)".to_string()),
        }
    } else {
        missing.push("save (saves dir not set)".to_string());
    }

    // Manifest first — triage starts here (and it documents the local
    // paths the button tooltip warns about).
    let info = format!(
        "Forward Command diagnostic bundle\nversion: {}\ncreated (UTC): {}\nexe: {}\nruntime root: {}\nhoi4_dir: {}\nsaves_dir: {}\ngame.log: {}\n\nincluded: {}\nmissing: {}\nnotes: {}\n",
        env!("CARGO_PKG_VERSION"),
        stamp,
        std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        crate::dirs::runtime_root().display(),
        settings.hoi4_dir,
        settings.saves_dir,
        settings.log_path,
        if included.is_empty() {
            "-".to_string()
        } else {
            included.join(", ")
        },
        if missing.is_empty() {
            "-".to_string()
        } else {
            missing.join("; ")
        },
        if notes.is_empty() {
            "-".to_string()
        } else {
            notes.join("; ")
        },
    );
    entries.insert(0, ("diag-info.txt".to_string(), info.into_bytes()));

    let dos = dos_datetime(secs);
    // Next to the exe first; %TEMP% when that dir is not writable (e.g.
    // unzipped under Program Files). A second export within the same
    // second overwrites the previous zip (std rename replaces on Windows).
    // The filename carries the version so beta bug reports need no
    // separate "which version are you on" step.
    let file_name = format!("fc-diagnostic-v{}-{stamp}.zip", env!("CARGO_PKG_VERSION"));
    let mut last_err = String::new();
    for dir in [exe_dir, std::env::temp_dir()] {
        let path = dir.join(&file_name);
        match write_zip(&path, &entries, dos) {
            Ok(()) => return Ok(DiagReport { zip_path: path }),
            Err(e) => last_err = e.to_string(),
        }
    }
    Err(last_err)
}

/// `YYYYMMDD-HHMMSS` in UTC from a Unix timestamp — no chrono in the
/// dependency graph, so the civil date comes from Howard Hinnant's
/// civil_from_days.
fn utc_stamp(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let day_secs = secs % 86_400;
    let (h, mi, s) = (day_secs / 3600, (day_secs % 3600) / 60, day_secs % 60);
    let (y, mo, d) = civil_from_days(days);
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}")
}

/// Days since Unix epoch → (year, month, day), proleptic Gregorian.
fn civil_from_days(z: i64) -> (i64, u64, u64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// MS-DOS (time, date) pair for the zip headers.
fn dos_datetime(secs: u64) -> (u16, u16) {
    let day_secs = secs % 86_400;
    let (h, mi, s) = (day_secs / 3600, (day_secs % 3600) / 60, day_secs % 60);
    let (y, mo, d) = civil_from_days((secs / 86_400) as i64);
    let time = ((h as u16) << 11) | ((mi as u16) << 5) | ((s as u16) / 2);
    let date = (((y.max(1980) as u16) - 1980) << 9) | ((mo as u16) << 5) | d as u16;
    (time, date)
}

/// Local wall-clock `YYYY-MM-DD` from a Unix timestamp (About page):
/// the build date shown to players is the exe's own file mtime —
/// the binary's birth time, which survives zip distribution — rendered in
/// the player's own timezone. std has no local-tz formatting, hence the
/// Win32 roundtrip; falls back to the UTC stamp if the conversion fails.
pub fn local_ymd(secs: u64) -> String {
    use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    // FILETIME counts 100-ns ticks since 1601-01-01 (11,644,473,600 s
    // before the Unix epoch).
    let ticks = (secs + 11_644_473_600) * 10_000_000;
    let ft = FILETIME {
        dwLowDateTime: (ticks & 0xFFFF_FFFF) as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut utc = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();
    // SAFETY: plain out-params; both SYSTEMTIME buffers outlive the calls.
    unsafe {
        if FileTimeToSystemTime(&ft, &mut utc).is_ok()
            && SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).is_ok()
        {
            return format!(
                "{:04}-{:02}-{:02}",
                local.wYear, local.wMonth, local.wDay
            );
        }
    }
    utc_stamp(secs)
}

/// Last `cap` bytes of a file, starting on a line boundary when truncated.
fn read_tail(path: &Path, cap: u64) -> io::Result<Vec<u8>> {
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    if len > cap {
        f.seek(SeekFrom::Start(len - cap))?;
    }
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    if len > cap {
        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            buf.drain(..=pos);
        }
    }
    Ok(buf)
}

const LOCAL_SIG: u32 = 0x0403_4b50;
const CENTRAL_SIG: u32 = 0x0201_4b50;
const EOCD_SIG: u32 = 0x0605_4b50;

fn write_zip(path: &Path, entries: &[(String, Vec<u8>)], dos: (u16, u16)) -> io::Result<()> {
    struct Central {
        name: Vec<u8>,
        crc: u32,
        comp_len: u32,
        raw_len: u32,
        offset: u32,
        method: u16,
    }

    let mut out: Vec<u8> = Vec::new();
    let mut central: Vec<Central> = Vec::new();
    for (name, raw) in entries {
        let name = name.as_bytes();
        let crc = crc32fast::hash(raw);
        let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(raw)?;
        let deflated = enc.finish()?;
        let (method, payload): (u16, &[u8]) = if deflated.len() < raw.len() {
            (8, &deflated)
        } else {
            (0, raw) // did not shrink — store
        };
        let offset = out.len() as u32;
        out.extend_from_slice(&LOCAL_SIG.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0x0800u16.to_le_bytes()); // UTF-8 entry names
        out.extend_from_slice(&method.to_le_bytes());
        out.extend_from_slice(&dos.0.to_le_bytes());
        out.extend_from_slice(&dos.1.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&(raw.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra field len
        out.extend_from_slice(name);
        out.extend_from_slice(payload);
        central.push(Central {
            name: name.to_vec(),
            crc,
            comp_len: payload.len() as u32,
            raw_len: raw.len() as u32,
            offset,
            method,
        });
    }
    let cd_start = out.len() as u32;
    for c in &central {
        out.extend_from_slice(&CENTRAL_SIG.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes()); // version made by
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0x0800u16.to_le_bytes());
        out.extend_from_slice(&c.method.to_le_bytes());
        out.extend_from_slice(&dos.0.to_le_bytes());
        out.extend_from_slice(&dos.1.to_le_bytes());
        out.extend_from_slice(&c.crc.to_le_bytes());
        out.extend_from_slice(&c.comp_len.to_le_bytes());
        out.extend_from_slice(&c.raw_len.to_le_bytes());
        out.extend_from_slice(&(c.name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra
        out.extend_from_slice(&0u16.to_le_bytes()); // comment
        out.extend_from_slice(&0u16.to_le_bytes()); // disk number
        out.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        out.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        out.extend_from_slice(&c.offset.to_le_bytes());
        out.extend_from_slice(&c.name);
    }
    let cd_len = out.len() as u32 - cd_start;
    out.extend_from_slice(&EOCD_SIG.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // disk
    out.extend_from_slice(&0u16.to_le_bytes()); // central-dir disk
    out.extend_from_slice(&(central.len() as u16).to_le_bytes());
    out.extend_from_slice(&(central.len() as u16).to_le_bytes());
    out.extend_from_slice(&cd_len.to_le_bytes());
    out.extend_from_slice(&cd_start.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // comment len

    // Atomic-ish: tmp + rename so a half-written zip never reads as valid.
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &out)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_stamp_known_epochs() {
        assert_eq!(utc_stamp(0), "19700101-000000");
        // 2000-02-29 / 03-01 00:00:00 UTC — exercises the leap-year February.
        assert_eq!(utc_stamp(951_782_400), "20000229-000000");
        assert_eq!(utc_stamp(951_868_800), "20000301-000000");
        assert_eq!(utc_stamp(951_868_800 + 86_399), "20000301-235959");
    }

    #[test]
    fn local_ymd_shape_and_tz_sanity() {
        // Shape: strict YYYY-MM-DD. Value is tz-dependent by design; the
        // only wrong answers are a panic or a malformed string.
        for secs in [0u64, 951_868_800, 1_700_000_000] {
            let s = local_ymd(secs);
            assert_eq!(s.len(), 10, "{s}");
            assert_eq!(s.as_bytes()[4], b'-');
            assert_eq!(s.as_bytes()[7], b'-');
            assert!(
                s.bytes()
                    .enumerate()
                    .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit()),
                "{s}"
            );
        }
        // Local date is the UTC date (2000-03-01) or at most one day off
        // (tz shift either way).
        let local = local_ymd(951_868_800);
        assert!(
            local == "2000-03-01" || local == "2000-02-29" || local == "2000-03-02",
            "local {local}"
        );
    }

    #[test]
    fn read_tail_caps_and_aligns_to_lines() {
        let dir = std::env::temp_dir().join(format!("fc_diag_tail_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("log.txt");
        let text = "aaa\nbbb\nccc\nddd\n";
        std::fs::write(&file, text).unwrap();
        let tail = read_tail(&file, 8).unwrap();
        // 8 bytes back lands mid-"ccc"-ish; the first partial line is dropped.
        assert_eq!(tail, b"ddd\n");
        // Small file: whole content, no alignment needed.
        let all = read_tail(&file, 1024).unwrap();
        assert_eq!(all, text.as_bytes());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Minimal zip reader: walk the central directory, inflate each entry,
    /// verify CRC. Roundtripping through a real parse (not just byte
    /// equality) is what makes the hand-rolled writer trustworthy.
    fn read_zip_entries(zip: &[u8]) -> Vec<(String, Vec<u8>)> {
        // EOCD at the tail: entry count, central-dir offset.
        let eocd = &zip[zip.len() - 22..];
        assert_eq!(&eocd[0..4], &EOCD_SIG.to_le_bytes());
        let count = u16::from_le_bytes([eocd[10], eocd[11]]) as usize;
        let mut cd = u32::from_le_bytes([eocd[16], eocd[17], eocd[18], eocd[19]]) as usize;

        let mut seen: Vec<(String, Vec<u8>)> = Vec::new();
        for _ in 0..count {
            assert_eq!(&zip[cd..cd + 4], &CENTRAL_SIG.to_le_bytes());
            let method = u16::from_le_bytes([zip[cd + 10], zip[cd + 11]]);
            let crc = u32::from_le_bytes(zip[cd + 16..cd + 20].try_into().unwrap());
            let comp_len = u32::from_le_bytes(zip[cd + 20..cd + 24].try_into().unwrap()) as usize;
            let raw_len = u32::from_le_bytes(zip[cd + 24..cd + 28].try_into().unwrap()) as usize;
            let name_len = u16::from_le_bytes([zip[cd + 28], zip[cd + 29]]) as usize;
            let offset = u32::from_le_bytes(zip[cd + 42..cd + 46].try_into().unwrap()) as usize;
            let name = String::from_utf8(zip[cd + 46..cd + 46 + name_len].to_vec()).unwrap();

            // Local header → payload.
            assert_eq!(&zip[offset..offset + 4], &LOCAL_SIG.to_le_bytes());
            let l_name_len = u16::from_le_bytes([zip[offset + 26], zip[offset + 27]]) as usize;
            let data_start = offset + 30 + l_name_len;
            let payload = &zip[data_start..data_start + comp_len];
            let bytes = match method {
                0 => payload.to_vec(),
                8 => {
                    let mut dec = flate2::read::DeflateDecoder::new(payload);
                    let mut buf = Vec::new();
                    dec.read_to_end(&mut buf).unwrap();
                    buf
                }
                other => panic!("unexpected method {other}"),
            };
            assert_eq!(bytes.len(), raw_len);
            assert_eq!(crc32fast::hash(&bytes), crc);
            seen.push((name, bytes));
            cd += 46 + name_len;
        }
        seen
    }

    #[test]
    fn zip_roundtrip_deflate_and_stored() {
        let compressible = b"forward command".repeat(500).to_vec();
        // 16 distinct bytes do not shrink under deflate → stored fallback.
        let raw: Vec<u8> = (0u8..16).collect();
        let entries = vec![
            ("logs/crash.log".to_string(), compressible.clone()),
            ("bin.raw".to_string(), raw.clone()),
        ];
        let dir = std::env::temp_dir().join(format!("fc_diag_zip_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let zip_path = dir.join("bundle.zip");
        write_zip(&zip_path, &entries, dos_datetime(951_782_400)).unwrap();
        let zip = std::fs::read(&zip_path).unwrap();
        let seen = read_zip_entries(&zip);
        assert_eq!(seen, [
            ("logs/crash.log".to_string(), compressible),
            ("bin.raw".to_string(), raw),
        ]);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// End-to-end: the real export path (exe-dir resolution, file
    /// collection, rename) produces a parseable zip whose first entry is
    /// the manifest. Machine state decides which logs exist, so only the
    /// manifest is asserted. The zip is cleaned up afterwards
    /// (`FC_KEEP_DIAG_ZIP=1` keeps it for manual inspection).
    #[test]
    fn export_smoke_writes_parseable_zip() {
        let report = export(&AppSettings::default(), false).unwrap();
        println!("diag zip: {}", report.zip_path.display());
        // The filename carries the version (beta reports read it straight
        // off the name) — keep this assert so the naming never regresses.
        let name = report
            .zip_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        assert!(
            name.contains(&format!("v{}", env!("CARGO_PKG_VERSION"))),
            "diag zip name lacks the version: {name}"
        );
        let zip = std::fs::read(&report.zip_path).unwrap();
        let seen = read_zip_entries(&zip);
        assert_eq!(seen[0].0, "diag-info.txt");
        let info = String::from_utf8(seen[0].1.clone()).unwrap();
        assert!(info.contains(env!("CARGO_PKG_VERSION")));
        if std::env::var_os("FC_KEEP_DIAG_ZIP").is_none() {
            std::fs::remove_file(&report.zip_path).ok();
        }
    }
}
