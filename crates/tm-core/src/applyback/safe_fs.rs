//! Directory-handle-relative file access for apply-back and restore (N-17).
//!
//! v7.7.0 joined proposal paths onto the root lexically and wrote with
//! `std::fs::write`, which follows symlinks: a planted `docs/note.md ->
//! ../../outside/target.txt` let a reply overwrite a file outside the root
//! while the preview said `modify docs/note.md`.
//!
//! Here every path is resolved one component at a time from a handle on the
//! root, never through an ambient path:
//! - intermediate directories are opened with `open_dir_nofollow`, so a
//!   symlink (or Windows junction) anywhere on the path is refused, not
//!   followed — even one that points back inside the root;
//! - the final component is opened with `FollowSymlinks::No` (O_NOFOLLOW);
//! - replacements are written to a temp file and renamed over the target
//!   inside the same directory handle (a hard link to an outside file is
//!   detached, not written through); new files are created with O_EXCL;
//! - after opening, the path the OS reports for the handle (`/proc/self/fd`,
//!   `F_GETPATH`, `GetFinalPathNameByHandleW`) must still lie under the root,
//!   and is returned so the caller can re-check it against the path policy.
//!
//! cap-std >= 4.0.3 is required (GHSA-hp8f-xmx4-4qrg: a trailing slash made
//! `O_NOFOLLOW` follow the final symlink); `RelPath` also refuses trailing
//! slashes before any I/O happens.

use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};

/// A validated root-relative path: one or more plain components.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RelPath {
    comps: Vec<String>,
}

/// Windows device names that alias devices in every directory.
#[cfg(windows)]
const RESERVED_NAMES: &[&str] = &[
    "con", "prn", "aux", "nul", "conin$", "conout$", "com0", "com1", "com2", "com3", "com4",
    "com5", "com6", "com7", "com8", "com9", "lpt0", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6",
    "lpt7", "lpt8", "lpt9",
];

impl RelPath {
    /// Parse and vet a proposal path (`/` or `\` separated).
    pub fn parse(raw: &str) -> Result<RelPath, String> {
        let rel = raw.trim();
        if rel.is_empty() {
            return Err("empty path".into());
        }
        // ':' rejects drive letters and NTFS alternate data streams in one go.
        if rel.contains(':') {
            return Err("absolute or drive-qualified paths are not allowed".into());
        }
        if rel.starts_with('/') || rel.starts_with('\\') || Path::new(rel).is_absolute() {
            return Err("absolute paths are not allowed".into());
        }
        if rel.ends_with('/') || rel.ends_with('\\') {
            return Err("path ends with a separator — a proposal must name a file".into());
        }
        if rel.chars().any(|c| c.is_control()) {
            return Err("control characters in path".into());
        }
        let mut comps = Vec::new();
        for comp in rel.split(['/', '\\']) {
            match comp {
                "" | "." => continue,
                ".." => return Err("path escapes the target root".into()),
                _ => {}
            }
            if comp.len() > 255 {
                return Err("path component longer than 255 bytes".into());
            }
            if comp.ends_with('.') || comp.ends_with(' ') {
                return Err(format!(
                    "`{}` ends in a dot or space — Windows would open a different name",
                    comp
                ));
            }
            #[cfg(windows)]
            {
                let stem = comp.split('.').next().unwrap_or("").to_ascii_lowercase();
                if RESERVED_NAMES.contains(&stem.as_str()) {
                    return Err(format!("`{}` is a reserved device name on Windows", comp));
                }
            }
            #[cfg(windows)]
            if comp
                .split('~')
                .skip(1)
                .any(|t| t.starts_with(|c: char| c.is_ascii_digit()))
            {
                return Err(format!(
                    "`{}` looks like an 8.3 short name, which can alias another file",
                    comp
                ));
            }
            comps.push(comp.to_string());
        }
        // Belt and braces: nothing std would treat as special survives.
        let joined: PathBuf = comps.iter().collect();
        if comps.is_empty()
            || joined
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err("path escapes the target root".into());
        }
        Ok(RelPath { comps })
    }

    fn from_comps(comps: Vec<String>) -> RelPath {
        RelPath { comps }
    }

    /// `/`-joined display form.
    pub fn display(&self) -> String {
        self.comps.join("/")
    }

    pub fn file_name(&self) -> &str {
        self.comps.last().map(String::as_str).unwrap_or("")
    }

    fn parents(&self) -> &[String] {
        &self.comps[..self.comps.len() - 1]
    }

    /// This path nested under `prefix` components (backup trees).
    pub fn under(&self, prefix: &[&str]) -> RelPath {
        let mut comps: Vec<String> = prefix.iter().map(|s| s.to_string()).collect();
        comps.extend(self.comps.iter().cloned());
        RelPath { comps }
    }
}

