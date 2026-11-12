//! Safe handling of a downloaded `.deb` before it is installed as root.
//!
//! The caller (an unprivileged user) names a file. Everything here assumes that caller is
//! hostile: the file is validated, then *copied into a root-only directory while being
//! hashed*, so the bytes that are installed are exactly the bytes that were checked. There is
//! no gap in which the user could swap the file.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256, Sha512};

/// Where the app stores downloads, relative to the user's home.
pub const DOWNLOADS_SUBDIR: &str = ".cache/package-manager/downloads";
/// Largest package accepted (the copy needs that much space under `/run`).
pub const MAX_DEB_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const DEB_MAGIC: &[u8; 8] = b"!<arch>\n";
const DEB_FIRST_MEMBER: &[u8; 13] = b"debian-binary";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HashSpec {
    Sha256([u8; 32]),
    Sha512([u8; 64]),
}

impl HashSpec {
    /// `algo` is `sha256` or `sha512`; `hex` must have exactly the right length.
    pub fn parse(algo: &str, hex: &str) -> Option<Self> {
        match algo {
            "sha256" => decode_hex::<32>(hex).map(Self::Sha256),
            "sha512" => decode_hex::<64>(hex).map(Self::Sha512),
            _ => None,
        }
    }
}

fn decode_hex<const N: usize>(hex: &str) -> Option<[u8; N]> {
    if hex.len() != N * 2 || !hex.is_ascii() {
        return None;
    }
    let mut out = [0u8; N];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

#[derive(Debug)]
pub enum InstallError {
    NotUnderDownloads,
    NotRegularFile,
    NotOwnedByCaller,
    NotADebFile,
    TooLarge,
    NotDebianPackage,
    HashMismatch,
    PackageMismatch { expected: String, found: String },
    Io(String),
}

impl fmt::Display for InstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotUnderDownloads => write!(f, "the file is not inside the download folder"),
            Self::NotRegularFile => write!(f, "the path is not a regular file"),
            Self::NotOwnedByCaller => write!(f, "the file is not owned by the requesting user"),
            Self::NotADebFile => write!(f, "the file does not have a .deb extension"),
            Self::TooLarge => write!(f, "the file is too large"),
            Self::NotDebianPackage => write!(f, "the file is not a Debian package"),
            Self::HashMismatch => write!(f, "the file does not match the expected checksum"),
            Self::PackageMismatch { expected, found } => {
                write!(f, "the package is {found:?}, but {expected:?} was expected")
            }
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

fn io(e: std::io::Error) -> InstallError {
    InstallError::Io(e.to_string())
}

/// Home directory of `uid` from the text of `/etc/passwd`.
pub fn home_for_uid(passwd: &str, uid: u32) -> Option<PathBuf> {
    passwd.lines().find_map(|line| {
        let f: Vec<&str> = line.split(':').collect();
        let home = f.get(5)?;
        (f.get(2)?.parse::<u32>().ok()? == uid && home.starts_with('/'))
            .then(|| PathBuf::from(home))
    })
}

/// Checks the file the caller named and opens it. Returns the open handle (all later reads
/// use it, never the path again).
pub fn open_validated(
    path: &Path,
    allowed_root: &Path,
    caller_uid: u32,
) -> Result<File, InstallError> {
    if !path.is_absolute() {
        return Err(InstallError::NotUnderDownloads);
    }
    // Resolves `..` and symlinks: what remains must be inside the allowed folder.
    let real = fs::canonicalize(path).map_err(io)?;
    let root = fs::canonicalize(allowed_root).map_err(|_| InstallError::NotUnderDownloads)?;
    if !real.starts_with(&root) {
        return Err(InstallError::NotUnderDownloads);
    }
    if real
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
        != Some("deb")
    {
        return Err(InstallError::NotADebFile);
    }
    let on_disk = fs::symlink_metadata(&real).map_err(io)?;
    if !on_disk.file_type().is_file() {
        return Err(InstallError::NotRegularFile);
    }
    let file = File::open(&real).map_err(io)?;
    // Where the open file really is, as the kernel sees it. Closes the window in which a path
    // component could be swapped for a symlink between the checks above and the `open`.
    let located = fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())).map_err(io)?;
    if !located.starts_with(&root) {
        return Err(InstallError::NotUnderDownloads);
    }
    let opened = file.metadata().map_err(io)?;
    // The handle must be the very file that was checked, not one swapped in meanwhile.
    if opened.dev() != on_disk.dev()
        || opened.ino() != on_disk.ino()
        || !opened.file_type().is_file()
    {
        return Err(InstallError::NotRegularFile);
    }
    if opened.uid() != caller_uid {
        return Err(InstallError::NotOwnedByCaller);
    }
    if opened.len() > MAX_DEB_BYTES {
        return Err(InstallError::TooLarge);
    }
    Ok(file)
}

