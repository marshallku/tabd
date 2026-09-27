//! `tabd profile import` — bring the human's everyday browser profile under
//! tabd's control without ever putting the original at risk.
//!
//! The shape is from `docs/visual-mode-plan.md` §V1: copy to a staging tree,
//! let the human verify it by launching it, then publish with an atomic
//! rename. **The original is only ever read.** Nothing here moves, modifies or
//! deletes it, so the cost of a bad import is a wasted copy.
//!
//! ## What this is not
//!
//! It is an interlock, not a filesystem snapshot. tabd cannot freeze another
//! process's writes to a directory it does not own. A browser that both starts
//! and fully exits inside the copy window could still produce an inconsistent
//! tree. What is done instead, in order of strength:
//!
//! 1. Refuse to start while the browser is running or the profile is locked.
//! 2. Watch the source for a `Singleton*` entry every 250 ms **throughout** the
//!    copy and abort the moment one appears. A Chromium cannot open a profile
//!    without creating those, and they live for its whole lifetime, so the only
//!    gap is a browser whose entire lifetime fits in 250 ms — shorter than
//!    Chromium's startup.
//! 3. Compare a metadata digest of the whole retained tree before and after.
//!
//! and then the human verifies the staging copy by actually using it before
//! anything is published.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::platform;

/// How often the copy re-checks that nothing has opened the source.
const OCCUPANCY_POLL: Duration = Duration::from_millis(250);

/// Disposable entries directly under the **profile root**.
const DISPOSABLE_AT_ROOT: &[&str] = &[
    // Crash dumps and the metrics spool.
    "Crashpad",
    "CrashpadMetrics",
    "BrowserMetrics",
    "lockfile",
    // Browser-wide GPU/shader caches.
    "GrShaderCache",
    "ShaderCache",
    "GraphiteDawnCache",
    "GPUCache",
    "component_crx_cache",
];

/// Disposable entries directly under a **profile directory** (`Default`,
/// `Profile 1`, `Guest Profile`, …).
const DISPOSABLE_IN_PROFILE: &[&str] = &[
    "Cache",
    "Code Cache",
    "DawnCache",
    "DawnGraphiteCache",
    "DawnWebGPUCache",
    "GPUCache",
    "JumpListIcons",
    "JumpListIconsOld",
    "optimization_guide_prediction_model_downloads",
];

/// Entries whose presence means some Chromium has this profile open.
const OCCUPANCY_MARKERS: &[&str] = &["SingletonLock", "SingletonSocket", "SingletonCookie"];

#[derive(Debug, Clone)]
pub struct Plan {
    /// The human's real profile. Read-only, always.
    pub source: PathBuf,
    /// `<destination>.staging` — a sibling, so the publish rename is atomic.
    pub staging: PathBuf,
    /// Where visual mode will look for the profile.
    pub destination: PathBuf,
    /// Base dir for the verification daemon the human is told to run.
    pub verify_base: PathBuf,
    /// The browser that owns `source`, and will own `destination`.
    pub executable: PathBuf,
}

/// Resolve the three paths and the browser, and refuse any arrangement where
/// they overlap.
pub fn plan(from: Option<&str>, to: Option<&str>) -> Result<Plan> {
    let executable = crate::browser::discover_chromium()?;
    let executable = executable
        .canonicalize()
        .with_context(|| format!("resolve browser executable {}", executable.display()))?;

    let source = match from {
        Some(path) => PathBuf::from(path),
        None => platform::real_profile_dir(&executable).with_context(|| {
            format!(
                "don't know where {} keeps its profile — pass --from <dir>",
                executable.display()
            )
        })?,
    };
    let source = source
        .canonicalize()
        .with_context(|| format!("source profile not found: {}", source.display()))?;
    if !source.is_dir() {
        bail!("source profile is not a directory: {}", source.display());
    }

    let destination = match to {
        Some(path) => PathBuf::from(path),
        None => platform::visual_profile_dir()?,
    };
    let destination = platform::canonical_profile_dir(&destination)?;
    let staging = staging_path(&destination);
    // The base dir the printed verification command uses. It is validated
    // here because a daemon started there chmods it and writes `daemon.sock`
    // and `daemon.pid` into it — so if it landed on the source, following our
    // own instructions would write into the original profile.
    let verify_base = platform::canonical_profile_dir(&sibling(&staging, ".verify"))?;

    // Canonical paths, so a symlinked alias cannot smuggle one inside another.
    for (a_name, a, b_name, b) in [
        ("source", &source, "destination", &destination),
        ("source", &source, "staging", &staging),
        ("source", &source, "verification directory", &verify_base),
        ("staging", &staging, "destination", &destination),
        (
            "verification directory",
            &verify_base,
            "destination",
            &destination,
        ),
    ] {
        if a == b || a.starts_with(b) || b.starts_with(a) {
            bail!(
                "{a_name} ({}) and {b_name} ({}) overlap; copying one into the other would \
                 corrupt both",
                a.display(),
                b.display()
            );
        }
    }
    Ok(Plan {
        source,
        staging,
        destination,
        verify_base,
        executable,
    })
}

/// `<destination>.staging` — a sibling, so the publish rename stays inside one
/// filesystem and can be atomic.
fn staging_path(destination: &Path) -> PathBuf {
    sibling(destination, ".staging")
}

// -- Exclusion --------------------------------------------------------------

