//! `dyode logs`: weekly archiving and inspection of the transfer logs
//! (the Rust form of dyode_logs.py).
//!
//! Rotation itself is logrotate's job (packaging/logrotate/dyode-transfer).
//! This runs once a week, from a systemd timer, and packs the daily files
//! logrotate left behind into one tarball:
//!
//! ```text
//!   /var/log/dyode-transfer/dyode.log-20260921         rotated by logrotate
//!   /var/log/dyode-transfer/transfer.jsonl-20260921
//!     ->  /var/log/dyode-transfer/archive/dyode-logs-2026-W39.tar.gz
//! ```
//!
//! It only ever touches ROTATED files: the live `dyode.log` and
//! `transfer.jsonl` are never read, moved or deleted.

use crate::timefmt::Date;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

pub const DEFAULT_DIR: &str = "/var/log/dyode-transfer";
pub const ARCHIVE_SUBDIR: &str = "archive";
pub const DEFAULT_KEEP_WEEKS: i64 = 26;

/// `<base>[.-]<8 digits or a number>[.gz]` whose base is a live log name.
fn rotated_base(name: &str) -> Option<&str> {
    let name = name.strip_suffix(".gz").unwrap_or(name);
    let cut = name.rfind(['.', '-'])?;
    let (base, stamp) = (&name[..cut], &name[cut + 1..]);
    if base.is_empty() || stamp.is_empty() || !stamp.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if stamp.len() != 8 && stamp.len() > 7 {
        return None;
    }
    Some(base)
}

/// Rotated log files in `dir`, sorted.  Live files are skipped however
/// they sort, which is what makes this safe to run at any time.
pub fn rotated_files(dir: &Path, live: &[&str]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(rd) = fs::read_dir(dir) else {
        return found;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !e.file_type().map(|t| t.is_file()).unwrap_or(false) || live.contains(&name.as_str()) {
            continue;
        }
        if rotated_base(&name).is_some_and(|b| live.contains(&b)) {
            found.push(e.path());
        }
    }
    found.sort();
    found
}

fn parse_archive_name(name: &str) -> Option<(i64, u32)> {
    let rest = name.strip_prefix("dyode-logs-")?.strip_suffix(".tar.gz")?;
    let (y, w) = rest.split_once("-W")?;
    if y.len() != 4 || w.len() != 2 {
        return None;
    }
    Some((y.parse().ok()?, w.parse().ok()?))
}