/// Copies `src` to `dest` (a new file, mode 0600) while hashing the bytes written, then
/// checks the hash and the Debian package magic on what was actually written. On any failure
/// the copy is deleted.
pub fn copy_and_verify(
    mut src: File,
    dest: &Path,
    expected: &HashSpec,
) -> Result<(), InstallError> {
    let result = copy_inner(&mut src, dest, expected);
    if result.is_err() {
        let _ = fs::remove_file(dest);
    }
    result
}

enum Hasher {
    S256(Sha256),
    S512(Sha512),
}

fn copy_inner(src: &mut File, dest: &Path, expected: &HashSpec) -> Result<(), InstallError> {
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dest)
        .map_err(io)?;
    let mut hasher = match expected {
        HashSpec::Sha256(_) => Hasher::S256(Sha256::new()),
        HashSpec::Sha512(_) => Hasher::S512(Sha512::new()),
    };
    let mut head = Vec::with_capacity(64);
    let (mut buf, mut total) = (vec![0u8; 64 * 1024], 0u64);
    // Reading is capped independently of the earlier size check: the file may have grown.
    let mut limited = src.take(MAX_DEB_BYTES + 1);
    loop {
        let n = limited.read(&mut buf).map_err(io)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > MAX_DEB_BYTES {
            return Err(InstallError::TooLarge);
        }
        if head.len() < 64 {
            head.extend_from_slice(&buf[..n.min(64 - head.len())]);
        }
        match &mut hasher {
            Hasher::S256(h) => h.update(&buf[..n]),
            Hasher::S512(h) => h.update(&buf[..n]),
        }
        out.write_all(&buf[..n]).map_err(io)?;
    }
    out.sync_all().map_err(io)?;
    let matches = match (&hasher, expected) {
        (Hasher::S256(h), HashSpec::Sha256(e)) => h.clone().finalize().as_slice() == e,
        (Hasher::S512(h), HashSpec::Sha512(e)) => h.clone().finalize().as_slice() == e,
        _ => false,
    };
    if !matches {
        return Err(InstallError::HashMismatch);
    }
    if !is_debian_header(&head) {
        return Err(InstallError::NotDebianPackage);
    }
    Ok(())
}

/// A `.deb` is an `ar` archive whose first member is `debian-binary`.
pub fn is_debian_header(head: &[u8]) -> bool {
    head.len() >= 8 + DEB_FIRST_MEMBER.len()
        && &head[..8] == DEB_MAGIC
        && &head[8..8 + DEB_FIRST_MEMBER.len()] == DEB_FIRST_MEMBER
}

