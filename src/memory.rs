// SPDX-License-Identifier: Apache-2.0
//! Optional isolated-worker allocation and resident-memory limits.
//!
//! The allocator never unwinds or allocates while reporting a budget failure.
//! A supervisor owns scratch directories and removes them even if the worker
//! exits from inside the allocator. Existing output is replaced only on success.
use crate::{args::Options, Result};
use std::alloc::{GlobalAlloc, Layout, System};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static LIMIT: AtomicUsize = AtomicUsize::new(0);
static NEXT_OUTPUT: AtomicU64 = AtomicU64::new(0);
const MANIFEST: &str = "output-manifest";
pub const LIMIT_EXIT: i32 = 75;
const WORKER: &str = "_JFRCONV_LIMITED_WORKER";
const BUDGET: &str = "_JFRCONV_ALLOCATION_BUDGET";
const RSS: &str = "_JFRCONV_RSS_LIMIT";
const OUTPUT_TEMP: &str = "_JFRCONV_OUTPUT_SCRATCH";

struct BudgetAllocator;
#[global_allocator]
static ALLOCATOR: BudgetAllocator = BudgetAllocator;

fn reserve(bytes: usize) {
    let previous = LIVE.fetch_add(bytes, Ordering::Relaxed);
    let now = previous.saturating_add(bytes);
    let cap = LIMIT.load(Ordering::Relaxed);
    if cap != 0 && now > cap {
        deny();
    }
    PEAK.fetch_max(now, Ordering::Relaxed);
}

#[cold]
fn deny() -> ! {
    const MESSAGE: &[u8] =
        b"jfrconv: allocation memory limit exceeded; increase --memory-limit or --max-rss\n";
    exit_limit(MESSAGE)
}

#[cold]
fn exit_limit(message: &'static [u8]) -> ! {
    // SAFETY: immutable static byte slice is valid for the exact byte length.
    // write/_exit are allocation-free; unwinding inside GlobalAlloc is forbidden.
    unsafe {
        libc::write(2, message.as_ptr().cast(), message.len());
        libc::_exit(LIMIT_EXIT)
    }
}

// SAFETY: all actual allocation operations are delegated to System with the
// caller's original pointer/Layout. Accounting does not alter pointer ownership.
unsafe impl GlobalAlloc for BudgetAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        reserve(layout.size());
        let pointer = unsafe { System.alloc(layout) };
        if pointer.is_null() {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            if LIMIT.load(Ordering::Relaxed) != 0 {
                deny();
            }
        }
        pointer
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        reserve(layout.size());
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if pointer.is_null() {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            if LIMIT.load(Ordering::Relaxed) != 0 {
                deny();
            }
        }
        pointer
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // Account for the old and new allocation simultaneously: System may
        // temporarily hold both when it has to copy to a different allocation.
        reserve(new_size);
        let result = unsafe { System.realloc(pointer, layout, new_size) };
        if result.is_null() {
            LIVE.fetch_sub(new_size, Ordering::Relaxed);
            if LIMIT.load(Ordering::Relaxed) != 0 {
                deny();
            }
        } else {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        }
        result
    }
}

pub fn parse_size(text: &str) -> Result<u64> {
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let number: u64 = text[..split]
        .parse()
        .map_err(|_| format!("Invalid memory size {text}"))?;
    let multiplier = match text[split..].to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        _ => return Err(format!("Invalid memory size suffix in {text}").into()),
    };
    let value = number
        .checked_mul(multiplier)
        .ok_or("Memory limit overflow")?;
    if value == 0 || value > usize::MAX as u64 {
        return Err("Memory limit must be positive and fit this platform".into());
    }
    Ok(value)
}