/// Delete archives older than `keep_weeks`.  Returns the paths removed.
pub fn prune(
    dir: &Path,
    keep_weeks: i64,
    today: Date,
    dry_run: bool,
    out: &mut dyn Write,
) -> Vec<PathBuf> {
    let adir = dir.join(ARCHIVE_SUBDIR);
    let mut removed = Vec::new();
    if keep_weeks <= 0 || !adir.is_dir() {
        return removed;
    }
    let cutoff = today.days() - keep_weeks * 7;
    let mut names: Vec<String> = fs::read_dir(&adir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    for name in names {
        let Some((y, w)) = parse_archive_name(&name) else {
            continue;
        };
        let Some(monday) = Date::from_iso_week(y, w) else {
            continue;
        };
        if monday.days() >= cutoff {
            continue;
        }
        let p = adir.join(&name);
        if dry_run {
            let _ = writeln!(out, "would remove {}", p.display());
        } else if fs::remove_file(&p).is_ok() {
            let _ = writeln!(out, "removed {}", p.display());
        }
        removed.push(p);
    }
    removed
}

/// Pack rotated logs into this ISO week's tarball and prune old ones.
pub fn archive(
    dir: &Path,
    live: &[&str],
    keep_weeks: i64,
    today: Date,
    dry_run: bool,
    out: &mut dyn Write,
) -> std::io::Result<Option<PathBuf>> {
    let files = rotated_files(dir, live);
    if files.is_empty() {
        let _ = writeln!(out, "nothing to archive in {}", dir.display());
        prune(dir, keep_weeks, today, dry_run, out);
        return Ok(None);
    }
    let adir = dir.join(ARCHIVE_SUBDIR);
    let tarball = adir.join(format!("dyode-logs-{}.tar.gz", today.week_tag()));
    if dry_run {
        let _ = writeln!(
            out,
            "would create {} with {} file(s):",
            tarball.display(),
            files.len()
        );
        for f in &files {
            let _ = writeln!(out, "  {}", f.file_name().unwrap().to_string_lossy());
        }
        prune(dir, keep_weeks, today, true, out);
        return Ok(Some(tarball));
    }
    std::os::unix::fs::DirBuilderExt::mode(fs::DirBuilder::new().recursive(true), 0o750)
        .create(&adir)?;
    // Write beside the target, then rename: a crash or a full disk leaves a
    // .partial behind rather than a truncated archive that looks complete.
    // A second run in the same ISO week rewrites the tarball with the old
    // members plus the new files.
    let partial = adir.join(format!(
        "{}.partial",
        tarball.file_name().unwrap().to_string_lossy()
    ));
    let result = (|| -> std::io::Result<usize> {
        let gz = GzEncoder::new(File::create(&partial)?, flate2::Compression::default());
        let mut tar = tar::Builder::new(gz);
        let mut carried = 0;
        if tarball.exists() {
            let mut old = tar::Archive::new(GzDecoder::new(File::open(&tarball)?));
            for entry in old.entries()? {
                let mut entry = entry?;
                if !entry.header().entry_type().is_file() {
                    continue;
                }
                let mut header = entry.header().clone();
                let path = entry.path()?.into_owned();
                tar.append_data(&mut header, path, &mut entry)?;
                carried += 1;
            }
        }
        for f in &files {
            tar.append_path_with_name(f, f.file_name().unwrap())?;
        }
        tar.into_inner()?.finish()?.sync_all()?;
        Ok(carried)
    })();
    let carried = match result {
        Ok(c) => c,
        Err(e) => {
            let _ = fs::remove_file(&partial);
            return Err(e);
        }
    };
    fs::rename(&partial, &tarball)?;
    for f in &files {
        let _ = fs::remove_file(f);
    }
    let _ = writeln!(
        out,
        "archived {} file(s) into {}{}",
        files.len(),
        tarball.display(),
        if carried > 0 {
            format!(" (kept {carried} already there)")
        } else {
            String::new()
        }
    );
    prune(dir, keep_weeks, today, false, out);
    Ok(Some(tarball))
}

#[derive(Default, Debug)]
pub struct Summary {
    pub events: BTreeMap<String, u64>,
    pub files_sent: u64,
    pub files_stored: u64,
    pub files_rejected: u64,
    pub bytes_sent: u64,
    pub bytes_stored: u64,
    pub log_lines: BTreeMap<&'static str, u64>,
}

/// Totals over a .jsonl file; unparsable lines (a partial last line, say)
/// are skipped.
pub fn summarize(path: &Path) -> std::io::Result<Summary> {
    let mut s = Summary::default();
    for line in BufReader::new(File::open(path)?).lines() {
        let Ok(line) = line else { continue };
        let Ok(ev) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let Some(obj) = ev.as_object() else { continue };
        let name = obj
            .get("event")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string();
        let n = |k: &str| obj.get(k).and_then(Value::as_u64).unwrap_or(0);
        match name.as_str() {
            "batch_sent" => {
                s.files_sent += n("files");
                s.bytes_sent += n("bytes");
            }
            "batch_received" => {
                s.files_stored += n("files_stored");
                s.files_rejected += n("files_rejected");
                s.bytes_stored += n("bytes");
            }
            "file_rejected" => s.files_rejected += 1,
            "logs_stats" => {
                if obj.get("side").and_then(Value::as_str) == Some("in") {
                    *s.log_lines.entry("sent").or_default() += n("lines_sent");
                    *s.log_lines.entry("dropped_at_source").or_default() += n("dropped");
                } else {
                    *s.log_lines.entry("delivered").or_default() += n("lines");
                    *s.log_lines.entry("batches_lost").or_default() += n("lost_batches");
                }
            }
            _ => {}
        }
        *s.events.entry(name).or_default() += 1;
    }
    Ok(s)
}

pub fn human_bytes(count: u64) -> String {
    if count < 1024 {
        return format!("{count} B");
    }
    let mut v = count as f64;
    for unit in ["KiB", "MiB", "GiB", "TiB"] {
        v /= 1024.0;
        if v < 1024.0 || unit == "TiB" {
            return format!("{v:.1} {unit}");
        }
    }
    unreachable!()
}

pub fn print_stats(path: &Path, s: &Summary) {
    println!("{}", path.display());
    for (name, count) in &s.events {
        println!("  {name:<20} {count}");
    }
    println!(
        "  files sent           {} ({})",
        s.files_sent,
        human_bytes(s.bytes_sent)
    );
    println!(
        "  files stored         {} ({})",
        s.files_stored,
        human_bytes(s.bytes_stored)
    );
    println!("  files rejected       {}", s.files_rejected);
    if !s.log_lines.is_empty() {
        let g = |k| s.log_lines.get(k).copied().unwrap_or(0);
        println!("  log lines sent       {}", g("sent"));
        println!("  log lines delivered  {}", g("delivered"));
        println!("  log batches lost     {}", g("batches_lost"));
        println!(
            "  log lines dropped    {} (input buffer full)",
            g("dropped_at_source")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotated_names() {
        assert_eq!(rotated_base("dyode.log-20260921"), Some("dyode.log"));
        assert_eq!(rotated_base("transfer.jsonl.1"), Some("transfer.jsonl"));
        assert_eq!(rotated_base("dyode.log.3.gz"), Some("dyode.log"));
        assert_eq!(rotated_base("dyode.log"), None);
    }

    #[test]
    fn archive_twice_in_a_week_keeps_both() {
        let d = crate::testutil::TempDir::new();
        let live = ["dyode.log", "transfer.jsonl"];
        fs::write(d.path().join("dyode.log"), "live").unwrap();
        fs::write(d.path().join("dyode.log-20260921"), "day1").unwrap();
        let today = Date {
            year: 2026,
            month: 9,
            day: 24,
        };
        let mut sink = Vec::new();
        let t = archive(d.path(), &live, 26, today, false, &mut sink)
            .unwrap()
            .unwrap();
        assert!(t.ends_with("dyode-logs-2026-W39.tar.gz"));
        fs::write(d.path().join("transfer.jsonl-20260922"), "day2").unwrap();
        archive(d.path(), &live, 26, today, false, &mut sink).unwrap();
        let mut names: Vec<String> = tar::Archive::new(GzDecoder::new(File::open(&t).unwrap()))
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, vec!["dyode.log-20260921", "transfer.jsonl-20260922"]);
        assert!(d.path().join("dyode.log").exists(), "live file untouched");
        // An old archive is pruned.
        fs::write(d.path().join("archive/dyode-logs-2025-W01.tar.gz"), "x").unwrap();
        let removed = prune(d.path(), 26, today, false, &mut sink);
        assert_eq!(removed.len(), 1);
    }

    #[test]
    fn stats() {
        let d = crate::testutil::TempDir::new();
        let p = d.path().join("transfer.jsonl");
        fs::write(
            &p,
            "{\"event\":\"batch_sent\",\"files\":2,\"bytes\":100}\n\
             {\"event\":\"logs_stats\",\"side\":\"in\",\"lines_sent\":5}\n\
             {\"event\":\"batch_received\",\"files_stored\":2,\"bytes\":100}\n\
             {partial",
        )
        .unwrap();
        let s = summarize(&p).unwrap();
        assert_eq!((s.files_sent, s.files_stored, s.bytes_sent), (2, 2, 100));
        assert_eq!(s.log_lines["sent"], 5);
        assert_eq!(human_bytes(1536), "1.5 KiB");
    }
}