/// What is on disk at a path.
#[derive(Debug, Clone, Copy)]
pub struct FileInfo {
    pub len: u64,
    /// Unix permission bits (`& 0o777`); `None` on Windows.
    pub mode: Option<u32>,
    pub executable: bool,
    /// Other names sharing this inode (they keep the old content after a replace).
    pub extra_links: u64,
}

/// A read: `None` when missing, else bytes, info and the OS-resolved
/// root-relative path (when the platform reports one).
pub type ReadOutcome = Option<(Vec<u8>, FileInfo, Option<String>)>;

/// A root directory handle plus the OS path of that handle.
pub struct SafeRoot {
    dir: Dir,
    root_os: Option<PathBuf>,
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn describe(e: &io::Error) -> String {
    e.to_string()
}

impl SafeRoot {
    pub fn open(root: &Path) -> Result<SafeRoot, String> {
        let dir = Dir::open_ambient_dir(root, cap_std::ambient_authority())
            .map_err(|e| format!("cannot open target root: {}", describe(&e)))?;
        let root_os = os_path(&dir).or_else(|| dunce::canonicalize(root).ok());
        Ok(SafeRoot { dir, root_os })
    }

    /// Open `comps` as a directory chain without following any link.
    /// `Ok(None)` when a component is missing and `create` is false.
    fn open_chain(&self, comps: &[String], create: bool) -> Result<Option<Dir>, String> {
        let mut cur = self
            .dir
            .try_clone()
            .map_err(|e| format!("cannot reopen root: {}", describe(&e)))?;
        let mut walked: Vec<&str> = Vec::new();
        for comp in comps {
            walked.push(comp);
            let next = match cur.open_dir_nofollow(comp) {
                Ok(d) => d,
                Err(e) => match cur.symlink_metadata(comp) {
                    Err(m) if m.kind() == io::ErrorKind::NotFound => {
                        if !create {
                            return Ok(None);
                        }
                        cur.create_dir(comp).map_err(|e| {
                            format!("cannot create `{}`: {}", walked.join("/"), describe(&e))
                        })?;
                        cur.open_dir_nofollow(comp).map_err(|e| {
                            format!("cannot open `{}`: {}", walked.join("/"), describe(&e))
                        })?
                    }
                    Ok(m) if m.file_type().is_symlink() => {
                        return Err(format!(
                            "`{}` is a symlink — apply-back never writes through links",
                            walked.join("/")
                        ))
                    }
                    Ok(m) if !m.is_dir() => {
                        return Err(format!("`{}` is not a directory", walked.join("/")))
                    }
                    _ => {
                        return Err(format!(
                            "cannot open `{}`: {}",
                            walked.join("/"),
                            describe(&e)
                        ))
                    }
                },
            };
            cur = next;
        }
        Ok(Some(cur))
    }

    /// The directory at `rel` itself (all components), if present.
    pub fn open_dir(&self, rel: &RelPath) -> Result<Option<Dir>, String> {
        self.open_chain(&rel.comps, false)
    }

    /// Check an opened handle: it must resolve under the root. Returns the
    /// root-relative path the OS reports (on-disk case), when available.
    fn resolved(&self, os: Option<PathBuf>) -> Result<Option<String>, String> {
        let (Some(root), Some(path)) = (&self.root_os, os) else {
            return Ok(None);
        };
        let root_comps: Vec<Component> = root.components().collect();
        let path_comps: Vec<Component> = path.components().collect();
        let under_root = path_comps.len() >= root_comps.len()
            && root_comps
                .iter()
                .zip(&path_comps)
                .all(|(r, p)| same_component(r.as_os_str(), p.as_os_str()));
        if !under_root {
            return Err(format!(
                "resolves outside the target root ({}) — refusing",
                path.display()
            ));
        }
        let rest: Vec<String> = path_comps[root_comps.len()..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        Ok(Some(rest.join("/")))
    }

    /// Metadata of the final component without following it. `Ok(None)` when
    /// it (or a parent) does not exist.
    pub fn stat(&self, rel: &RelPath) -> Result<Option<FileInfo>, String> {
        let Some(parent) = self.open_chain(rel.parents(), false)? else {
            return Ok(None);
        };
        match parent.symlink_metadata(rel.file_name()) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("cannot stat: {}", describe(&e))),
            Ok(m) => Ok(Some(file_info(&m)?)),
        }
    }