/// Called before parsing any potentially allocating CLI filters in a worker.
pub fn configure_worker() -> Result<()> {
    if std::env::var_os(WORKER).is_none() {
        return Ok(());
    }
    if let Some(value) = std::env::var_os(BUDGET) {
        let limit: usize = value.to_str().ok_or("Invalid allocation budget")?.parse()?;
        LIMIT.store(limit, Ordering::Relaxed);
        if LIVE.load(Ordering::Relaxed) > limit {
            deny();
        }
    }
    if let Some(value) = std::env::var_os(RSS) {
        let limit: u64 = value.to_str().ok_or("Invalid RSS limit")?.parse()?;
        if resident_bytes(std::process::id()).is_none() {
            return Err("Cannot observe worker resident memory; --max-rss is unavailable".into());
        }
        #[cfg(target_os = "linux")]
        {
            // RLIMIT_RSS is ignored on Linux. Bounding all mapped address space
            // is conservative. Reject already-larger startup mappings before
            // applying it: setrlimit alone does not evict existing mappings.
            let status = std::fs::read_to_string("/proc/self/status")?;
            let virtual_size = status
                .lines()
                .find_map(|line| {
                    line.strip_prefix("VmSize:")?
                        .split_whitespace()
                        .next()?
                        .parse::<u64>()
                        .ok()?
                        .checked_mul(1024)
                })
                .ok_or("Cannot inspect startup address space for --max-rss")?;
            if virtual_size > limit {
                exit_limit(
                    b"jfrconv: --max-rss is smaller than the worker's startup address space\n",
                );
            }
            let limit = libc::rlimit {
                rlim_cur: limit as libc::rlim_t,
                rlim_max: limit as libc::rlim_t,
            };
            // SAFETY: valid initialized rlimit; this changes only this worker.
            if unsafe { libc::setrlimit(libc::RLIMIT_AS, &limit) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = limit;
    }
    Ok(())
}

/// Return None for the normal fast path, otherwise supervise an isolated worker.
pub fn supervise() -> Result<Option<i32>> {
    if std::env::var_os(WORKER).is_some() {
        return Ok(None);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args
        .iter()
        .any(|a| a == "--memory-limit" || a == "--max-rss")
    {
        return Ok(None);
    }
    let options = Options::parse(args.clone())?;
    if options.help || options.version || options.files.is_empty() {
        return Ok(None);
    }
    let rss = options.max_rss;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    if rss.is_some() {
        return Err("--max-rss is supported only on Linux and macOS".into());
    }
    if rss.is_some() && resident_bytes(std::process::id()).is_none() {
        return Err("Cannot observe resident memory; --max-rss is unavailable".into());
    }
    if rss.is_some_and(|n| n < 4 * 1024 * 1024) {
        return Err("--max-rss must be at least 4MiB".into());
    }
    let rss_budget = rss.map(|n| n.saturating_sub(16 * 1024 * 1024).max(1024 * 1024));
    let allocation_budget = match (options.memory_limit, rss_budget) {
        (Some(a), Some(b)) => a.min(b),
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => return Ok(None),
    };
    let implicit_inputs = if options.diff { 2 } else { 1 };
    let target = if options.files.len() > implicit_inputs {
        Path::new(options.files.last().unwrap())
    } else {
        Path::new(".")
    };
    let parent = if target.is_dir() {
        target
    } else {
        target
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
    };
    let authorized = authorized_outputs(&options, target)?;
    let output_scratch = if is_stream_output(target) {
        tempfile::Builder::new()
            .prefix(".jfrconv-worker-")
            .tempdir()?
    } else {
        tempfile::Builder::new()
            .prefix(".jfrconv-worker-")
            .tempdir_in(parent)?
    };
    let scratch = tempfile::Builder::new()
        .prefix("jfrconv-spill-")
        .tempdir()?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(&args)
        .env(WORKER, "1")
        .env(BUDGET, allocation_budget.to_string())
        .env(OUTPUT_TEMP, output_scratch.path())
        .env("TMPDIR", scratch.path())
        .env("TMP", scratch.path())
        .env("TEMP", scratch.path())
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if let Some(limit) = rss {
        command.env(RSS, limit.to_string());
    }
    let mut child = command.spawn()?;
    let mut rss_observed = false;
    let mut missing_samples = 0;
    loop {
        if let Some(status) = child.try_wait()? {
            if rss.is_some() && !rss_observed && status.success() {
                eprintln!("jfrconv: worker finished before an RSS sample; allocation budget remained active");
            }
            if status.success() {
                publish_staged(output_scratch.path(), &authorized)?;
            }
            return Ok(Some(status.code().unwrap_or(1)));
        }
        if let Some(limit) = rss {
            if let Some(actual) = resident_bytes(child.id()) {
                rss_observed = true;
                missing_samples = 0;
                if actual > limit {
                    child.kill()?;
                    child.wait()?;
                    eprintln!("jfrconv: resident memory limit exceeded ({actual} > {limit} bytes)");
                    return Ok(Some(LIMIT_EXIT));
                }
            } else {
                missing_samples += 1;
                if missing_samples >= 4 {
                    child.kill()?;
                    child.wait()?;
                    return Err("Cannot sample live worker resident memory; refusing an unenforced --max-rss guard".into());
                }
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    // Both supervisor-owned directories drop on all regular/error returns,
    // including a worker exiting from the global allocator's deny() path.
}

// Derive the same destinations as main without opening or changing user files.
fn authorized_outputs(options: &Options, target: &Path) -> Result<Vec<PathBuf>> {
    let implicit = if options.diff { 2 } else { 1 };
    let count = options.files.len() - usize::from(options.files.len() > implicit);
    let inputs = &options.files[..count];
    if options.diff && inputs.len() != 2 {
        return Err("--diff requires two inputs".into());
    }
    let directory = target.is_dir();
    if inputs.len() > 1 && !directory && !options.diff {
        return Err("Multiple inputs require an output directory".into());
    }
    let format = options.output.as_deref().unwrap_or_else(|| {
        if directory {
            "html"
        } else {
            target
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("html")
        }
    });
    let extension = if options.diff {
        format!("diff.{format}")
    } else if format == "heatmap" {
        "html".into()
    } else if format == "gz" {
        "pb.gz".into()
    } else {
        format.to_owned()
    };
    let inputs = if options.diff { &inputs[1..] } else { inputs };
    inputs
        .iter()
        .map(|input| {
            if directory {
                Ok(target
                    .join(
                        Path::new(input)
                            .file_name()
                            .ok_or("Invalid input filename")?,
                    )
                    .with_extension(&extension))
            } else {
                Ok(target.to_owned())
            }
        })
        .collect()
}
#[cfg(unix)]
fn path_bytes(path: &Path) -> Result<&[u8]> {
    use std::os::unix::ffi::OsStrExt;
    Ok(path.as_os_str().as_bytes())
}
#[cfg(not(unix))]
fn path_bytes(path: &Path) -> Result<&[u8]> {
    Ok(path
        .to_str()
        .ok_or("Output path must be UTF-8 on this platform")?
        .as_bytes())
}
#[cfg(unix)]
fn path_from_bytes(bytes: Vec<u8>) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    Ok(std::ffi::OsString::from_vec(bytes).into())
}
#[cfg(not(unix))]
fn path_from_bytes(bytes: Vec<u8>) -> Result<PathBuf> {
    Ok(String::from_utf8(bytes)?.into())
}
fn staged_path(scratch: &Path, id: u64) -> PathBuf {
    scratch.join(format!("result-{id}"))
}

/// In a limited worker, publication belongs to the supervisor after exit0.
/// A budget failure while flushing diagnostics cannot replace an existing file.
pub fn publish_output(temp: tempfile::NamedTempFile, output: &Path) -> Result<()> {
    if std::env::var_os(WORKER).is_none() {
        return publish_file(temp, output);
    }
    let scratch = PathBuf::from(
        std::env::var_os(OUTPUT_TEMP).ok_or("Limited worker has no output scratch directory")?,
    );
    stage_output(
        temp,
        output,
        &scratch,
        NEXT_OUTPUT.fetch_add(1, Ordering::Relaxed),
    )
}
fn stage_output(
    temp: tempfile::NamedTempFile,
    output: &Path,
    scratch: &Path,
    id: u64,
) -> Result<()> {
    temp.persist(staged_path(scratch, id))?;
    let bytes = path_bytes(output)?;
    let mut entry = Vec::with_capacity(16 + bytes.len());
    entry.extend_from_slice(&id.to_le_bytes());
    entry.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    entry.extend_from_slice(bytes);
    let mut manifest = OpenOptions::new()
        .create(true)
        .append(true)
        .open(scratch.join(MANIFEST))?;
    manifest.write_all(&entry)?;
    Ok(())
}
fn publish_staged(scratch: &Path, expected: &[PathBuf]) -> Result<()> {
    let mut manifest = File::open(scratch.join(MANIFEST))?;
    let mut entries = Vec::with_capacity(expected.len());
    let mut ids = std::collections::HashSet::new();
    loop {
        let mut id = [0u8; 8];
        if manifest.read(&mut id[..1])? == 0 {
            break;
        }
        manifest.read_exact(&mut id[1..])?;
        let id = u64::from_le_bytes(id);
        let mut length = [0u8; 8];
        manifest.read_exact(&mut length)?;
        let length = u64::from_le_bytes(length);
        let destination = expected
            .get(entries.len())
            .ok_or("Limited worker produced too many outputs")?;
        if length != path_bytes(destination)?.len() as u64 {
            return Err("Limited worker output path length is invalid".into());
        }
        let mut bytes = vec![0; usize::try_from(length)?];
        manifest.read_exact(&mut bytes)?;
        let path = path_from_bytes(bytes)?;
        if &path != destination {
            return Err("Limited worker output destination is unauthorized".into());
        }
        if !ids.insert(id) {
            return Err("Duplicate limited worker output id".into());
        }
        let staged = staged_path(scratch, id);
        if !std::fs::symlink_metadata(&staged)?.file_type().is_file() {
            return Err("Limited worker output is not a regular file".into());
        }
        if is_stream_output(&path) {
            validate_stream_kind(
                &std::fs::metadata(&path)?,
                output_descriptor(&path).is_some(),
            )?;
        }
        entries.push((staged, path));
    }
    if entries.len() != expected.len() {
        return Err("Limited worker output manifest is incomplete".into());
    }
    // Every record and staged file is checked before replacing any destination.
    for (staged, destination) in entries {
        let file = File::open(&staged)?;
        let path = tempfile::TempPath::try_from_path(staged)?;
        publish_file(
            tempfile::NamedTempFile::from_parts(file, path),
            &destination,
        )?;
    }
    Ok(())
}

/// Existing devices/FIFOs and descriptor aliases are streamed, never renamed.
/// Descriptor aliases remain streams even when stdout points at a regular file.
pub fn is_stream_output(output: &Path) -> bool {
    output_descriptor(output).is_some()
        || output
            .metadata()
            .is_ok_and(|meta| !meta.is_file() && !meta.is_dir())
}
fn output_descriptor(output: &Path) -> Option<i32> {
    if output == Path::new("/dev/stdout") {
        return Some(1);
    }
    if output == Path::new("/dev/stderr") {
        return Some(2);
    }
    for prefix in ["/dev/fd", "/proc/self/fd"] {
        if let Ok(suffix) = output.strip_prefix(prefix) {
            let mut components = suffix.components();
            if let Some(std::path::Component::Normal(name)) = components.next() {
                if components.next().is_none() {
                    let name = name.to_str()?;
                    if !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit()) {
                        return name.parse().ok();
                    }
                }
            }
        }
    }
    None
}
fn validate_stream_kind(metadata: &std::fs::Metadata, descriptor: bool) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        let kind = metadata.file_type();
        if kind.is_block_device() {
            return Err("Block-device output is unsupported".into());
        }
        if kind.is_char_device()
            || kind.is_fifo()
            || descriptor && (kind.is_file() || kind.is_socket())
        {
            return Ok(());
        }
        if kind.is_socket() {
            return Err(
                "Socket paths are unsupported output destinations; use an open descriptor".into(),
            );
        }
    }
    #[cfg(not(unix))]
    let _ = (metadata, descriptor);
    Err("Unsupported special output destination".into())
}
fn stream_file(output: &Path) -> Result<File> {
    #[cfg(unix)]
    if let Some(fd) = output_descriptor(output) {
        use std::os::fd::FromRawFd;
        // SAFETY: fcntl borrows the numeric descriptor and returns a new owned
        // descriptor. Duplication preserves the current offset and access mode.
        let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
        if duplicate < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: successful F_DUPFD_CLOEXEC gives this function sole ownership
        // of the returned descriptor; File closes it on every path.
        let file = unsafe { File::from_raw_fd(duplicate) };
        validate_stream_kind(&file.metadata()?, true)?;
        return Ok(file);
    }
    validate_stream_kind(&std::fs::metadata(output)?, false)?;
    // Existing-only, with neither create nor truncate. A character device/FIFO
    // retains its inode, as does a symlink referring to one.
    let file = OpenOptions::new().write(true).open(output)?;
    validate_stream_kind(&file.metadata()?, false)?;
    Ok(file)
}
fn publish_file(mut source: tempfile::NamedTempFile, output: &Path) -> Result<()> {
    if is_stream_output(output) {
        let mut destination = stream_file(output)?;
        source.as_file_mut().seek(SeekFrom::Start(0))?;
        std::io::copy(source.as_file_mut(), &mut destination)?;
        destination.flush()?;
        // Dropping source removes its owned staging path after a successful
        // copy, or on an error; no stream pathname is ever renamed/unlinked.
    } else {
        source.persist(output)?;
    }
    Ok(())
}
pub fn output_temp_dir(parent: &Path, output: &Path) -> PathBuf {
    std::env::var_os(OUTPUT_TEMP)
        .filter(|_| std::env::var_os(WORKER).is_some())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            if is_stream_output(output) {
                std::env::temp_dir()
            } else {
                parent.to_owned()
            }
        })
}

