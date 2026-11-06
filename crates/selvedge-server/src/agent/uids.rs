//! A Linux user per account for its workerd process.
//!
//! With `--account-uids`, the agent (running as root with only the
//! capabilities this needs) starts each account's workerd under its own UID
//! and GID, and makes that account's directories (0700) and binding socket
//! (0600) owned by it. A compromised process can then neither read another
//! account's files nor connect to its binding socket, which is what stamps an
//! account's identity on KV, R2, D1 and queue calls.
//!
//! An account keeps its UID for the agent's lifetime and across restarts (the
//! map is saved in the state directory), and a UID is never given to a
//! different account, so files left by one account are never readable by
//! another.

use std::collections::BTreeMap;
use std::ops::Range;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use selvedge_core::AccountId;

pub struct Uids {
    range: Range<u32>,
    path: PathBuf,
    map: Mutex<BTreeMap<String, u32>>,
}

/// Parses `first-last` (inclusive).
pub fn parse_range(s: &str) -> Result<Range<u32>> {
    let (a, b) = s.split_once('-').context("expected `first-last`, e.g. 10000-59999")?;
    let (a, b): (u32, u32) = (a.trim().parse()?, b.trim().parse()?);
    if a == 0 || b < a {
        bail!("invalid UID range `{s}`: it must not include 0 and must not be empty");
    }
    Ok(a..b + 1)
}

impl Uids {
    pub fn open(range: Range<u32>, path: PathBuf) -> Result<Self> {
        let map = match std::fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).with_context(|| format!("reading {}", path.display()))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self { range, path, map: Mutex::new(map) })
    }

    /// The account's UID (also its GID), allocating one if needed.
    pub fn uid(&self, account: &AccountId) -> Result<u32> {
        let mut map = self.map.lock().expect("poisoned");
        if let Some(&u) = map.get(account.as_str()) {
            return Ok(u);
        }
        let used: std::collections::HashSet<u32> = map.values().copied().collect();
        let n = u64::from(self.range.end - self.range.start);
        // FNV-1a, so an account usually gets the same UID on every runner.
        let hash = account
            .as_str()
            .bytes()
            .fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3));
        let start = hash % n;
        let uid = (0..n)
            .map(|i| self.range.start + ((start + i) % n) as u32)
            .find(|u| !used.contains(u))
            .context("every account UID in the range is taken")?;
        map.insert(account.as_str().to_owned(), uid);
        let bytes = serde_json::to_vec(&*map)?;
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(uid)
    }
}

/// Makes `path` owned by `uid:uid` with `mode`.
pub fn own(path: &Path, uid: u32, mode: u32) -> Result<()> {
    std::os::unix::fs::chown(path, Some(uid), Some(uid))
        .with_context(|| format!("chown {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("chmod {}", path.display()))
}

/// Makes everything under `path` owned by `uid:uid`, keeping modes.
pub fn own_tree(path: &Path, uid: u32) -> Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    std::os::unix::fs::lchown(path, Some(uid), Some(uid))
        .with_context(|| format!("chown {}", path.display()))?;
    if meta.is_dir() {
        for entry in std::fs::read_dir(path)? {
            own_tree(&entry?.path(), uid)?;
        }
    }
    Ok(())
}

/// Account users must be able to run workerd: the binary and every directory
/// above it need the execute bit for others.
pub fn check_executable_by_others(binary: &Path) -> Result<()> {
    let binary = std::fs::canonicalize(binary).with_context(|| format!("resolving {}", binary.display()))?;
    for p in binary.ancestors() {
        let mode = std::fs::metadata(p)?.permissions().mode();
        if mode & 0o001 == 0 {
            bail!(
                "{} is not executable by other users (mode {:o}), so account users cannot run {}",
                p.display(),
                mode & 0o777,
                binary.display()
            );
        }
    }
    Ok(())
}

/// Sets `dir` to 0711 and checks that every directory above it lets others
/// through.
pub fn traversable(dir: &Path) -> Result<()> {
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o711))
        .with_context(|| format!("chmod {}", dir.display()))?;
    let dir = std::fs::canonicalize(dir)?;
    for p in dir.ancestors().skip(1) {
        let mode = std::fs::metadata(p)?.permissions().mode();
        if mode & 0o001 == 0 {
            bail!(
                "{} does not let other users through (mode {:o}), so account users cannot reach their directories under {}",
                p.display(),
                mode & 0o777,
                dir.display()
            );
        }
    }
    Ok(())
}

/// The effective UID of this process.
pub fn euid() -> u32 {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::metadata("/proc/self").map(|m| m.uid()).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocation_is_stable_and_unique() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("uids.json");
        let uids = Uids::open(parse_range("10000-10001").unwrap(), path.clone()).unwrap();
        let (a, b) = (AccountId::random(), AccountId::random());
        let ua = uids.uid(&a).unwrap();
        let ub = uids.uid(&b).unwrap();
        assert_ne!(ua, ub);
        assert!((10000..=10001).contains(&ua) && (10000..=10001).contains(&ub));
        assert_eq!(uids.uid(&a).unwrap(), ua);
        assert!(uids.uid(&AccountId::random()).is_err());
        let reopened = Uids::open(parse_range("10000-10001").unwrap(), path).unwrap();
        assert_eq!(reopened.uid(&b).unwrap(), ub);
        assert!(parse_range("0-5").is_err());
        assert!(parse_range("9-5").is_err());
    }
}