/// Whether a path relative to the profile root is disposable.
///
/// Only the first two levels are considered, and that is the point. Matching a
/// cache name at *any* depth also drops
/// `Default/Extensions/<id>/<version>/Cache/…`, which is an extension's own
/// bundled asset — referenced by its manifest, not regenerable, and silently
/// missing from both the copy and the digest that is supposed to prove the
/// copy complete.
pub(crate) fn is_excluded(relative: &Path) -> bool {
    let mut components = relative.iter();
    let Some(first) = components.next() else {
        return false;
    };
    let name = first.to_string_lossy();
    if name.starts_with("Singleton") || DISPOSABLE_AT_ROOT.iter().any(|d| name.as_ref() == *d) {
        return true;
    }
    // `<profile dir>/<cache>` — and everything beneath it, since the walk
    // never descends into a directory this rejects.
    match components.next() {
        Some(second) => DISPOSABLE_IN_PROFILE
            .iter()
            .any(|d| second == OsStr::new(d)),
        None => false,
    }
}

// -- Occupancy --------------------------------------------------------------

/// The name of the marker showing some Chromium has this profile open, if any.
/// Uses `symlink_metadata`: `SingletonLock` is a symlink to `<host>-<pid>` that
/// usually does not resolve, so `exists()` would miss it.
fn occupancy_marker(profile: &Path) -> Option<String> {
    OCCUPANCY_MARKERS
        .iter()
        .find(|name| profile.join(name).symlink_metadata().is_ok())
        .map(|name| (*name).to_string())
}

/// PID of a running process whose command line mentions this executable.
///
/// The *flag* `--user-data-dir` cannot be used for this: a normally-launched
/// browser uses its default profile path implicitly and carries no such flag.
/// Chromium also rewrites its own argv, so the command line comes back as one
/// space-joined blob — fine for a substring search, useless for splitting.
fn browser_pid(executable: &Path) -> Result<Option<u32>> {
    // Errors rather than answering "nothing is running". This is a safety
    // interlock: if process enumeration fails we have learned nothing, and
    // treating that as "no browser" would silently skip the check and copy a
    // live profile.
    let output = std::process::Command::new("ps")
        .args(["-Ao", "pid=,command="])
        .output()
        .context("run `ps` to check whether the browser is running")?;
    if !output.status.success() {
        bail!(
            "`ps` failed ({}), so it cannot be confirmed that the browser is not running",
            output.status
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| {
            let line = line.trim_start();
            let (pid, command) = line.split_once(char::is_whitespace)?;
            command_is_browser(command, executable).then(|| pid.parse().ok())?
        }))
}

/// Whether one `ps` command line belongs to this browser.
///
/// Two tests, because neither covers both platforms:
///
/// - **The canonical path as a substring.** This is what matches on macOS,
///   where the executable is `…/Contents/MacOS/Google Chrome` — a name with a
///   space in it, so splitting the command line into tokens would break it.
/// - **`argv[0]`'s file name.** This is what matches on Linux, where Chromium
///   ships a [wrapper script] that does `exec -a "$0"`: the process keeps
///   `/usr/bin/google-chrome` as `argv[0]` while the binary that is actually
///   running lives at `/opt/google/chrome/…`, so the canonical path need not
///   appear anywhere in `ps` at all.
///
/// [wrapper script]: https://source.chromium.org/chromium/chromium/src/+/main:chrome/installer/linux/common/wrapper
///
/// A false positive here costs a refused import with a clear message; a false
/// negative means copying a live profile. So when the two disagree, err
/// towards "running".
fn command_is_browser(command: &str, executable: &Path) -> bool {
    if command.contains(&*executable.to_string_lossy()) {
        return true;
    }
    let Some(wanted) = executable.file_name() else {
        return false;
    };
    command
        .split_whitespace()
        .next()
        .map(Path::new)
        .and_then(Path::file_name)
        .is_some_and(|argv0| argv0 == wanted)
}

/// Everything that must be true before a profile directory may be read or
/// published. `executable` is passed in rather than rediscovered, so the
/// caller can check against the browser that actually *owns* the data.
fn assert_quiet(profile: &Path, executable: &Path) -> Result<()> {
    if let Some(marker) = occupancy_marker(profile) {
        bail!(
            "{} is open in a browser ({marker} is present). Quit the browser completely and \
             try again — copying a live profile yields an inconsistent SQLite snapshot.",
            profile.display()
        );
    }
    if let Some(pid) = browser_pid(executable)? {
        bail!(
            "{} is still running (pid {pid}). Quit it completely and try again.",
            executable.display()
        );
    }
    Ok(())
}

// -- Tree digest ------------------------------------------------------------

/// A digest over the metadata of every file this import would keep.
///
/// Replaces an earlier seven-file manifest, which could not support the
/// promise made at cutover: a source whose bookmarks, IndexedDB, extension
/// storage or second profile had changed looked untouched, and the stale copy
/// was published over that work. The digest covers the whole retained tree —
/// every path, size, mtime and inode — and is 32 bytes instead of a
/// hundred-thousand-entry list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TreeDigest {
    files: u64,
    bytes: u64,
    /// Hex sha256 over the sorted `(path, size, mtime_ns, inode)` tuples.
    sha256: String,
}

