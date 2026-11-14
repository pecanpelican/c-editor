//! Crash recovery: unsaved edits are copied to the config folder every few
//! seconds, and offered back after cedit quits without a chance to save
//! (a crash, a power cut, a killed process).
//!
//! Each running cedit writes `<pid>-<n>.json` backups plus a `<pid>.alive`
//! heartbeat. Backups whose heartbeat stopped are from a cedit that is gone.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

/// A heartbeat older than this means that cedit is no longer running.
const STALE: Duration = Duration::from_secs(20);

#[derive(Serialize, Deserialize, Clone)]
pub struct Backup {
    /// The file the edits belong to (None for an untitled tab).
    pub path: Option<PathBuf>,
    pub text: String,
    #[serde(skip)]
    pub file: PathBuf,
}

pub fn dir() -> Option<PathBuf> {
    crate::settings::config_dir().map(|d| d.join("recovery"))
}

fn pid() -> u32 {
    std::process::id()
}

/// Write (or replace) the backup for one document.
pub fn write(dir: &Path, doc_id: u64, path: Option<&Path>, text: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let file = dir.join(format!("{}-{doc_id}.json", pid()));
    let b = Backup { path: path.map(Path::to_path_buf), text: text.to_string(), file: PathBuf::new() };
    let json = serde_json::to_vec(&b).map_err(std::io::Error::other)?;
    let tmp = file.with_extension("tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &file)?;
    Ok(file)
}

/// Mark this cedit as running.
pub fn heartbeat(dir: &Path) {
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(dir.join(format!("{}.alive", pid())), b"");
}

/// Remove everything this cedit wrote (on a normal quit).
pub fn clear_own(dir: &Path) {
    let prefix = format!("{}-", pid());
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with(&prefix) || name == format!("{}.alive", pid()) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

/// Backups left by cedits that are no longer running.
pub fn orphans(dir: &Path) -> Vec<Backup> {
    orphans_at(dir, SystemTime::now())
}

fn orphans_at(dir: &Path, now: SystemTime) -> Vec<Backup> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let entries: Vec<_> = rd.flatten().collect();
    let alive = |owner: &str| -> bool {
        if owner == pid().to_string() {
            return true;
        }
        let hb = dir.join(format!("{owner}.alive"));
        let fresh = std::fs::metadata(hb)
            .and_then(|m| m.modified())
            .is_ok_and(|t| now.duration_since(t).unwrap_or_default() < STALE);
        // A crash leaves a fresh heartbeat for a few seconds; ask the OS too.
        fresh && owner.parse::<u32>().ok().and_then(process_exists).unwrap_or(true)
    };
    let mut out = Vec::new();
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(stem) = name.strip_suffix(".json") else { continue };
        let Some((owner, _)) = stem.split_once('-') else { continue };
        if alive(owner) {
            continue;
        }
        if let Ok(bytes) = std::fs::read(e.path())
            && let Ok(mut b) = serde_json::from_slice::<Backup>(&bytes)
        {
            b.file = e.path();
            out.push(b);
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Whether a process with this id is running (None if we can't tell).
fn process_exists(pid: u32) -> Option<bool> {
    if cfg!(target_os = "linux") {
        return Some(Path::new(&format!("/proc/{pid}")).exists());
    }
    let out = if cfg!(windows) {
        std::process::Command::new("tasklist").args(["/FI", &format!("PID eq {pid}"), "/NH"]).output().ok()?
    } else {
        std::process::Command::new("ps").args(["-p", &pid.to_string(), "-o", "pid="]).output().ok()?
    };
    Some(String::from_utf8_lossy(&out.stdout).contains(&pid.to_string()))
}

/// Delete restored or discarded backups, and heartbeats nobody owns any more.
pub fn discard(dir: &Path, backups: &[Backup]) {
    for b in backups {
        let _ = std::fs::remove_file(&b.file);
    }
    if let Ok(rd) = std::fs::read_dir(dir) {
        let names: Vec<String> = rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        for n in &names {
            if let Some(owner) = n.strip_suffix(".alive")
                && owner != pid().to_string()
                && !names.iter().any(|m| m.starts_with(&format!("{owner}-")))
            {
                let _ = std::fs::remove_file(dir.join(n));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let dir = std::env::temp_dir().join(format!("cedit-recovery-test-{}", pid()));
        let _ = std::fs::remove_dir_all(&dir);
        write(&dir, 1, Some(Path::new("/x/a.c")), "int a;").unwrap();
        heartbeat(&dir);
        // Our own backups are never orphans.
        assert!(orphans(&dir).is_empty());
        // A backup from a cedit whose heartbeat stopped is.
        std::fs::write(dir.join("999999-3.json"), r#"{"path":null,"text":"x"}"#).unwrap();
        std::fs::write(dir.join("999999.alive"), b"").unwrap();
        // Fresh heartbeat, but no such process: still an orphan.
        assert_eq!(orphans(&dir).len(), usize::from(process_exists(999999) == Some(false)));
        let later = SystemTime::now() + Duration::from_secs(60);
        let o = orphans_at(&dir, later);
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].text, "x");
        discard(&dir, &o);
        assert!(!dir.join("999999-3.json").exists());
        assert!(!dir.join("999999.alive").exists());
        clear_own(&dir);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