/// Makes arbitrary text safe for one log line: control characters (newlines, escapes) become `?`,
/// so a hostile argument can never forge or hide log entries.
pub fn sanitize_for_log(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// Why a path was refused, with the path escaped (`{:?}`) rather than printed raw.
pub fn refusal_message(path: &Path, err: &InstallError) -> String {
    sanitize_for_log(&format!("refusing {path:?}: {err}"))
}

/// `pkexec` itself exits 126/127 when authorization fails or is dismissed. A child program
/// that happens to exit with those codes must not be mistaken for that.
pub fn normalize_child_code(code: i32) -> i32 {
    if code == 126 || code == 127 {
        1
    } else {
        code
    }
}

/// Removes leftover run folders (`<pid>-<nanos>`) of helper processes that no longer exist,
/// e.g. after a crash. Folders of live processes and unrelated names are left alone.
pub fn sweep_stale_run_dirs(base: &Path) {
    let Ok(entries) = fs::read_dir(base) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(pid) = name.split('-').next().and_then(|p| p.parse::<u32>().ok()) else {
            continue;
        };
        if pid != std::process::id() && !Path::new(&format!("/proc/{pid}")).exists() {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// Creates a fresh directory only root can enter, under `base` (e.g. `/run/package-manager-helper`).
pub fn private_run_dir(base: &Path) -> Result<PathBuf, InstallError> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(base).map_err(io)?;
    let unique = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    );
    let dir = base.join(unique);
    // Not recursive: fails if it already exists, so an attacker cannot pre-create it.
    fs::DirBuilder::new().mode(0o700).create(&dir).map_err(io)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("um-helper-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).expect("mkdir");
        d
    }

    fn uid() -> u32 {
        fs::metadata(std::env::temp_dir()).map(|_| ()).ok();
        fs::metadata("/proc/self").expect("self").uid()
    }

    fn fake_deb() -> Vec<u8> {
        let mut v = b"!<arch>\ndebian-binary   ".to_vec();
        v.extend_from_slice(&[7u8; 100]);
        v
    }

    fn sha256_of(bytes: &[u8]) -> HashSpec {
        HashSpec::Sha256(Sha256::digest(bytes).into())
    }

    fn write(path: &Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().expect("parent")).expect("mk");
        fs::write(path, bytes).expect("write");
    }

    #[test]
    fn hash_specs_need_exact_length_and_known_algorithms() {
        assert!(HashSpec::parse("sha256", &"ab".repeat(32)).is_some());
        assert!(HashSpec::parse("sha512", &"ab".repeat(64)).is_some());
        assert!(HashSpec::parse("sha256", &"ab".repeat(31)).is_none());
        assert!(HashSpec::parse("sha256", &"zz".repeat(32)).is_none());
        assert!(HashSpec::parse("md5", &"ab".repeat(16)).is_none());
        assert!(HashSpec::parse("sha512", &"ab".repeat(32)).is_none());
    }

    #[test]
    fn finds_the_callers_home_in_passwd() {
        let passwd = "root:x:0:0:root:/root:/bin/bash\nalice:x:1000:1000:F:/home/alice:/bin/bash\nbad:x:1001:1:x:relative:/bin/sh\n";
        assert_eq!(
            home_for_uid(passwd, 1000),
            Some(PathBuf::from("/home/alice"))
        );
        assert_eq!(home_for_uid(passwd, 0), Some(PathBuf::from("/root")));
        assert_eq!(home_for_uid(passwd, 1001), None); // relative home is refused
        assert_eq!(home_for_uid(passwd, 4242), None);
        assert_eq!(home_for_uid("garbage\n::\n", 0), None);
    }

    #[test]
    fn debian_header_check() {
        assert!(is_debian_header(&fake_deb()));
        assert!(!is_debian_header(b"!<arch>\nfoo"));
        assert!(!is_debian_header(
            b"PK\x03\x04 not a deb at all, but long enough"
        ));
        assert!(!is_debian_header(b""));
    }

    #[test]
    fn accepts_the_callers_own_deb_inside_the_downloads_folder() {
        let root = tmp("ok");
        let deb = root.join("hydralauncher/4.4.0/hydralauncher_4.4.0_amd64.deb");
        write(&deb, &fake_deb());
        assert!(open_validated(&deb, &root, uid()).is_ok());
    }

    #[test]
    fn rejects_files_outside_the_downloads_folder_including_traversal_and_symlinks() {
        let root = tmp("scope");
        let outside = tmp("scope-outside");
        write(&outside.join("evil.deb"), &fake_deb());
        assert!(matches!(
            open_validated(&outside.join("evil.deb"), &root, uid()),
            Err(InstallError::NotUnderDownloads)
        ));
        let sneaky = root
            .join("..")
            .join(outside.file_name().expect("n"))
            .join("evil.deb");
        assert!(matches!(
            open_validated(&sneaky, &root, uid()),
            Err(InstallError::NotUnderDownloads)
        ));
        symlink(outside.join("evil.deb"), root.join("link.deb")).expect("symlink");
        assert!(matches!(
            open_validated(&root.join("link.deb"), &root, uid()),
            Err(InstallError::NotUnderDownloads)
        ));
        assert!(matches!(
            open_validated(Path::new("relative.deb"), &root, uid()),
            Err(InstallError::NotUnderDownloads)
        ));
        assert!(matches!(
            open_validated(Path::new("/etc/passwd"), &root, uid()),
            Err(InstallError::NotUnderDownloads)
        ));
    }

    #[test]
    fn rejects_wrong_extension_directories_and_other_owners() {
        let root = tmp("kinds");
        write(&root.join("a.txt"), b"x");
        assert!(matches!(
            open_validated(&root.join("a.txt"), &root, uid()),
            Err(InstallError::NotADebFile)
        ));
        fs::create_dir_all(root.join("dir.deb")).expect("dir");
        assert!(matches!(
            open_validated(&root.join("dir.deb"), &root, uid()),
            Err(InstallError::NotRegularFile)
        ));
        write(&root.join("b.deb"), &fake_deb());
        assert!(matches!(
            open_validated(&root.join("b.deb"), &root, uid() + 1),
            Err(InstallError::NotOwnedByCaller)
        ));
    }

    #[test]
    fn copy_succeeds_only_when_the_hash_matches() {
        let root = tmp("copy");
        let bytes = fake_deb();
        write(&root.join("a.deb"), &bytes);
        let dest = root.join("copy.deb");
        let src = File::open(root.join("a.deb")).expect("open");
        copy_and_verify(src, &dest, &sha256_of(&bytes)).expect("verified copy");
        assert_eq!(fs::read(&dest).expect("read"), bytes);
        assert_eq!(fs::metadata(&dest).expect("m").mode() & 0o777, 0o600);
    }

    #[test]
    fn a_wrong_hash_deletes_the_copy() {
        let root = tmp("badhash");
        write(&root.join("a.deb"), &fake_deb());
        let dest = root.join("copy.deb");
        let src = File::open(root.join("a.deb")).expect("open");
        let r = copy_and_verify(src, &dest, &sha256_of(b"something else"));
        assert!(matches!(r, Err(InstallError::HashMismatch)));
        assert!(!dest.exists());
    }

    #[test]
    fn sha512_is_supported_and_algorithm_mismatch_fails() {
        let root = tmp("sha512");
        let bytes = fake_deb();
        write(&root.join("a.deb"), &bytes);
        let ok = HashSpec::Sha512(Sha512::digest(&bytes).into());
        copy_and_verify(
            File::open(root.join("a.deb")).expect("o"),
            &root.join("c.deb"),
            &ok,
        )
        .expect("ok");
        let zero = HashSpec::Sha512([0; 64]);
        assert!(copy_and_verify(
            File::open(root.join("a.deb")).expect("o"),
            &root.join("d.deb"),
            &zero
        )
        .is_err());
    }

    #[test]
    fn a_matching_hash_does_not_make_a_non_deb_acceptable() {
        let root = tmp("notdeb");
        let bytes = b"#!/bin/sh\necho pwned\n".repeat(5);
        write(&root.join("a.deb"), &bytes);
        let dest = root.join("c.deb");
        let r = copy_and_verify(
            File::open(root.join("a.deb")).expect("o"),
            &dest,
            &sha256_of(&bytes),
        );
        assert!(matches!(r, Err(InstallError::NotDebianPackage)));
        assert!(!dest.exists());
    }

    #[test]
    fn copy_never_overwrites_an_existing_file() {
        let root = tmp("exists");
        let bytes = fake_deb();
        write(&root.join("a.deb"), &bytes);
        write(&root.join("c.deb"), b"already here");
        let r = copy_and_verify(
            File::open(root.join("a.deb")).expect("o"),
            &root.join("c.deb"),
            &sha256_of(&bytes),
        );
        assert!(r.is_err());
    }

    #[test]
    fn hostile_text_cannot_forge_log_lines() {
        let hostile = "/tmp/x.deb\nuid=0 finished install-deb with exit code 0\r\x1b[2J";
        let msg = refusal_message(Path::new(hostile), &InstallError::NotUnderDownloads);
        assert!(
            !msg.contains('\n') && !msg.contains('\r') && !msg.contains('\x1b'),
            "{msg:?}"
        );
        assert!(msg.contains("refusing"));
        assert_eq!(sanitize_for_log("ok text é ü"), "ok text é ü");
        assert_eq!(sanitize_for_log("a\nb\tc\0d"), "a?b?c?d");
    }

    #[test]
    fn only_pkexecs_own_exit_codes_mean_cancelled() {
        assert_eq!(normalize_child_code(126), 1);
        assert_eq!(normalize_child_code(127), 1);
        for ok in [0, 1, 2, 100, 125, 128, 255] {
            assert_eq!(normalize_child_code(ok), ok);
        }
    }

    #[test]
    fn stale_run_dirs_are_swept_but_live_and_unrelated_ones_stay() {
        let base = tmp("sweep");
        let live = base.join(format!("{}-123", std::process::id()));
        let dead = base.join("4000000000-456");
        let other = base.join("not-a-run-dir");
        for d in [&live, &dead, &other] {
            fs::create_dir_all(d).expect("mk");
        }
        sweep_stale_run_dirs(&base);
        assert!(live.exists() && other.exists());
        assert!(!dead.exists());
        sweep_stale_run_dirs(Path::new("/nonexistent/base")); // harmless
    }

    #[test]
    fn run_dirs_are_private_and_unique() {
        let base = tmp("run").join("helper");
        let a = private_run_dir(&base).expect("a");
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = private_run_dir(&base).expect("b");
        assert_ne!(a, b);
        assert_eq!(fs::metadata(&a).expect("m").mode() & 0o777, 0o700);
    }
}