/// What `import` recorded, for `cutover` to check against./// What `import` recorded, for `cutover` to check against.
///
/// Written beside the staging tree, not inside it: anything inside would be
/// published into the profile. It is what makes cutover able to answer
/// questions the filesystem alone cannot — "has the source been used since the
/// copy" and "which browser did this data come from".
#[derive(Debug, Serialize, Deserialize)]
struct ImportState {
    source: PathBuf,
    /// The browser whose profile this is. Recorded at import, because a
    /// `$BROWSER_EXECUTABLE` set differently at cutover time would otherwise
    /// bind Chrome's data to Brave — and opening it with Brave rewrites it.
    executable: PathBuf,
    /// The source's retained tree as it was when the copy finished.
    digest: TreeDigest,
}

/// `<destination>.import`, beside the staging tree.
fn state_path(destination: &Path) -> PathBuf {
    sibling(destination, ".import")
}

/// `<profile>.browser`, matching `daemon::visual`.
fn binding_path(profile_dir: &Path) -> PathBuf {
    sibling(profile_dir, ".browser")
}

/// `<path><suffix>`, built from parent + file name so a trailing slash cannot
/// turn the result into a file *inside* `path`.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let Some(name) = path.file_name() else {
        return path.join(format!(".tabd{suffix}"));
    };
    let mut sibling = name.to_os_string();
    sibling.push(suffix);
    match path.parent() {
        Some(parent) => parent.join(sibling),
        None => PathBuf::from(sibling),
    }
}

/// Quote a path for the shell commands printed to the user. Default macOS
/// profile paths contain spaces, so an unquoted instruction is one the human
/// cannot paste.
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', r"'\''"))
}

// -- Scan -------------------------------------------------------------------

#[derive(Debug)]
struct Scan {
    files: Vec<PathBuf>,
    dirs: Vec<PathBuf>,
    bytes: u64,
    digest: TreeDigest,
}

/// Walk the source, collecting what to copy and refusing anything that is not
/// a plain file or directory.
///
/// Symlinks are **rejected, not copied**. Copying the link preserves the
/// hazard: Chromium follows it during the verification launch, and an absolute
/// link reaches straight back into the original profile.
fn scan(source: &Path) -> Result<Scan> {
    let mut scan = Scan {
        files: Vec::new(),
        dirs: Vec::new(),
        bytes: 0,
        digest: TreeDigest {
            files: 0,
            bytes: 0,
            sha256: String::new(),
        },
    };
    let mut fingerprints: Vec<String> = Vec::new();
    let mut refused: Vec<String> = Vec::new();
    let mut queue = vec![PathBuf::new()];

    while let Some(relative) = queue.pop() {
        let absolute = source.join(&relative);
        let entries =
            std::fs::read_dir(&absolute).with_context(|| format!("read {}", absolute.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("read {}", absolute.display()))?;
            let child = relative.join(entry.file_name());
            if is_excluded(&child) {
                continue;
            }
            let meta = entry
                .metadata() // symlink_metadata semantics: DirEntry::metadata does not follow
                .with_context(|| format!("stat {}", source.join(&child).display()))?;
            if meta.is_symlink() {
                refused.push(child.display().to_string());
            } else if meta.is_dir() {
                scan.dirs.push(child.clone());
                queue.push(child);
            } else if meta.is_file() {
                use std::os::unix::fs::MetadataExt;
                scan.bytes += meta.len();
                fingerprints.push(format!(
                    "{}\0{}\0{}.{}\0{}",
                    child.display(),
                    meta.len(),
                    meta.mtime(),
                    meta.mtime_nsec(),
                    meta.ino()
                ));
                scan.files.push(child);
            } else {
                refused.push(format!("{} (not a regular file)", child.display()));
            }
        }
    }

    if !refused.is_empty() {
        bail!(
            "refusing to import: {} entr{} in {} {} not plain files or directories, and a copied \
             symlink would be followed straight back into the original profile:\n  {}",
            refused.len(),
            if refused.len() == 1 { "y" } else { "ies" },
            source.display(),
            if refused.len() == 1 { "is" } else { "are" },
            refused.join("\n  ")
        );
    }
    scan.dirs.sort();

    use sha2::{Digest, Sha256};
    fingerprints.sort();
    let mut hasher = Sha256::new();
    for line in &fingerprints {
        hasher.update(line.as_bytes());
        hasher.update(b"\n");
    }
    scan.digest = TreeDigest {
        files: fingerprints.len() as u64,
        bytes: scan.bytes,
        sha256: format!("{:x}", hasher.finalize()),
    };
    Ok(scan)
}

/// The digest alone, for comparing a source against a recorded import. Shares
/// `scan`'s traversal and exclusion rules so the two are always in step; the
/// symlink refusal is part of that, and a symlink appearing after the import
/// is itself a change worth failing on.
fn tree_digest(source: &Path) -> Result<TreeDigest> {
    Ok(scan(source)?.digest)
}

// -- Import -----------------------------------------------------------------

