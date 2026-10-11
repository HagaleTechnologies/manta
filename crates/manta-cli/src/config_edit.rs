//! MAN-127: sets `[input].freq_correction_ppm` in an existing config file
//! while keeping every other byte, then validates the result with the real
//! loader and atomically replaces the file.

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, ensure, Context, Result};
use toml_edit::{DocumentMut, Item, Table, Value};

const KEY: &str = "freq_correction_ppm";
const BOM: char = '\u{feff}';

/// Pure: returns the new text and the previous value (None when unset).
pub(crate) fn set_freq_correction_ppm(text: &str, ppm: f64) -> Result<(String, Option<f64>)> {
    ensure!(ppm.is_finite(), "{KEY} must be a finite number, got {ppm}");
    // -0.0 would print as `-0.0`.
    let ppm = if ppm == 0.0 { 0.0 } else { ppm };
    let (bom, body) = match text.strip_prefix(BOM) {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let mut doc: DocumentMut = body.parse().context("parsing the config file")?;
    if !doc.contains_key("input") {
        doc.insert("input", Item::Table(Table::new()));
    }
    let previous = match doc.get_mut("input") {
        Some(Item::Table(table)) => set_in_table(table, ppm)?,
        Some(Item::Value(Value::InlineTable(table))) => match table.get_mut(KEY) {
            Some(old) => replace(old, ppm)?,
            None => {
                let mut new = Value::from(ppm);
                // The space before `}` sits on the last value; move it to
                // the new one so the old value doesn't print `"h" ,`.
                if let Some((_, last)) = table.iter_mut().last() {
                    if let Some(suffix) = last.decor().suffix().cloned() {
                        last.decor_mut().set_suffix("");
                        new.decor_mut().set_suffix(suffix);
                    }
                }
                table.insert(KEY, new);
                None
            }
        },
        _ => bail!("[input] is not a table, so {KEY} cannot be set in it"),
    };
    let mut out = doc.to_string();
    if bom {
        out.insert(0, BOM);
    }
    Ok((out, previous))
}

fn set_in_table(table: &mut Table, ppm: f64) -> Result<Option<f64>> {
    match table.get_mut(KEY) {
        Some(Item::Value(old)) => replace(old, ppm),
        Some(_) => bail!("[input].{KEY} is not a number"),
        None => {
            table.insert(KEY, Item::Value(Value::from(ppm)));
            Ok(None)
        }
    }
}

/// Swaps `old` for `ppm`, keeping its surrounding whitespace and trailing
/// comment (a plain assignment would drop them).
fn replace(old: &mut Value, ppm: f64) -> Result<Option<f64>> {
    let previous = match old {
        Value::Float(f) => *f.value(),
        Value::Integer(i) => *i.value() as f64,
        _ => bail!("[input].{KEY} is not a number"),
    };
    let decor = old.decor().clone();
    let mut new = Value::from(ppm);
    *new.decor_mut() = decor;
    *old = new;
    Ok(Some(previous))
}

/// The value as `set_freq_correction_ppm` writes it, e.g. `-1.42`.
fn literal(ppm: f64) -> String {
    let ppm = if ppm == 0.0 { 0.0 } else { ppm };
    Value::from(ppm).to_string().trim().to_string()
}

/// Removes the temp file unless the save completed.
struct TempGuard {
    path: PathBuf,
    armed: bool,
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// A config file as calibrate read it before measuring.
#[derive(Debug, PartialEq)]
pub(crate) struct ReadBefore {
    /// `path` canonicalized.
    pub(crate) target: PathBuf,
    pub(crate) text: String,
}

impl ReadBefore {
    pub(crate) fn read(path: &Path) -> Result<Self> {
        let target = fs::canonicalize(path)
            .with_context(|| format!("cannot resolve config file {}", path.display()))?;
        let text = fs::read_to_string(&target)
            .with_context(|| format!("reading config file {}", path.display()))?;
        Ok(Self { target, text })
    }
}

/// Saves to the file at `path` (following symlinks), returns the previous value.
/// With `expected`, refuses unless `path` still resolves to that canonical
/// target holding exactly that text: `calibrate` passes what it read before
/// measuring, so a config rewritten or retargeted during the measurement
/// (possibly describing another receiver) is never given that measurement.
/// The edited file is validated with this process's `MANTA_*` overlay (a
/// file can rely on it, as `run` and `calibrate` do), minus the ppm
/// variable, which would mask the value just written.
pub(crate) fn save_freq_correction_ppm(
    path: &Path,
    ppm: f64,
    expected: Option<&ReadBefore>,
) -> Result<Option<f64>> {
    let vars: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    save_with_env(path, ppm, expected, &vars)
}

const PPM_ENV_VAR: &str = "MANTA_INPUT_FREQ_CORRECTION_PPM";

fn save_with_env(
    path: &Path,
    ppm: f64,
    expected: Option<&ReadBefore>,
    vars: &[(OsString, OsString)],
) -> Result<Option<f64>> {
    let target = fs::canonicalize(path)
        .with_context(|| format!("cannot resolve config file {}", path.display()))?;
    let bytes =
        fs::read(&target).with_context(|| format!("reading config file {}", target.display()))?;
    let text = String::from_utf8(bytes)
        .map_err(|e| anyhow!("config file {} is not valid UTF-8: {e}", target.display()))?;
    if let Some(expected) = expected {
        ensure!(
            target == expected.target,
            "{} left unchanged: it now resolves to {}, not {}, which may describe another \
             receiver than the one measured; re-run calibrate",
            path.display(),
            target.display(),
            expected.target.display()
        );
        ensure!(
            text == expected.text,
            "{} left unchanged: it changed after calibrate read it, so it may no longer \
             describe the receiver that was measured; re-run calibrate",
            target.display()
        );
    }
    let (new_text, previous) =
        set_freq_correction_ppm(&text, ppm).with_context(|| target.display().to_string())?;
    let ppm = if ppm == 0.0 { 0.0 } else { ppm };
    let meta = fs::metadata(&target)
        .with_context(|| format!("reading the metadata of {}", target.display()))?;

    let dir = target
        .parent()
        .ok_or_else(|| anyhow!("config file {} has no parent directory", target.display()))?;
    let name = target
        .file_name()
        .ok_or_else(|| anyhow!("config file {} has no file name", target.display()))?;
    let temp = dir.join(format!(
        ".{}.calibrate-{}.tmp",
        name.to_string_lossy(),
        std::process::id()
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    // Owner-only until the original's owner and mode are copied over: the
    // config can hold a receiver password or uplink credentials, and a
    // umask-default temp file would expose them to other local users.
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    // WRITE_DAC, so keep_dacl can give the file the original's DACL, and no
    // sharing, so no one else opens it while it has the directory's.
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::{Foundation::GENERIC_WRITE, Storage::FileSystem::WRITE_DAC};
        options.access_mode(GENERIC_WRITE | WRITE_DAC).share_mode(0);
    }
    let mut file = options
        .open(&temp)
        .with_context(|| format!("creating temp file {}", temp.display()))?;
    let mut guard = TempGuard {
        path: temp.clone(),
        armed: true,
    };
    #[cfg(windows)]
    keep_dacl(&target, &file, ppm)?;
    #[cfg(target_os = "macos")]
    keep_acl(&target, &file, ppm)?;
    file.write_all(new_text.as_bytes())
        .and_then(|()| file.sync_all())
        .with_context(|| format!("writing temp file {}", temp.display()))?;
    #[cfg(unix)]
    keep_owner(&target, &meta, &temp, ppm)?;
    #[cfg(target_os = "linux")]
    keep_acl(&target, &file, ppm)?;
    // Through the open handle: on Windows it is unshared, so a path-based
    // call could be refused a second handle.
    file.set_permissions(meta.permissions())
        .with_context(|| format!("setting the permissions of {}", temp.display()))?;
    drop(file);

    let vars: Vec<(OsString, OsString)> = vars
        .iter()
        .filter(|(k, _)| k != PPM_ENV_VAR)
        .cloned()
        .collect();
    let loaded =
        crate::config::load(Some(&temp), crate::config::Env::Read(&vars)).with_context(|| {
            format!(
                "{} left unchanged: the edited file does not load",
                target.display()
            )
        })?;
    let got = loaded.input.shared.freq_correction_ppm;
    ensure!(
        got == Some(ppm),
        "{} left unchanged: the edited file reads {KEY} = {got:?}, expected {}",
        target.display(),
        literal(ppm)
    );
    // Re-check right before the replace, so an edit made while the temp
    // file was written and validated is not silently overwritten. (Not a
    // lock: a writer racing this last read can still lose; the window is
    // the gap between this read and the rename.)
    let current =
        fs::read(&target).with_context(|| format!("reading config file {}", target.display()))?;
    ensure!(
        current == text.as_bytes(),
        "{} left unchanged: it changed while calibrate was saving; set {KEY} = {} by hand",
        target.display(),
        literal(ppm)
    );
    fs::rename(&temp, &target)
        .with_context(|| format!("replacing {} with {}", target.display(), temp.display()))?;
    guard.armed = false;
    #[cfg(unix)]
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(previous)
}

#[cfg(unix)]
fn keep_owner(target: &Path, meta: &fs::Metadata, temp: &Path, ppm: f64) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let (uid, gid) = (meta.uid(), meta.gid());
    let temp_meta = fs::metadata(temp)
        .with_context(|| format!("reading the metadata of {}", temp.display()))?;
    if temp_meta.uid() == uid && temp_meta.gid() == gid {
        return Ok(());
    }
    std::os::unix::fs::chown(temp, Some(uid), Some(gid)).map_err(|e| {
        anyhow!(
            "cannot keep {}'s owner (uid {uid}, gid {gid}): {e}; run calibrate as that user, \
             or set {KEY} = {} by hand",
            target.display(),
            literal(ppm)
        )
    })
}

/// Gives the temp file the original's POSIX access ACL, or none when the
/// original has none. A new file takes its directory's default ACL, whose
/// named entries the 0600 create mode masks; `set_permissions` would then
/// unmask them, because the ACL mask is the mode's group bits. ENOTSUP means
/// the filesystem has no POSIX ACLs; other ACL kinds, such as NFSv4's, are
/// not copied.
#[cfg(target_os = "linux")]
fn keep_acl(target: &Path, temp: &fs::File, ppm: f64) -> Result<()> {
    use std::io::Error;
    use std::os::unix::io::AsRawFd;
    const NAME: &std::ffi::CStr = c"system.posix_acl_access";
    let fail = |e: Error| {
        anyhow!(
            "cannot keep {}'s permissions: {e}; set {KEY} = {} by hand",
            target.display(),
            literal(ppm)
        )
    };
    let original = fs::File::open(target).map_err(fail)?;
    let (from, to) = (original.as_raw_fd(), temp.as_raw_fd());
    let errno = |e: &Error| e.raw_os_error().unwrap_or(0);
    // SAFETY: a live fd, a NUL-terminated name, and a zero-length buffer,
    // which asks for the ACL's size.
    let len = unsafe { libc::fgetxattr(from, NAME.as_ptr(), std::ptr::null_mut(), 0) };
    let result = if len < 0 {
        match Error::last_os_error() {
            e if errno(&e) == libc::ENOTSUP => Ok(()),
            e if errno(&e) == libc::ENODATA => {
                // SAFETY: a live fd and a NUL-terminated name.
                match unsafe { libc::fremovexattr(to, NAME.as_ptr()) } {
                    0 => Ok(()),
                    _ => match Error::last_os_error() {
                        e if errno(&e) == libc::ENODATA => Ok(()),
                        e => Err(e),
                    },
                }
            }
            e => Err(e),
        }
    } else {
        let mut acl = vec![0u8; len as usize];
        // SAFETY: a live fd, a NUL-terminated name, and a writable buffer of
        // `acl.len()` bytes.
        let n = unsafe { libc::fgetxattr(from, NAME.as_ptr(), acl.as_mut_ptr().cast(), acl.len()) };
        if n < 0 {
            Err(Error::last_os_error())
        } else {
            // SAFETY: a live fd, a NUL-terminated name, and the first `n`
            // bytes of `acl`, which fgetxattr filled.
            match unsafe { libc::fsetxattr(to, NAME.as_ptr(), acl.as_ptr().cast(), n as usize, 0) }
            {
                0 => Ok(()),
                _ => Err(Error::last_os_error()),
            }
        }
    };
    result.map_err(fail)
}

/// Gives the still-empty temp file the original's extended ACL, or an empty
/// one when the original has none. A new file inherits its directory's
/// inheritable ACEs, which can grant principals the config's own ACL and
/// mode exclude, and `set_permissions` carries only the mode bits. ENOTSUP
/// means the filesystem has no extended ACLs.
#[cfg(target_os = "macos")]
fn keep_acl(target: &Path, temp: &fs::File, ppm: f64) -> Result<()> {
    use std::ffi::{c_int, c_void};
    use std::io::Error;
    use std::os::unix::io::AsRawFd;
    // <sys/acl.h>; libc does not bind these.
    const ACL_TYPE_EXTENDED: c_int = 0x0000_0100;
    extern "C" {
        fn acl_get_fd_np(fd: c_int, ty: c_int) -> *mut c_void;
        fn acl_set_fd_np(fd: c_int, acl: *mut c_void, ty: c_int) -> c_int;
        fn acl_init(count: c_int) -> *mut c_void;
        fn acl_free(obj: *mut c_void) -> c_int;
    }
    let fail = |e: Error| {
        anyhow!(
            "cannot keep {}'s permissions: {e}; set {KEY} = {} by hand",
            target.display(),
            literal(ppm)
        )
    };
    let errno = |e: &Error| e.raw_os_error().unwrap_or(0);
    let original = fs::File::open(target).map_err(fail)?;
    // SAFETY: a live fd. A non-null result is an acl_t we own and free below.
    let mut acl = unsafe { acl_get_fd_np(original.as_raw_fd(), ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        match Error::last_os_error() {
            e if errno(&e) == libc::ENOTSUP => return Ok(()),
            // No ACL on the original: give the temp file an empty one.
            e if errno(&e) == libc::ENOENT => {
                // SAFETY: allocates an empty acl_t we own and free below.
                acl = unsafe { acl_init(0) };
                if acl.is_null() {
                    return Err(fail(Error::last_os_error()));
                }
            }
            e => return Err(fail(e)),
        }
    }
    // SAFETY: a live fd and the acl_t from above, freed once after use.
    let result = unsafe {
        let r = match acl_set_fd_np(temp.as_raw_fd(), acl, ACL_TYPE_EXTENDED) {
            0 => Ok(()),
            _ => match Error::last_os_error() {
                e if errno(&e) == libc::ENOTSUP => Ok(()),
                e => Err(e),
            },
        };
        acl_free(acl);
        r
    };
    result.map_err(fail)
}

/// Gives the still-empty temp file the original's DACL. On Windows a new
/// file takes its directory's inheritable ACL, which can be broader than
/// the config's own, and `set_permissions` carries only the read-only
/// attribute. A DACL that inherits keeps inheriting, from the same
/// directory; a protected one is copied as it is.
#[cfg(windows)]
fn keep_dacl(target: &Path, temp: &fs::File, ppm: f64) -> Result<()> {
    use std::io::Error;
    use std::os::windows::io::AsRawHandle;
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::{LocalFree, ERROR_SUCCESS};
    use windows_sys::Win32::Security::Authorization::{
        GetSecurityInfo, SetSecurityInfo, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{
        GetSecurityDescriptorControl, ACL, DACL_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED,
        UNPROTECTED_DACL_SECURITY_INFORMATION,
    };
    let fail = |e: Error| {
        anyhow!(
            "cannot keep {}'s permissions: {e}; set {KEY} = {} by hand",
            target.display(),
            literal(ppm)
        )
    };
    let original = fs::File::open(target).map_err(fail)?;
    let mut dacl: *mut ACL = null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: a live handle and out-pointers to locals. On success `sd` is
    // a LocalAlloc'd descriptor that `dacl` points into.
    let err = unsafe {
        GetSecurityInfo(
            original.as_raw_handle(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut sd,
        )
    };
    if err != ERROR_SUCCESS {
        return Err(fail(Error::from_raw_os_error(err as i32)));
    }
    let (mut control, mut revision) = (0u16, 0u32);
    // SAFETY: `sd` is the descriptor GetSecurityInfo returned, and `dacl`
    // stays valid until `sd` is freed, below; `temp` is a live handle
    // opened with WRITE_DAC.
    let result = unsafe {
        if GetSecurityDescriptorControl(sd, &mut control, &mut revision) == 0 {
            Err(Error::last_os_error())
        } else {
            let inheritance = if control & SE_DACL_PROTECTED != 0 {
                PROTECTED_DACL_SECURITY_INFORMATION
            } else {
                UNPROTECTED_DACL_SECURITY_INFORMATION
            };
            match SetSecurityInfo(
                temp.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | inheritance,
                null_mut(),
                null_mut(),
                dacl,
                null(),
            ) {
                ERROR_SUCCESS => Ok(()),
                e => Err(Error::from_raw_os_error(e as i32)),
            }
        }
    };
    // SAFETY: `sd` came from GetSecurityInfo and is freed once.
    unsafe { LocalFree(sd) };
    result.map_err(fail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{load, Env};

    fn set(text: &str, ppm: f64) -> (String, Option<f64>) {
        set_freq_correction_ppm(text, ppm).unwrap()
    }

    #[test]
    fn replaces_an_existing_value_and_keeps_its_comment() {
        let text =
            "# top\n[input]\ntype = \"kiwi\"\nfreq_correction_ppm = 0.5   # tuned\nhost = \"h\"\n";
        let (out, previous) = set(text, -1.42);
        assert_eq!(previous, Some(0.5));
        assert_eq!(
            out,
            "# top\n[input]\ntype = \"kiwi\"\nfreq_correction_ppm = -1.42   # tuned\nhost = \"h\"\n"
        );
    }

    #[test]
    fn reads_an_integer_previous_value() {
        let (out, previous) = set("[input]\nfreq_correction_ppm = 2\n", 2.5);
        assert_eq!(previous, Some(2.0));
        assert_eq!(out, "[input]\nfreq_correction_ppm = 2.5\n");
    }

    #[test]
    fn adds_the_key_under_an_existing_input_table() {
        let scaffold = include_str!("config_init.toml");
        let (out, previous) = set(scaffold, -1.42);
        assert_eq!(previous, None);
        let before: Vec<&str> = scaffold.lines().collect();
        let after: Vec<&str> = out.lines().collect();
        let at = before.iter().position(|l| *l == "[input]").unwrap() + 1;
        assert_eq!(after.len(), before.len() + 1);
        assert_eq!(after[..at], before[..at]);
        assert_eq!(after[at], "freq_correction_ppm = -1.42");
        assert_eq!(after[at + 1..], before[at..]);
        assert_eq!(out.ends_with('\n'), scaffold.ends_with('\n'));
    }

    #[test]
    fn appends_an_input_table_when_missing() {
        let (out, previous) = set("# c\n[spot]\nallowlist = []\n", 3.0);
        assert_eq!(previous, None);
        assert_eq!(
            out,
            "# c\n[spot]\nallowlist = []\n\n[input]\nfreq_correction_ppm = 3.0\n"
        );
        let (empty, _) = set("", 3.0);
        assert_eq!(empty, "[input]\nfreq_correction_ppm = 3.0\n");
    }

    #[test]
    fn edits_an_inline_input_table() {
        let (out, previous) = set("input = { type = \"kiwi\", host = \"h\" }  # rx\n", -1.42);
        assert_eq!(previous, None);
        assert_eq!(
            out,
            "input = { type = \"kiwi\", host = \"h\", freq_correction_ppm = -1.42 }  # rx\n"
        );
        let (out, previous) = set("input = { freq_correction_ppm = 1.0 }\n", -1.42);
        assert_eq!(previous, Some(1.0));
        assert_eq!(out, "input = { freq_correction_ppm = -1.42 }\n");
    }

    #[test]
    fn edits_dotted_input_keys() {
        let text = "input.type = \"kiwi\"\ninput.host = \"h\"\n\n[spot]\nallowlist = []\n";
        let (out, previous) = set(text, -1.42);
        assert_eq!(previous, None);
        assert_eq!(
            out,
            "input.type = \"kiwi\"\ninput.host = \"h\"\ninput.freq_correction_ppm = -1.42\n\n[spot]\nallowlist = []\n"
        );
        let (out, previous) = set("input.freq_correction_ppm = 0.5 # old\n", -1.42);
        assert_eq!(previous, Some(0.5));
        assert_eq!(out, "input.freq_correction_ppm = -1.42 # old\n");
    }

    #[test]
    fn keeps_a_utf8_bom() {
        let (out, previous) = set("\u{feff}[input]\nfreq_correction_ppm = 0.5\n", -1.42);
        assert_eq!(previous, Some(0.5));
        assert_eq!(out, "\u{feff}[input]\nfreq_correction_ppm = -1.42\n");
        let (out, _) = set("[input]\n", -1.42);
        assert!(!out.starts_with('\u{feff}'));
    }

    #[test]
    fn normalises_negative_zero() {
        let (out, _) = set("[input]\n", -0.0);
        assert_eq!(out, "[input]\nfreq_correction_ppm = 0.0\n");
        assert_eq!(literal(-0.0), "0.0");
        assert_eq!(literal(-1.42), "-1.42");
    }

    #[test]
    fn rejects_a_non_table_input() {
        let err = set_freq_correction_ppm("input = 5\n", -1.42).unwrap_err();
        assert!(format!("{err:#}").contains("[input]"), "{err:#}");
        let err = set_freq_correction_ppm("[[input]]\n", -1.42).unwrap_err();
        assert!(format!("{err:#}").contains("[input]"), "{err:#}");
    }

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, body).unwrap();
        p
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn save_validates_with_the_loader() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            dir.path(),
            "manta.toml",
            "[input]\nfreq_correction_ppm = 0.5\n",
        );
        let previous = save_freq_correction_ppm(&p, -1.42, None).unwrap();
        assert_eq!(previous, Some(0.5));
        let loaded = load(Some(&p), Env::Ignore).unwrap();
        assert_eq!(loaded.input.shared.freq_correction_ppm, Some(-1.42));
    }

    #[test]
    fn save_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(dir.path(), "manta.toml", "[input]\n");
        assert_eq!(save_freq_correction_ppm(&p, 2.5, None).unwrap(), None);
        assert_eq!(entries(dir.path()), ["manta.toml"]);

        let bad_body = "[input]\nfreq_correction_ppm = 0.5\n\n[bogus]\nx = 1\n";
        let bad = write(dir.path(), "bad.toml", bad_body);
        let err = save_freq_correction_ppm(&bad, -1.42, None).unwrap_err();
        assert!(format!("{err:#}").contains("left unchanged"), "{err:#}");
        assert_eq!(fs::read_to_string(&bad).unwrap(), bad_body);
        assert_eq!(entries(dir.path()), ["bad.toml", "manta.toml"]);
    }

    #[test]
    fn save_validates_with_the_environment_overlay() {
        let dir = tempfile::tempdir().unwrap();
        // Valid only with the environment's station callsign.
        let p = write(dir.path(), "manta.toml", "[input]\n\n[server]\n");
        let var = |k: &str, v: &str| (OsString::from(k), OsString::from(v));
        assert!(save_with_env(&p, -1.42, None, &[]).is_err());
        let vars = [
            var("MANTA_SERVER_STATION_CALLSIGN", "W1AW"),
            // Ignored: it would mask the value just written.
            var(PPM_ENV_VAR, "3.0"),
        ];
        assert_eq!(save_with_env(&p, -1.42, None, &vars).unwrap(), None);
        assert_eq!(
            fs::read_to_string(&p).unwrap(),
            "[input]\nfreq_correction_ppm = -1.42\n\n[server]\n"
        );
    }

    #[test]
    fn save_refuses_a_file_changed_since_it_was_read() {
        let dir = tempfile::tempdir().unwrap();
        let body = "[input]\nfreq_correction_ppm = 0.5\n";
        let p = write(dir.path(), "manta.toml", body);
        let before = ReadBefore::read(&p).unwrap();
        let changed = "[input]\ndevice = \"other\"\n";
        fs::write(&p, changed).unwrap();
        let err = save_freq_correction_ppm(&p, -1.42, Some(&before)).unwrap_err();
        assert!(
            format!("{err:#}").contains("changed after calibrate read it"),
            "{err:#}"
        );
        assert_eq!(fs::read_to_string(&p).unwrap(), changed);
        assert_eq!(entries(dir.path()), ["manta.toml"]);
        fs::write(&p, body).unwrap();
        assert_eq!(
            save_freq_correction_ppm(&p, -1.42, Some(&before)).unwrap(),
            Some(0.5)
        );
    }

    #[cfg(unix)]
    #[test]
    fn save_refuses_a_symlink_retargeted_since_it_was_read() {
        let dir = tempfile::tempdir().unwrap();
        let body = "[input]\npath = \"rx.wav\"\n";
        let a = write(dir.path(), "a.toml", body);
        let b = write(dir.path(), "b.toml", body);
        let link = dir.path().join("manta.toml");
        std::os::unix::fs::symlink(&a, &link).unwrap();
        let before = ReadBefore::read(&link).unwrap();
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&b, &link).unwrap();
        // Byte-identical, but another file.
        let err = save_freq_correction_ppm(&link, -1.42, Some(&before)).unwrap_err();
        assert!(format!("{err:#}").contains("now resolves to"), "{err:#}");
        assert_eq!(fs::read_to_string(&a).unwrap(), body);
        assert_eq!(fs::read_to_string(&b).unwrap(), body);
    }

    #[cfg(unix)]
    #[test]
    fn save_keeps_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = write(dir.path(), "manta.toml", "[input]\n");
        fs::set_permissions(&p, fs::Permissions::from_mode(0o600)).unwrap();
        save_freq_correction_ppm(&p, -1.42, None).unwrap();
        let mode = fs::metadata(&p).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn save_through_a_symlink_edits_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let real_dir = dir.path().join("real");
        fs::create_dir(&real_dir).unwrap();
        let target = write(
            &real_dir,
            "manta.toml",
            "[input]\nfreq_correction_ppm = 1\n",
        );
        let link = dir.path().join("link.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(
            save_freq_correction_ppm(&link, -1.42, None).unwrap(),
            Some(1.0)
        );
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "[input]\nfreq_correction_ppm = -1.42\n"
        );
        assert_eq!(entries(&real_dir), ["manta.toml"]);
        assert_eq!(entries(dir.path()), ["link.toml", "real"]);
    }

    /// `path`'s POSIX access ACL as the kernel stores it, or `None` when it
    /// has only mode bits.
    #[cfg(target_os = "linux")]
    fn access_acl(path: &Path) -> Option<Vec<u8>> {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        let mut buf = vec![0u8; 256];
        // SAFETY: NUL-terminated strings and a writable buffer of its length.
        let n = unsafe {
            libc::getxattr(
                path.as_ptr(),
                c"system.posix_acl_access".as_ptr(),
                buf.as_mut_ptr().cast(),
                buf.len(),
            )
        };
        if n < 0 {
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ENODATA)
            );
            return None;
        }
        buf.truncate(n as usize);
        Some(buf)
    }

    /// Sets the ACL `name` on `path` to rw- for the owner, r-- for uid 65534
    /// and the mask, and nothing for the group and others. False when the
    /// filesystem has no POSIX ACLs.
    #[cfg(target_os = "linux")]
    fn set_acl(path: &Path, name: &std::ffi::CStr) -> bool {
        use std::os::unix::ffi::OsStrExt;
        const UNDEFINED: u32 = u32::MAX;
        let mut acl = 2u32.to_le_bytes().to_vec();
        // (tag, perm, id): USER_OBJ, USER, GROUP_OBJ, MASK, OTHER.
        for (tag, perm, id) in [
            (0x01u16, 6u16, UNDEFINED),
            (0x02, 4, 65534),
            (0x04, 0, UNDEFINED),
            (0x10, 4, UNDEFINED),
            (0x20, 0, UNDEFINED),
        ] {
            acl.extend(tag.to_le_bytes());
            acl.extend(perm.to_le_bytes());
            acl.extend(id.to_le_bytes());
        }
        let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: NUL-terminated strings and a buffer of its length.
        let rc = unsafe {
            libc::setxattr(
                path.as_ptr(),
                name.as_ptr(),
                acl.as_ptr().cast(),
                acl.len(),
                0,
            )
        };
        if rc == 0 {
            return true;
        }
        let e = std::io::Error::last_os_error();
        assert_eq!(e.raw_os_error(), Some(libc::ENOTSUP), "{e}");
        false
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn save_keeps_the_posix_access_acl() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o7777;
        // A plain 0640 config in a directory whose default ACL, set after
        // the config was written, grants uid 65534 read.
        let dir = tempfile::tempdir().unwrap();
        let p = write(dir.path(), "manta.toml", "[input]\n");
        fs::set_permissions(&p, fs::Permissions::from_mode(0o640)).unwrap();
        if !set_acl(dir.path(), c"system.posix_acl_default") {
            eprintln!("skipped: {} has no POSIX ACLs", dir.path().display());
            return;
        }
        assert_eq!(access_acl(&p), None);
        save_freq_correction_ppm(&p, -1.42, None).unwrap();
        assert_eq!(
            access_acl(&p),
            None,
            "the saved config took the directory's default ACL"
        );
        assert_eq!(mode(&p), 0o640);

        // A config with its own ACL keeps it.
        let dir = tempfile::tempdir().unwrap();
        let q = write(dir.path(), "manta.toml", "[input]\n");
        assert!(set_acl(&q, c"system.posix_acl_access"));
        let acl = access_acl(&q);
        assert!(acl.is_some());
        save_freq_correction_ppm(&q, -1.42, None).unwrap();
        assert_eq!(access_acl(&q), acl, "the saved config lost its ACL");
        assert_eq!(mode(&q), 0o640);
    }

    /// `ls -le`'s ACL lines for `path`, one per ACE.
    #[cfg(target_os = "macos")]
    fn extended_acl(path: &Path) -> Vec<String> {
        let out = std::process::Command::new("/bin/ls")
            .arg("-led")
            .arg(path)
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .skip(1)
            .map(|l| l.trim().to_string())
            .collect()
    }

    #[cfg(target_os = "macos")]
    fn chmod_acl(path: &Path, ace: &str) -> bool {
        std::process::Command::new("/bin/chmod")
            .arg("+a")
            .arg(ace)
            .arg(path)
            .status()
            .unwrap()
            .success()
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn save_keeps_the_extended_acl() {
        // A config with no ACL in a directory whose inheritable ACE, added
        // after the config was written, grants everyone read.
        let dir = tempfile::tempdir().unwrap();
        let p = write(dir.path(), "manta.toml", "[input]\n");
        if !chmod_acl(dir.path(), "everyone allow read,file_inherit") {
            eprintln!("skipped: {} has no extended ACLs", dir.path().display());
            return;
        }
        assert!(extended_acl(&p).is_empty());
        save_freq_correction_ppm(&p, -1.42, None).unwrap();
        assert_eq!(
            extended_acl(&p),
            Vec::<String>::new(),
            "the saved config took the directory's inheritable ACE"
        );

        // A config with its own ACL keeps it.
        let dir = tempfile::tempdir().unwrap();
        let q = write(dir.path(), "manta.toml", "[input]\n");
        assert!(chmod_acl(&q, "user:nobody allow read"));
        let acl = extended_acl(&q);
        assert_eq!(acl.len(), 1, "{acl:?}");
        save_freq_correction_ppm(&q, -1.42, None).unwrap();
        assert_eq!(extended_acl(&q), acl, "the saved config lost its ACL");
    }

    /// Whether `path`'s DACL is protected from inheritance, after making it
    /// so when `protect`.
    #[cfg(windows)]
    fn dacl_protected(path: &Path, protect: bool) -> bool {
        use std::os::windows::fs::OpenOptionsExt;
        use std::os::windows::io::AsRawHandle;
        use std::ptr::{null, null_mut};
        use windows_sys::Win32::Foundation::{LocalFree, ERROR_SUCCESS};
        use windows_sys::Win32::Security::Authorization::{
            GetSecurityInfo, SetSecurityInfo, SE_FILE_OBJECT,
        };
        use windows_sys::Win32::Security::{
            GetSecurityDescriptorControl, ACL, DACL_SECURITY_INFORMATION,
            PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED,
        };
        use windows_sys::Win32::Storage::FileSystem::{READ_CONTROL, WRITE_DAC};
        let f = OpenOptions::new()
            .access_mode(READ_CONTROL | WRITE_DAC)
            .open(path)
            .unwrap();
        let mut dacl: *mut ACL = null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = null_mut();
        let (mut control, mut revision) = (0u16, 0u32);
        // SAFETY: a live handle opened with READ_CONTROL and WRITE_DAC,
        // out-pointers to locals, and `sd` freed once after its last use.
        unsafe {
            let h = f.as_raw_handle();
            let info = DACL_SECURITY_INFORMATION;
            assert_eq!(
                GetSecurityInfo(
                    h,
                    SE_FILE_OBJECT,
                    info,
                    null_mut(),
                    null_mut(),
                    &mut dacl,
                    null_mut(),
                    &mut sd
                ),
                ERROR_SUCCESS
            );
            if protect {
                let info = info | PROTECTED_DACL_SECURITY_INFORMATION;
                assert_eq!(
                    SetSecurityInfo(
                        h,
                        SE_FILE_OBJECT,
                        info,
                        null_mut(),
                        null_mut(),
                        dacl,
                        null()
                    ),
                    ERROR_SUCCESS
                );
            }
            assert_ne!(
                GetSecurityDescriptorControl(sd, &mut control, &mut revision),
                0
            );
            LocalFree(sd);
        }
        protect || control & SE_DACL_PROTECTED != 0
    }

    #[cfg(windows)]
    #[test]
    fn save_keeps_a_protected_dacl() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(dir.path(), "manta.toml", "[input]\n");
        dacl_protected(&p, true);
        assert!(dacl_protected(&p, false));
        save_freq_correction_ppm(&p, -1.42, None).unwrap();
        assert!(
            dacl_protected(&p, false),
            "the saved config took the directory's inherited ACL"
        );
        // An inheriting DACL stays inheriting.
        let q = write(dir.path(), "other.toml", "[input]\n");
        assert!(!dacl_protected(&q, false));
        save_freq_correction_ppm(&q, -1.42, None).unwrap();
        assert!(!dacl_protected(&q, false));
    }
}