    /// A sibling whose name differs from `rel`'s only by case (a repo with
    /// both breaks on macOS and Windows checkouts).
    pub fn case_variant(&self, rel: &RelPath) -> Result<Option<String>, String> {
        let Some(parent) = self.open_chain(rel.parents(), false)? else {
            return Ok(None);
        };
        let want = rel.file_name();
        let entries = parent
            .entries()
            .map_err(|e| format!("cannot list directory: {}", describe(&e)))?;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name != want && name.to_lowercase() == want.to_lowercase() {
                return Ok(Some(name));
            }
        }
        Ok(None)
    }

    /// Read a regular file without following links. Returns bytes, info and
    /// the OS-resolved relative path (see `resolved`).
    pub fn read(&self, rel: &RelPath) -> Result<ReadOutcome, String> {
        let Some(parent) = self.open_chain(rel.parents(), false)? else {
            return Ok(None);
        };
        let name = rel.file_name();
        match parent.symlink_metadata(name) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("cannot stat: {}", describe(&e))),
            Ok(m) => {
                file_info(&m)?;
            }
        }
        let mut opts = OpenOptions::new();
        opts.read(true).follow(FollowSymlinks::No);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            // A FIFO swapped in after the stat must not block the read.
            opts.custom_flags(libc::O_NONBLOCK);
        }
        let mut f = parent
            .open_with(name, &opts)
            .map_err(|e| format!("cannot open: {}", describe(&e)))?;
        let meta = f
            .metadata()
            .map_err(|e| format!("cannot stat: {}", describe(&e)))?;
        let info = file_info(&meta)?;
        let resolved = self.resolved(os_path(&f))?;
        let mut buf = Vec::with_capacity(info.len as usize);
        f.read_to_end(&mut buf)
            .map_err(|e| format!("unreadable: {}", describe(&e)))?;
        Ok(Some((buf, info, resolved)))
    }

    /// Atomically replace an existing regular file: temp file in the same
    /// directory handle, fsync, then rename over the target. `mode` (unix
    /// permission bits) is applied to the replacement.
    pub fn replace(&self, rel: &RelPath, bytes: &[u8], mode: Option<u32>) -> Result<(), String> {
        let parent = self
            .open_chain(rel.parents(), false)?
            .ok_or("parent directory disappeared")?;
        let name = rel.file_name();
        match parent.symlink_metadata(name) {
            Ok(m) => {
                file_info(&m)?;
            }
            Err(e) => return Err(format!("target vanished: {}", describe(&e))),
        }
        self.resolved(os_path(&parent))?;
        let tmp = format!(
            ".{}.tm-{}-{}.tmp",
            name,
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true).follow(FollowSymlinks::No);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let written = (|| -> io::Result<()> {
            let mut f = parent.open_with(&tmp, &opts)?;
            f.write_all(bytes)?;
            #[cfg(unix)]
            if let Some(m) = mode {
                use cap_std::fs::PermissionsExt;
                f.set_permissions(cap_std::fs::Permissions::from_mode(m & 0o777))?;
            }
            #[cfg(not(unix))]
            let _ = mode;
            f.sync_all()?;
            Ok(())
        })();
        if let Err(e) = written {
            let _ = parent.remove_file(&tmp);
            return Err(format!("write failed: {}", describe(&e)));
        }
        if let Err(e) = parent.rename(&tmp, &parent, name) {
            let _ = parent.remove_file(&tmp);
            return Err(format!("replace failed: {}", describe(&e)));
        }
        Ok(())
    }

    /// Create a new file (O_EXCL + O_NOFOLLOW), creating missing parent
    /// directories through handles. Fails if anything — a file, a directory,
    /// a dangling symlink — already has the name. Returns the resolved path.
    pub fn create_new(&self, rel: &RelPath, bytes: &[u8]) -> Result<Option<String>, String> {
        let parent = self
            .open_chain(rel.parents(), true)?
            .ok_or("cannot create parent directories")?;
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true).follow(FollowSymlinks::No);
        let mut f = parent.open_with(rel.file_name(), &opts).map_err(|e| {
            if e.kind() == io::ErrorKind::AlreadyExists {
                "a file with this name appeared since the preview — re-parse the reply".to_string()
            } else {
                format!("create failed: {}", describe(&e))
            }
        })?;
        let resolved = self.resolved(os_path(&f))?;
        f.write_all(bytes)
            .and_then(|_| f.sync_all())
            .map_err(|e| format!("write failed: {}", describe(&e)))?;
        Ok(resolved)
    }

    /// Remove a regular file (never a directory; a symlink is refused).
    pub fn remove_file(&self, rel: &RelPath) -> Result<bool, String> {
        let Some(parent) = self.open_chain(rel.parents(), false)? else {
            return Ok(false);
        };
        match parent.symlink_metadata(rel.file_name()) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(format!("cannot stat: {}", describe(&e))),
            Ok(m) => {
                file_info(&m)?;
            }
        }
        parent
            .remove_file(rel.file_name())
            .map_err(|e| format!("delete failed: {}", describe(&e)))?;
        Ok(true)
    }

    /// Names of the plain (non-symlink) subdirectories of `rel`.
    pub fn subdirs(&self, rel: &RelPath) -> Result<Vec<String>, String> {
        let Some(dir) = self.open_dir(rel)? else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for entry in dir
            .entries()
            .map_err(|e| format!("cannot list directory: {}", describe(&e)))?
            .flatten()
        {
            let is_plain_dir = entry
                .file_type()
                .map(|t| t.is_dir() && !t.is_symlink())
                .unwrap_or(false);
            if is_plain_dir {
                out.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        out.sort();
        Ok(out)
    }

    /// Nested path helper for callers that build backup trees.
    pub fn rel(comps: &[&str]) -> RelPath {
        RelPath::from_comps(comps.iter().map(|s| s.to_string()).collect())
    }
}

fn file_info(m: &cap_std::fs::Metadata) -> Result<FileInfo, String> {
    let ft = m.file_type();
    if ft.is_symlink() {
        return Err("target is a symlink — apply-back never writes through links".into());
    }
    if ft.is_dir() {
        return Err("target is a directory".into());
    }
    if !ft.is_file() {
        return Err("target is not a regular file".into());
    }
    #[cfg(unix)]
    {
        use cap_std::fs::MetadataExt;
        let mode = m.mode() & 0o777;
        Ok(FileInfo {
            len: m.len(),
            mode: Some(mode),
            executable: mode & 0o111 != 0,
            extra_links: m.nlink().saturating_sub(1),
        })
    }
    #[cfg(not(unix))]
    {
        Ok(FileInfo {
            len: m.len(),
            mode: None,
            executable: false,
            extra_links: 0,
        })
    }
}

/// Component equality as the filesystem sees it: exact on Linux, ASCII
/// case-insensitive on the (default) case-insensitive macOS/Windows volumes.
fn same_component(a: &std::ffi::OsStr, b: &std::ffi::OsStr) -> bool {
    if cfg!(any(windows, target_os = "macos")) {
        a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
    } else {
        a == b
    }
}

/// The path the OS reports for an open handle, when the platform offers it.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn os_path<T: std::os::fd::AsRawFd>(h: &T) -> Option<PathBuf> {
    let p = std::fs::read_link(format!("/proc/self/fd/{}", h.as_raw_fd())).ok()?;
    // Unlinked or unreachable (other mount namespace) handles are not paths.
    if !p.is_absolute() || p.to_string_lossy().ends_with(" (deleted)") {
        return None;
    }
    Some(p)
}