/// Copy the source into a fresh staging tree. Non-destructive: it only ever
/// reads the source, and it creates exactly one new directory.
pub fn import(plan: &Plan) -> Result<()> {
    assert_quiet(&plan.source, &plan.executable)?;

    if plan.destination.exists() && std::fs::read_dir(&plan.destination)?.next().is_some() {
        bail!(
            "destination {} already has a profile in it. Move it aside first — tabd will not \
             overwrite a profile.",
            plan.destination.display()
        );
    }

    // Held for everything below. It is the *same* lock a verification browser
    // takes (`TABD_VISUAL_PROFILE_DIR=<staging> tabd browser`), so a cutover,
    // a discard, a second import and a verification launch can never overlap
    // with each other.
    let _staging_lock = lock_staging(plan)?;

    // The watcher starts **before** the scan, not between the scan and the
    // copy. A browser that ran during a long scan could add files to
    // directories already walked; the copy would then use a stale file list
    // and every later check would still pass, with data missing from staging.
    let occupied = Arc::new(AtomicBool::new(false));
    let watcher = spawn_occupancy_watch(vec![plan.source.clone()], occupied.clone());

    let scanned = scan(&plan.source);
    if occupied.load(Ordering::Acquire) {
        watcher.stop();
        bail!(
            "a browser opened {} while it was being examined; try again",
            plan.source.display()
        );
    }
    let scan = match scanned {
        Ok(scan) => scan,
        Err(err) => {
            watcher.stop();
            return Err(err);
        }
    };
    let before = scan.digest.clone();

    if let Some(parent) = plan.staging.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    // `create_dir`, not `create_dir_all`: it must fail if staging already
    // exists, so two imports cannot interleave into one tree. Mode 0700 from
    // the start — this is about to hold every cookie and saved password the
    // person has, and inheriting the umask would leave it 0755.
    {
        use std::os::unix::fs::DirBuilderExt;
        if let Err(err) = std::fs::DirBuilder::new().mode(0o700).create(&plan.staging) {
            watcher.stop();
            return Err(anyhow::Error::new(err)).with_context(|| {
                format!(
                    "create staging {} (remove it with `tabd profile discard` if a previous \
                     import was interrupted)",
                    plan.staging.display()
                )
            });
        }
    }

    eprintln!(
        "copying {} ({:.1} GiB in {} files) -> {}",
        plan.source.display(),
        scan.bytes as f64 / (1024.0 * 1024.0 * 1024.0),
        scan.files.len(),
        plan.staging.display()
    );

    let result = copy_tree(plan, &scan, &occupied);
    watcher.stop();

    let discard_staging = || {
        let _ = std::fs::remove_dir_all(&plan.staging);
    };

    if let Err(err) = result {
        discard_staging();
        return Err(err);
    }
    // The latched flag, re-read after the watcher has been joined. The copy
    // loop only tests it between files, so a browser that appeared during the
    // last (possibly large) file and exited again would otherwise pass both
    // the final marker check and the digest comparison.
    if occupied.load(Ordering::Acquire) {
        discard_staging();
        bail!(
            "a browser opened {} while it was being copied; staging discarded, try again",
            plan.source.display()
        );
    }
    if let Some(marker) = occupancy_marker(&plan.source) {
        discard_staging();
        bail!(
            "a browser opened {} during the copy ({marker}); staging discarded, try again",
            plan.source.display()
        );
    }
    let after = match tree_digest(&plan.source) {
        Ok(digest) => digest,
        Err(err) => {
            discard_staging();
            return Err(err);
        }
    };
    if before != after {
        discard_staging();
        bail!(
            "{} changed while it was being copied; staging discarded, try again with the \
             browser fully quit",
            plan.source.display()
        );
    }

    // Recorded now, from the post-copy digest: cutover compares the source
    // against *this*, which is what catches the source being used again
    // between import and cutover.
    let state = ImportState {
        source: plan.source.clone(),
        executable: plan.executable.clone(),
        digest: after,
    };
    let state_file = state_path(&plan.destination);
    platform::write_sidecar(
        &state_file,
        &serde_json::to_vec_pretty(&state).context("serialize import state")?,
    )?;

    // Bind staging to the importing browser **now**, before the human is
    // invited to verify it. Otherwise the verification launch binds whatever
    // `discover_chromium` happens to find, and if that is a different browser
    // it opens — and therefore rewrites — this data. Cutover would only notice
    // afterwards.
    platform::write_sidecar(
        &binding_path(&plan.staging),
        plan.executable.to_string_lossy().as_bytes(),
    )?;

    eprintln!(
        "\nStaged. The original at {} has not been touched.\n\n\
         Check the copy by actually using it — logins, extensions, bookmarks:\n\
         \x20   BROWSER_EXECUTABLE={} TABD_VISUAL_PROFILE_DIR={} {} browser --base-dir {} https://example.com\n\
         Then quit that browser and stop its daemon:\n\
         \x20   {} daemon stop --base-dir {}\n\n\
         If it looks right:   {} profile cutover --from {} --to {}\n\
         If it looks wrong:   {} profile discard --from {} --to {}",
        plan.source.display(),
        shell_quote(&plan.executable),
        shell_quote(&plan.staging),
        tabd_command(),
        shell_quote(&plan.verify_base),
        tabd_command(),
        shell_quote(&plan.verify_base),
        tabd_command(),
        shell_quote(&plan.source),
        shell_quote(&plan.destination),
        tabd_command(),
        shell_quote(&plan.source),
        shell_quote(&plan.destination),
    );
    Ok(())
}

/// This binary's path, quoted, for the instructions printed to the user — it
/// may not be on `$PATH` yet.
fn tabd_command() -> String {
    std::env::current_exe()
        .map(|exe| shell_quote(&exe))
        .unwrap_or_else(|_| "tabd".to_string())
}

/// Take the staging lock, translating the lock error into advice.
fn lock_staging(plan: &Plan) -> Result<platform::ProfileLock> {
    platform::ProfileLock::acquire(&plan.staging).with_context(|| {
        format!(
            "another tabd is using {} — if a verification browser is open on it, quit that first",
            plan.staging.display()
        )
    })
}