pub fn allocation_bytes() -> (usize, usize) {
    let live = LIVE.load(Ordering::Relaxed);
    // A concurrent reserve can update LIVE before its PEAK.fetch_max. The
    // snapshot's observed peak must include the live reservation we just read.
    (live, PEAK.load(Ordering::Relaxed).max(live))
}

#[cfg(target_os = "linux")]
pub fn resident_bytes(pid: u32) -> Option<u64> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    text.lines().find_map(|line| {
        line.strip_prefix("VmRSS:")?
            .split_whitespace()
            .next()?
            .parse::<u64>()
            .ok()?
            .checked_mul(1024)
    })
}
#[cfg(target_os = "macos")]
pub fn resident_bytes(pid: u32) -> Option<u64> {
    let mut info = std::mem::MaybeUninit::<libc::proc_taskinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_taskinfo>();
    // SAFETY: proc_pidinfo is given an appropriately sized writable buffer.
    // Only assume initialization if the API reports the entire structure.
    let read = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTASKINFO,
            0,
            info.as_mut_ptr().cast(),
            size as i32,
        )
    };
    if read as usize != size {
        return None;
    }
    Some(unsafe { info.assume_init() }.pti_resident_size)
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn resident_bytes(_pid: u32) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sizes_are_checked_and_unambiguous() {
        assert_eq!(parse_size("64MiB").unwrap(), 64 * 1024 * 1024);
        assert_eq!(parse_size("2g").unwrap(), 2 * 1024 * 1024 * 1024);
        for s in ["0", "-1", "1.5M", "18446744073709551615G", "12wat"] {
            assert!(parse_size(s).is_err());
        }
    }
    #[test]
    fn current_process_has_observable_resident_memory() {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        assert!(resident_bytes(std::process::id()).unwrap() > 0);
        let (live, peak) = allocation_bytes();
        assert!(peak >= live);
    }
    fn stage(scratch: &Path, output: &Path, id: u64, bytes: &[u8]) {
        let mut file = tempfile::NamedTempFile::new_in(scratch).unwrap();
        file.write_all(bytes).unwrap();
        stage_output(file, output, scratch, id).unwrap();
    }
    #[test]
    fn staged_outputs_do_not_replace_prior_results_until_publication() {
        let root = tempfile::tempdir().unwrap();
        let scratch = root.path().join("worker");
        std::fs::create_dir(&scratch).unwrap();
        let output = root.path().join("résumé-α.collapsed");
        std::fs::write(&output, b"prior result").unwrap();
        stage(&scratch, &output, 0, b"new result");
        assert_eq!(std::fs::read(&output).unwrap(), b"prior result");
        publish_staged(&scratch, std::slice::from_ref(&output)).unwrap();
        assert_eq!(std::fs::read(&output).unwrap(), b"new result");
        assert!(!staged_path(&scratch, 0).exists());
    }
    #[test]
    fn validates_every_staged_file_before_replacing_any_destination() {
        let root = tempfile::tempdir().unwrap();
        let scratch = root.path().join("worker");
        std::fs::create_dir(&scratch).unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        std::fs::write(&first, b"old first").unwrap();
        std::fs::write(&second, b"old second").unwrap();
        stage(&scratch, &first, 0, b"new first");
        stage(&scratch, &second, 1, b"new second");
        std::fs::remove_file(staged_path(&scratch, 1)).unwrap();
        assert!(publish_staged(&scratch, &[first.clone(), second.clone()]).is_err());
        assert_eq!(std::fs::read(first).unwrap(), b"old first");
        assert_eq!(std::fs::read(second).unwrap(), b"old second");
    }
    #[test]
    fn refuses_unexpected_destinations_and_excessive_manifest_lengths() {
        let root = tempfile::tempdir().unwrap();
        let scratch = root.path().join("worker");
        std::fs::create_dir(&scratch).unwrap();
        let output = root.path().join("valid");
        let other = root.path().join("other");
        std::fs::write(&output, b"prior").unwrap();
        stage(&scratch, &other, 0, b"new");
        assert!(publish_staged(&scratch, std::slice::from_ref(&output)).is_err());
        assert!(!other.exists());
        assert_eq!(std::fs::read(&output).unwrap(), b"prior");
        let mut manifest = Vec::new();
        manifest.extend_from_slice(&0u64.to_le_bytes());
        manifest.extend_from_slice(&u64::MAX.to_le_bytes());
        std::fs::write(scratch.join(MANIFEST), manifest).unwrap();
        assert!(publish_staged(&scratch, std::slice::from_ref(&output)).is_err());
        assert_eq!(std::fs::read(output).unwrap(), b"prior");
    }
    #[cfg(unix)]
    #[test]
    fn path_codec_preserves_non_utf8_unix_path_bytes() {
        use std::os::unix::ffi::OsStringExt;
        let bytes = b"output-\xff".to_vec();
        let path = PathBuf::from(std::ffi::OsString::from_vec(bytes.clone()));
        assert_eq!(path_bytes(&path).unwrap(), bytes);
        assert_eq!(path_from_bytes(bytes).unwrap(), path);
    }
    #[cfg(unix)]
    #[test]
    fn descriptor_stream_preserves_regular_file_identity_and_current_offset() {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::MetadataExt;
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("redirected");
        let mut descriptor = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&output)
            .unwrap();
        descriptor.write_all(b"prefix:").unwrap();
        let identity = descriptor.metadata().unwrap().ino();
        let alias = PathBuf::from(format!("/dev/fd/{}", descriptor.as_raw_fd()));
        assert!(is_stream_output(&alias));
        assert!(alias.metadata().unwrap().is_file());
        let mut source = tempfile::NamedTempFile::new_in(root.path()).unwrap();
        source.write_all(b"profile").unwrap();
        publish_file(source, &alias).unwrap();
        assert_eq!(std::fs::read(&output).unwrap(), b"prefix:profile");
        assert_eq!(descriptor.stream_position().unwrap(), 14);
        assert_eq!(descriptor.metadata().unwrap().ino(), identity);
    }
    #[cfg(unix)]
    #[test]
    fn symlink_to_character_device_is_copied_without_replacing_the_symlink() {
        use std::os::unix::fs::{symlink, FileTypeExt, MetadataExt};
        let device = Path::new("/dev/null");
        let identity = device.metadata().unwrap().ino();
        assert!(device.metadata().unwrap().file_type().is_char_device());
        let root = tempfile::tempdir().unwrap();
        let alias = root.path().join("null-output");
        symlink(device, &alias).unwrap();
        let link_identity = alias.symlink_metadata().unwrap().ino();
        let mut source = tempfile::NamedTempFile::new_in(root.path()).unwrap();
        source.write_all(b"discard this profile").unwrap();
        publish_file(source, &alias).unwrap();
        assert!(alias.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(alias.symlink_metadata().unwrap().ino(), link_identity);
        assert_eq!(device.metadata().unwrap().ino(), identity);
    }
}