#[cfg(target_os = "macos")]
fn os_path<T: std::os::fd::AsFd>(h: &T) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let c = rustix::fs::getpath(h).ok()?;
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(c.as_bytes())))
}

#[cfg(windows)]
fn os_path<T: std::os::windows::io::AsRawHandle>(h: &T) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFinalPathNameByHandleW, FILE_NAME_NORMALIZED, VOLUME_NAME_DOS,
    };
    let handle = h.as_raw_handle() as isize;
    let mut buf: Vec<u16> = vec![0; 512];
    loop {
        // SAFETY: `handle` is a live handle borrowed from `h`; `buf` is a
        // writable buffer of `buf.len()` u16s.
        let n = unsafe {
            GetFinalPathNameByHandleW(
                handle,
                buf.as_mut_ptr(),
                buf.len() as u32,
                FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
            )
        } as usize;
        if n == 0 {
            return None;
        }
        if n < buf.len() {
            buf.truncate(n);
            return Some(PathBuf::from(std::ffi::OsString::from_wide(&buf)));
        }
        buf.resize(n + 1, 0);
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    windows
)))]
fn os_path<T>(_h: &T) -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relpath_accepts_plain_paths() {
        assert_eq!(RelPath::parse("src/ok.rs").unwrap().display(), "src/ok.rs");
        assert_eq!(
            RelPath::parse("./src/ok.rs").unwrap().display(),
            "src/ok.rs"
        );
        assert_eq!(
            RelPath::parse("src\\win\\p.rs").unwrap().display(),
            "src/win/p.rs"
        );
        assert_eq!(RelPath::parse("a//b.rs").unwrap().display(), "a/b.rs");
        assert_eq!(
            RelPath::parse("My Dir/file name.rs").unwrap().display(),
            "My Dir/file name.rs"
        );
    }

    #[test]
    fn relpath_refuses_escapes_and_aliases() {
        for bad in [
            "",
            "../evil.rs",
            "src/../../evil.rs",
            "src/../ok.rs",
            "C:/evil.rs",
            "C:\\evil.rs",
            "/etc/passwd",
            "\\\\server\\share",
            "file.txt:stream",
            "docs/",
            "docs\\",
            "note.md.",
            "dir /x.rs",
            "a\nb.rs",
            ".",
        ] {
            assert!(RelPath::parse(bad).is_err(), "{bad:?} must be refused");
        }
        // Device names are only special on Windows.
        for name in ["CON", "aux.txt", "sub/nul.rs", "Lpt1.log"] {
            assert_eq!(RelPath::parse(name).is_err(), cfg!(windows), "{name:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_never_followed() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("target.txt"), "OUTSIDE\n").unwrap();
        symlink("../../outside/target.txt", root.join("docs/note.md")).unwrap();
        symlink("../outside", root.join("linkdir")).unwrap();
        symlink("docs", root.join("inner_link")).unwrap();

        let sr = SafeRoot::open(&root).unwrap();
        let note = RelPath::parse("docs/note.md").unwrap();
        assert!(sr.read(&note).unwrap_err().contains("symlink"));
        assert!(sr.stat(&note).unwrap_err().contains("symlink"));
        assert!(sr
            .replace(&note, b"x", None)
            .unwrap_err()
            .contains("symlink"));
        assert!(sr.create_new(&note, b"x").is_err());
        assert!(sr.remove_file(&note).is_err());
        // Directory links are refused whether they escape or stay inside.
        let via = RelPath::parse("linkdir/target.txt").unwrap();
        assert!(sr.read(&via).unwrap_err().contains("symlink"));
        assert!(sr
            .create_new(&RelPath::parse("linkdir/new.txt").unwrap(), b"x")
            .is_err());
        let inner = RelPath::parse("inner_link/new.md").unwrap();
        assert!(sr.create_new(&inner, b"x").unwrap_err().contains("symlink"));
        assert_eq!(
            std::fs::read_to_string(outside.join("target.txt")).unwrap(),
            "OUTSIDE\n"
        );
        assert!(!outside.join("new.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn replace_detaches_hard_links_and_keeps_mode() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let outside = tmp.path().join("outside.txt");
        std::fs::write(&outside, "shared\n").unwrap();
        std::fs::hard_link(&outside, root.join("linked.txt")).unwrap();
        std::fs::set_permissions(
            root.join("linked.txt"),
            std::fs::Permissions::from_mode(0o640),
        )
        .unwrap();

        let sr = SafeRoot::open(&root).unwrap();
        let rel = RelPath::parse("linked.txt").unwrap();
        let (_, info, resolved) = sr.read(&rel).unwrap().unwrap();
        assert_eq!(info.extra_links, 1);
        assert_eq!(info.mode, Some(0o640));
        assert_eq!(resolved.as_deref(), Some("linked.txt"));
        sr.replace(&rel, b"new\n", info.mode).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("linked.txt")).unwrap(),
            "new\n"
        );
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "shared\n");
        let mode = std::fs::metadata(root.join("linked.txt"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o640);
        // No temp files left behind.
        let names: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
    }

    #[test]
    fn create_new_makes_parents_and_refuses_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let sr = SafeRoot::open(&root).unwrap();
        let rel = RelPath::parse("a/b/c.txt").unwrap();
        let resolved = sr.create_new(&rel, b"hello\n").unwrap();
        if cfg!(target_os = "linux") {
            assert_eq!(resolved.as_deref(), Some("a/b/c.txt"));
        }
        assert_eq!(
            std::fs::read_to_string(root.join("a/b/c.txt")).unwrap(),
            "hello\n"
        );
        assert!(sr.create_new(&rel, b"again").is_err());
        assert!(sr.remove_file(&rel).unwrap());
        assert!(!sr.remove_file(&rel).unwrap());
    }
}