struct Watcher {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Watcher {
    fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Watch these profiles for a browser opening one, for as long as the caller
/// is working on them.
/// This is the strong check: a before/after comparison would miss a browser
/// that started and exited inside the window, but a `Singleton*` entry exists
/// for the whole of a browser's life.
fn spawn_occupancy_watch(profiles: Vec<PathBuf>, occupied: Arc<AtomicBool>) -> Watcher {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_thread = stop.clone();
    let handle = std::thread::spawn(move || {
        while !stop_for_thread.load(Ordering::Acquire) {
            if profiles.iter().any(|p| occupancy_marker(p).is_some()) {
                occupied.store(true, Ordering::Release);
                return;
            }
            std::thread::sleep(OCCUPANCY_POLL);
        }
    });
    Watcher {
        stop,
        handle: Some(handle),
    }
}

fn copy_tree(plan: &Plan, scan: &Scan, occupied: &AtomicBool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    // Staging was created 0700. Widen it to the source's own mode only at the
    // very end, so the tree is never readable by anyone else while it is
    // filling up with cookies and passwords.
    let source_root_mode = plan
        .source
        .symlink_metadata()
        .ok()
        .map(|meta| meta.permissions().mode());

    // Directories first, shortest path first, so every parent exists.
    for relative in &scan.dirs {
        let target = plan.staging.join(relative);
        std::fs::create_dir_all(&target).with_context(|| format!("create {}", target.display()))?;
        if let Ok(meta) = plan.source.join(relative).symlink_metadata() {
            let _ = std::fs::set_permissions(
                &target,
                std::fs::Permissions::from_mode(meta.permissions().mode()),
            );
        }
    }

    // Test-only seam. Copying a real 9 GiB profile takes minutes, but a
    // synthetic one finishes in milliseconds on a fast disk — which makes the
    // mid-copy interlock (the strongest guarantee here) impossible to
    // exercise deterministically across machines. Unset in normal use; the
    // read happens once, outside the loop.
    let per_file_delay = std::env::var("TABD_PROFILE_TEST_SLOW_COPY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map(Duration::from_millis);

    let mut copied = 0u64;
    for relative in &scan.files {
        if let Some(delay) = per_file_delay {
            std::thread::sleep(delay);
        }
        if occupied.load(Ordering::Acquire) {
            bail!(
                "a browser opened {} while it was being copied; nothing was published",
                plan.source.display()
            );
        }
        let from = plan.source.join(relative);
        let to = plan.staging.join(relative);
        match std::fs::copy(&from, &to) {
            Ok(_) => copied += 1,
            // A profile churns while it is closed too (an updater tidying up).
            // A file that vanished between the scan and the copy is not a
            // reason to fail the whole import.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(anyhow::Error::new(err))
                    .with_context(|| format!("copy {} -> {}", from.display(), to.display()));
            }
        }
        if copied.is_multiple_of(2000) {
            eprint!("\r  {copied}/{} files", scan.files.len());
        }
    }
    eprint!("\r  {copied}/{} files\n", scan.files.len());

    if let Some(mode) = source_root_mode {
        let _ = std::fs::set_permissions(&plan.staging, std::fs::Permissions::from_mode(mode));
    }
    Ok(())
}

// -- Cutover ----------------------------------------------------------------

/// Publish the staged copy: one atomic rename, under both locks.
pub fn cutover(plan: &Plan) -> Result<()> {
    // `symlink_metadata`, not `is_dir()`: a `<destination>.staging` symlink
    // pointing at the source would satisfy `is_dir()`, and the rename would
    // then publish an alias to the original instead of an independent copy.
    let staging_meta = std::fs::symlink_metadata(&plan.staging).with_context(|| {
        format!(
            "nothing staged at {} — run `tabd profile import` first",
            plan.staging.display()
        )
    })?;
    if !staging_meta.file_type().is_dir() {
        bail!(
            "{} is not a directory; refusing to publish it",
            plan.staging.display()
        );
    }

    // Taken before anything is inspected, and before the destination lock, so
    // the ordering is always staging-then-destination and two tabd processes
    // cannot deadlock. This is also what stops a cutover renaming an import
    // that is still running, or racing an open verification browser.
    let _staging_lock = lock_staging(plan)?;

    let state_file = state_path(&plan.destination);
    let state: ImportState = match std::fs::read(&state_file) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("parse {}", state_file.display()))?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => bail!(
            "{} has no import record ({} is missing), so it cannot be verified as something \
             tabd staged. Discard it and import again.",
            plan.staging.display(),
            state_file.display()
        ),
        Err(err) => {
            return Err(anyhow::Error::new(err))
                .with_context(|| format!("read {}", state_file.display()));
        }
    };
    if state.source != plan.source {
        bail!(
            "this staging tree was imported from {}, not {}",
            state.source.display(),
            plan.source.display()
        );
    }

    // Occupancy of the *staging* tree too, which the design requires: a
    // browser launched directly at staging (not through tabd) holds no flock
    // of ours, so the lock alone does not prove it is idle.
    //
    // Watched for the whole of validation rather than sampled once. Re-reading
    // the source's digest below walks the entire tree, which on a real profile
    // takes long enough for a browser to open and still be running when the
    // rename happens.
    let occupied = Arc::new(AtomicBool::new(false));
    let watcher = spawn_occupancy_watch(
        vec![plan.source.clone(), plan.staging.clone()],
        occupied.clone(),
    );
    let validate = || -> Result<()> {
        if let Some(marker) = occupancy_marker(&plan.staging) {
            bail!(
                "{} is open in a browser ({marker}); quit the verification browser before \
                 publishing",
                plan.staging.display()
            );
        }
        // Checked against the browser recorded at import, not whatever
        // `$BROWSER_EXECUTABLE` says now — otherwise changing it at cutover
        // time silently points the "is it running" check at another process.
        assert_quiet(&plan.source, &state.executable)
    };
    if let Err(err) = validate() {
        watcher.stop();
        return Err(err);
    }

