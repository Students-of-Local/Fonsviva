//! Atomic, mode-preserving file writes with read-back, in the same shape as
//! Local Studio's `write_private`: a `create_new` temp file beside the target,
//! fsync, rename, fsync the directory.

use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

pub fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn read_opt(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// The permission bits of an existing file (Unix only).
pub fn mode_of(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path)
            .ok()
            .map(|meta| meta.permissions().mode() & 0o7777)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Creates `dir` and its parents; new directories get mode 0700 on Unix.
pub fn create_private_dir(dir: &Path) -> io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// Writes `bytes` to `path` atomically with `mode` (Unix), following a
/// symlink to its target, then reads the file back and checks the bytes.
pub fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> io::Result<PathBuf> {
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let parent = target.parent().map(Path::to_path_buf).unwrap_or_default();
    create_private_dir(&parent)?;
    let name = target
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let temporary = parent.join(format!(".{name}.{}.{nanos}.tmp", std::process::id()));
    let result = replace_file(&temporary, &target, bytes, mode);
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    if fs::read(&target)? != bytes {
        return Err(io::Error::other("read-back did not match what was written"));
    }
    Ok(target)
}

fn replace_file(temporary: &Path, target: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    let mut file = options.open(temporary)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(mode))?;
    }
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(temporary, target)?;
    #[cfg(unix)]
    if let Some(parent) = target.parent() {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

/// Appends one line to a file, creating it with mode 0600.
pub fn append_line(path: &Path, line: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        create_private_dir(parent)?;
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()
}