    // The check that matters between the two commands: import, reopen the
    // original, change something, quit, cutover — every liveness check passes,
    // and without this the stale copy would be published over the top.
    let current = match tree_digest(&plan.source) {
        Ok(digest) => digest,
        Err(err) => {
            watcher.stop();
            return Err(err).with_context(|| {
                format!("re-examine {} before publishing", plan.source.display())
            });
        }
    };
    if current != state.digest {
        watcher.stop();
        bail!(
            "{} has changed since it was imported ({} files / {} bytes then, {} / {} now), so \
             the staged copy is stale and publishing it would lose that work. Run \
             `tabd profile discard` and import again.",
            plan.source.display(),
            state.digest.files,
            state.digest.bytes,
            current.files,
            current.bytes
        );
    }

    // The verification launch records its own binding. If it disagrees with
    // the importing browser, a *different* browser has already opened this
    // data — and opening a Chromium profile with another browser rewrites it.
    let staging_binding = binding_path(&plan.staging);
    if let Ok(recorded) = std::fs::read_to_string(&staging_binding) {
        let recorded = recorded.trim();
        if recorded != state.executable.to_string_lossy() {
            bail!(
                "{} was verified with {recorded}, but it was imported from {}. A Chromium \
                 profile opened by a different browser is rewritten, so this copy can no longer \
                 be trusted — discard it and import again.",
                plan.staging.display(),
                state.executable.display()
            );
        }
    }

    let _destination_lock = match platform::ProfileLock::acquire(&plan.destination) {
        Ok(lock) => lock,
        Err(err) => {
            watcher.stop();
            return Err(err);
        }
    };

    let prepare = || -> Result<()> {
        match std::fs::read_dir(&plan.destination) {
            Ok(mut entries) => {
                if entries.next().is_some() {
                    bail!(
                        "destination {} is not empty; refusing to publish over an existing profile",
                        plan.destination.display()
                    );
                }
                // An empty directory makes `rename` fail on some platforms and
                // succeed on others; remove it so the rename is unambiguous. If
                // something recreates it in between, `rename` onto an empty dir
                // succeeds and onto a non-empty one fails with ENOTEMPTY — safe
                // either way, and no other tabd can do it while we hold the lock.
                std::fs::remove_dir(&plan.destination)
                    .with_context(|| format!("remove empty {}", plan.destination.display()))?;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(anyhow::Error::new(err))
                    .with_context(|| format!("read {}", plan.destination.display()));
            }
        }

        assert_same_filesystem(&plan.staging, &plan.destination)?;

        // The binding goes down **before** the rename. If it were written
        // after, a full disk or an unwritable existing `.browser` would leave
        // the profile published but unbound — or bound to an earlier browser —
        // and the daemon trusts that file. A binding with no profile beside
        // it, the other way round, is inert.
        let binding = binding_path(&plan.destination);
        platform::write_sidecar(&binding, state.executable.to_string_lossy().as_bytes())
            .with_context(|| format!("record browser binding {}", binding.display()))
    };
    if let Err(err) = prepare() {
        watcher.stop();
        return Err(err);
    }

    // Only now: the directory preparation and the binding's `sync_all` above
    // can block long enough for a browser to open staging, and the rename is
    // the irreversible step. The latched flag catches a browser that came and
    // went; the direct checks catch one that is still there.
    watcher.stop();
    if occupied.load(Ordering::Acquire) {
        bail!(
            "a browser opened {} or {} while the publish was being prepared; nothing was \
             renamed, try again",
            plan.source.display(),
            plan.staging.display()
        );
    }
    validate()?;

    std::fs::rename(&plan.staging, &plan.destination).with_context(|| {
        format!(
            "publish {} -> {}",
            plan.staging.display(),
            plan.destination.display()
        )
    })?;

    // Removed while both locks are still held. Unlinking a lock file after
    // releasing it is a race: another import can have acquired that inode by
    // then, and the unlink would let a third process create and lock a
    // different inode for the same path. So `<staging>.lock` is never removed
    // at all — it is an empty file, and keeping it is what keeps the lock
    // meaningful.
    let _ = std::fs::remove_file(&staging_binding);
    let _ = std::fs::remove_file(&state_file);
    drop(_destination_lock);
    drop(_staging_lock);

    eprintln!(
        "Published {} -> {}\n\n\
         Your original profile is untouched at {}.\n\
         To roll back: point your default browser at {} again and delete {}.",
        plan.staging.display(),
        plan.destination.display(),
        plan.source.display(),
        state.executable.display(),
        plan.destination.display()
    );
    Ok(())
}

/// A rename is only atomic within one filesystem. Staging is a sibling of the
/// destination, so this holds by construction — but "by construction" is worth
/// asserting when the failure mode is a half-published profile.
fn assert_same_filesystem(staging: &Path, destination: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let staging_dev = staging.metadata()?.dev();
    let parent = destination.parent().unwrap_or_else(|| Path::new("/"));
    let parent_dev = std::fs::metadata(parent)
        .with_context(|| format!("stat {}", parent.display()))?
        .dev();
    if staging_dev != parent_dev {
        bail!(
            "{} and {} are on different filesystems, so publishing could not be atomic",
            staging.display(),
            parent.display()
        );
    }
    Ok(())
}

// -- Discard ----------------------------------------------------------------

/// Delete the staging tree. It holds a copy of the human's real cookies and
/// passwords, so a command that deletes exactly the right directory is safer
/// than telling someone to type `rm -rf` at one.
pub fn discard(plan: &Plan) -> Result<()> {
    if plan.staging.extension() != Some(OsStr::new("staging")) {
        bail!(
            "refusing to delete {}: not a staging directory",
            plan.staging.display()
        );
    }
    // The lock comes **before** the existence check, not after it. Otherwise a
    // concurrent import can create staging and write its record in the gap,
    // and this would then delete that record — leaving a staged copy that can
    // never be published.
    let lock = lock_staging(plan)?;
    if !plan.staging.exists() {
        eprintln!("nothing staged at {}", plan.staging.display());
        let _ = std::fs::remove_file(state_path(&plan.destination));
        drop(lock);
        return Ok(());
    }
    if let Some(marker) = occupancy_marker(&plan.staging) {
        bail!(
            "{} is open in a browser ({marker}); quit it before discarding",
            plan.staging.display()
        );
    }
    std::fs::remove_dir_all(&plan.staging)
        .with_context(|| format!("remove {}", plan.staging.display()))?;
    // Under the lock, and never the lock file itself — see the note in
    // `cutover`.
    let _ = std::fs::remove_file(binding_path(&plan.staging));
    let _ = std::fs::remove_file(state_path(&plan.destination));
    drop(lock);
    eprintln!("removed {}", plan.staging.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disposable_caches_are_excluded_wherever_they_sit() {
        for path in [
            "Default/Cache",
            "Default/Cache/index",
            "Default/Code Cache/js/x",
            "Default/GPUCache",
            "Profile 2/DawnCache/a",
            "GrShaderCache/b",
            "component_crx_cache/x",
        ] {
            assert!(is_excluded(Path::new(path)), "{path} should be excluded");
        }
    }

    #[test]
    fn persistent_storage_is_never_excluded() {
        // The reason the exclusion is a name list and not a `*Cache*` glob:
        // CacheStorage is real website data, and dropping ScriptCache alone
        // leaves registered service workers pointing at missing scripts.
        for path in [
            "Default/Service Worker/CacheStorage/x",
            "Default/Service Worker/ScriptCache/y",
            "Default/Service Worker/Database/z",
            "Default/IndexedDB/a",
            "Default/Local Storage/leveldb/b",
            "Default/Session Storage/c",
            "Default/Cookies",
            "Default/Login Data",
            "Default/Extensions/abc/manifest.json",
            "Local State",
            "Default/shared_proto_db/d",
            "Default/blob_storage/e",
        ] {
            assert!(!is_excluded(Path::new(path)), "{path} must be copied");
        }
    }

    #[test]
    fn singleton_and_crash_entries_are_dropped_at_the_root_only() {
        // These are a symlink, a socket and a marker — not things to copy, and
        // meaningless in another directory. Excluding them *before* the
        // symlink check is also what stops SingletonLock aborting the import.
        for name in [
            "SingletonLock",
            "SingletonSocket",
            "SingletonCookie",
            "Crashpad",
            "lockfile",
        ] {
            assert!(is_excluded(Path::new(name)), "{name} should be excluded");
        }
        // …but a file a site happens to name the same, deeper in, is data.
        assert!(!is_excluded(Path::new("Default/Local Storage/lockfile")));
        assert!(!is_excluded(Path::new("Default/Extensions/SingletonLock")));
    }

    #[test]
    fn extension_assets_named_cache_are_kept() {
        // An extension's own bundled `Cache/` is referenced by its manifest
        // and is not regenerable. Matching cache names at any depth dropped
        // it — and dropped it from the digest too, so every consistency check
        // still passed on an incomplete extension.
        assert!(!is_excluded(Path::new(
            "Default/Extensions/abcdefgh/1.2.3_0/Cache/worker.js"
        )));
        assert!(!is_excluded(Path::new(
            "Default/Local Extension Settings/abc/Code Cache/x"
        )));
        // The browser's own caches, at the two levels they actually live at.
        assert!(is_excluded(Path::new("Default/Cache/index")));
        assert!(is_excluded(Path::new("Profile 2/Code Cache/js/x")));
        assert!(is_excluded(Path::new("GrShaderCache/a")));
        assert!(is_excluded(Path::new("GPUCache/a")));
    }

    #[test]
    fn a_wrapper_script_launch_still_counts_as_running() {
        // Chromium's Linux wrapper does `exec -a "$0"`, so argv[0] stays
        // /usr/bin/google-chrome while the binary is elsewhere: the canonical
        // path never appears in `ps`.
        let canonical = Path::new("/opt/google/chrome/google-chrome");
        assert!(command_is_browser(
            "/usr/bin/google-chrome --enable-features=X",
            canonical
        ));
        // The direct-path case, and macOS, where the name has a space and
        // tokenizing argv[0] would not work.
        assert!(command_is_browser(
            "/opt/google/chrome/google-chrome --type=zygote",
            canonical
        ));
        assert!(command_is_browser(
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome --no-first-run",
            Path::new("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
        ));
        // Something else entirely is not the browser.
        assert!(!command_is_browser("/usr/bin/firefox", canonical));
        assert!(!command_is_browser("", canonical));
        // A mention deeper in the command line is not argv[0]; the substring
        // test is what would catch a genuine one, and this is not that.
        assert!(!command_is_browser(
            "/usr/bin/grep google-chrome /var/log/syslog",
            canonical
        ));
    }

    #[test]
    fn staging_is_a_sibling_of_the_destination() {
        // Sibling, so the publish rename stays within one filesystem — and
        // never a directory *inside* the destination, whatever the spelling.
        assert_eq!(
            staging_path(Path::new("/data/tabd/profile")),
            PathBuf::from("/data/tabd/profile.staging")
        );
        assert_eq!(
            staging_path(Path::new("/data/tabd/profile/")),
            PathBuf::from("/data/tabd/profile.staging")
        );
    }

    #[test]
    fn scan_refuses_symlinks_rather_than_copying_them() {
        // A copied link is followed by Chromium during the verification
        // launch, and an absolute one reaches back into the original profile.
        let dir = tempfile::TempDir::new().expect("tempdir");
        let source = dir.path().join("src");
        std::fs::create_dir_all(source.join("Default")).expect("mkdir");
        std::fs::write(source.join("Default/Cookies"), b"x").expect("write");
        std::os::unix::fs::symlink("/etc/passwd", source.join("Default/sneaky")).expect("symlink");

        let err = scan(&source).expect_err("symlink must be refused");
        let msg = err.to_string();
        assert!(msg.contains("Default/sneaky"), "{msg}");
        assert!(msg.contains("symlink"), "{msg}");
    }

    #[test]
    fn scan_collects_files_and_skips_excluded_trees() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let source = dir.path().join("src");
        std::fs::create_dir_all(source.join("Default/Cache")).expect("mkdir");
        std::fs::create_dir_all(source.join("Default/Service Worker/CacheStorage")).expect("mkdir");
        std::fs::write(source.join("Default/Cookies"), b"real").expect("write");
        std::fs::write(source.join("Default/Cache/junk"), b"junk").expect("write");
        std::fs::write(
            source.join("Default/Service Worker/CacheStorage/keep"),
            b"k",
        )
        .expect("write");
        // An excluded symlink must not trip the refusal.
        std::os::unix::fs::symlink("nowhere-12345", source.join("SingletonLock")).expect("symlink");

        let scan = scan(&source).expect("scan");
        let files: Vec<String> = scan.files.iter().map(|p| p.display().to_string()).collect();
        assert!(files.contains(&"Default/Cookies".to_string()), "{files:?}");
        assert!(
            files.contains(&"Default/Service Worker/CacheStorage/keep".to_string()),
            "{files:?}"
        );
        assert!(!files.iter().any(|f| f.contains("Cache/junk")), "{files:?}");
        assert!(
            !scan.dirs.iter().any(|d| d.ends_with("Cache")),
            "{:?}",
            scan.dirs
        );
    }

    #[test]
    fn occupancy_sees_a_dangling_singleton_symlink() {
        // SingletonLock points at `<host>-<pid>`, which usually does not
        // resolve — `exists()` would say no and we would copy a live profile.
        let dir = tempfile::TempDir::new().expect("tempdir");
        assert_eq!(occupancy_marker(dir.path()), None);
        std::os::unix::fs::symlink("host-99999", dir.path().join("SingletonLock"))
            .expect("symlink");
        assert_eq!(
            occupancy_marker(dir.path()).as_deref(),
            Some("SingletonLock")
        );
    }

    #[test]
    fn the_digest_notices_any_change_to_retained_data() {
        // The reason this replaced a seven-file manifest: bookmarks,
        // IndexedDB, extension storage and a second profile all live outside
        // that list, and a stale copy of them would have published silently.
        let dir = tempfile::TempDir::new().expect("tempdir");
        let source = dir.path().join("src");
        std::fs::create_dir_all(source.join("Default/IndexedDB")).expect("mkdir");
        std::fs::create_dir_all(source.join("Profile 2")).expect("mkdir");
        std::fs::write(source.join("Default/Bookmarks"), b"a").expect("write");
        let baseline = tree_digest(&source).expect("digest");

        // Unchanged tree, unchanged digest.
        assert_eq!(baseline, tree_digest(&source).expect("digest"));

        for change in [
            "Default/IndexedDB/new",
            "Profile 2/Cookies",
            "Default/Bookmarks",
        ] {
            let before = tree_digest(&source).expect("digest");
            std::fs::write(source.join(change), b"changed-content").expect("write");
            assert_ne!(
                before,
                tree_digest(&source).expect("digest"),
                "a change to {change} must be visible"
            );
        }
    }

    #[test]
    fn the_digest_ignores_disposable_caches() {
        // Otherwise a browser-free machine still churns them and every cutover
        // would refuse.
        let dir = tempfile::TempDir::new().expect("tempdir");
        let source = dir.path().join("src");
        std::fs::create_dir_all(source.join("Default/Cache")).expect("mkdir");
        std::fs::write(source.join("Default/Cookies"), b"c").expect("write");
        let before = tree_digest(&source).expect("digest");
        std::fs::write(source.join("Default/Cache/junk"), b"junk").expect("write");
        assert_eq!(before, tree_digest(&source).expect("digest"));
    }
}
