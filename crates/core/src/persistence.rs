//! The driver for `--data-dir`: loads each service's state at start, saves it while the emulator runs and
//! when it stops. What a service writes is its own business (see [`crate::persist`] and `docs/services/persistence.md`);
//! this module decides when, where, and what to tell the user.
//!
//! Layout of a data directory:
//!
//! ```text
//! <data-dir>/LOCK          process id of the emulator using the directory (held with an OS file lock)
//! <data-dir>/MANIFEST      JSON: which azemu wrote the directory, when, and the file versions of each service
//! <data-dir>/<service>/    the files of one service (mode 0700)
//! ```

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::sync::watch;

use crate::service::Service;

/// Layout version of the data directory itself (the `MANIFEST` and the `LOCK`), not of any service's files.
const LAYOUT_VERSION: u32 = 1;
const LOCK_FILE: &str = "LOCK";
const MANIFEST_FILE: &str = "MANIFEST";
/// The default `--save-interval`, seconds.
pub const DEFAULT_SAVE_INTERVAL_SECS: u64 = 15;

/// What the embedding code (or the command line) asks for.
#[derive(Debug, Clone)]
pub struct PersistConfig {
    pub dir: PathBuf,
    /// Zero: save only on request and at shutdown.
    pub interval: Duration,
    /// An unreadable service directory is renamed aside and that service starts empty, instead of
    /// stopping the emulator.
    pub discard_unreadable: bool,
}

/// How one service's save went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveOutcome {
    /// Written now.
    Saved,
    /// Nothing changed since the last save (or load).
    Unchanged,
    /// Failed; the message names the file when the service could.
    Failed(String),
}

/// The result of a save for one service, as returned by [`RunningEmulator::save_now`](crate::RunningEmulator::save_now).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceSave {
    pub service: &'static str,
    pub outcome: SaveOutcome,
}

#[derive(Default, Clone)]
struct Status {
    /// The generation the files on disk correspond to.
    saved_gen: Option<u64>,
    last_attempt: Option<OffsetDateTime>,
    last_ok: Option<OffsetDateTime>,
    last_error: Option<String>,
    /// Set when this service's directory was found unreadable and moved aside at start.
    discarded_to: Option<String>,
}

struct Entry {
    service: Arc<dyn Service>,
    name: &'static str,
    dir: PathBuf,
    /// Held for the whole of a save, so two saves of one service never overlap.
    serial: Arc<tokio::sync::Mutex<()>>,
    status: Mutex<Status>,
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct ManifestService {
    /// The azemu version that saved this service's files last.
    #[serde(default)]
    saved_by: String,
    #[serde(default)]
    saved_at: String,
    /// State files with a header (`AZEMU-STATE <kind> <version>`): file name, kind, version.
    #[serde(default)]
    files: BTreeMap<String, ManifestFile>,
}

#[derive(Serialize, Deserialize, Clone)]
struct ManifestFile {
    kind: String,
    version: u32,
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    layout: u32,
    /// The azemu version that created the directory.
    created_by: String,
    created_at: String,
    /// The azemu version that wrote the directory last.
    updated_by: String,
    updated_at: String,
    #[serde(default)]
    services: BTreeMap<String, ManifestService>,
}

pub(crate) struct Persistence {
    root: PathBuf,
    interval: Duration,
    entries: Vec<Entry>,
    /// Held while the emulator runs; dropping it releases the OS lock.
    lock: Mutex<Option<fs::File>>,
    manifest: Mutex<Manifest>,
}

fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

fn stamp(t: OffsetDateTime) -> String {
    crate::time::iso8601_secs(t)
}

fn make_private_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().recursive(true).mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)
    }
}

/// A service directory made by an earlier run or copied here with wider rights is set to 0700: its files may hold
/// secrets. (The data directory itself is the user's and is left as it is.)
fn tighten(dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(m) = fs::metadata(dir) {
            if m.permissions().mode() & 0o077 != 0 {
                let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
            }
        }
    }
    #[cfg(not(unix))]
    let _ = dir;
}

impl Persistence {
    /// Open the directory, take the lock, read every service's state. Runs before any listener opens.
    pub(crate) async fn open(cfg: PersistConfig, services: &[Arc<dyn Service>]) -> Result<Arc<Persistence>> {
        // The data directory holds Key Vault keys and secrets. A new one is private; one that exists is the user's and
        // stays as it is, with a warning when others can use it.
        let created = crate::private_dir::create_private_dir(&cfg.dir)
            .with_context(|| format!("cannot create the data directory {}", cfg.dir.display()))?;
        if !created {
            if let Some(w) = crate::private_dir::open_directory_warning(
                &cfg.dir,
                "the data directory",
                "the saved state of the services, Key Vault keys and secrets included",
            ) {
                tracing::warn!("{w}");
            }
        }
        // the full path, so that `health` and the messages name the same directory whatever the working directory is
        let root = fs::canonicalize(&cfg.dir).unwrap_or_else(|_| cfg.dir.clone());
        let lock = take_lock(&root)?;
        remove_stale_temp_files_of_dead_processes(&root);
        let manifest = read_manifest(&root)?;

        let mut entries = Vec::new();
        for s in services {
            if s.persist_generation().is_none() {
                continue;
            }
            let name = s.name();
            if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
                bail!("service name '{name}' cannot be used as a directory name for --data-dir");
            }
            entries.push(Entry {
                service: s.clone(),
                name,
                dir: root.join(name),
                serial: Arc::new(tokio::sync::Mutex::new(())),
                status: Mutex::new(Status::default()),
            });
        }
        let p =
            Arc::new(Persistence { root, interval: cfg.interval, entries, lock: Mutex::new(Some(lock)), manifest: Mutex::new(manifest) });
        for i in 0..p.entries.len() {
            p.load_entry(i, cfg.discard_unreadable).await?;
        }
        // The directory now holds a MANIFEST that names this version, even when nothing is saved yet.
        let this = p.clone();
        tokio::task::spawn_blocking(move || this.write_manifest(None))
            .await
            .map_err(|e| anyhow!("writing MANIFEST failed: {e}"))?
            .with_context(|| format!("cannot write {}", p.root.join(MANIFEST_FILE).display()))?;
        Ok(p)
    }

    async fn load_entry(&self, idx: usize, discard: bool) -> Result<()> {
        let e = &self.entries[idx];
        make_private_dir(&e.dir).with_context(|| format!("cannot create {}", e.dir.display()))?;
        tighten(&e.dir);
        remove_stale_temp_files(&e.dir);
        let (svc, dir) = (e.service.clone(), e.dir.clone());
        let result = tokio::task::spawn_blocking(move || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| svc.load(&dir)))
                .unwrap_or_else(|_| Err(io::Error::other("the service panicked while reading its files")))
        })
        .await
        .map_err(|e| anyhow!("loading task failed: {e}"))?;
        match result {
            Ok(()) => {
                e.status.lock().saved_gen = e.service.persist_generation();
                Ok(())
            }
            Err(err) if discard => {
                // A half-loaded service must not stay: empty it, move the files aside, carry on.
                e.service.reset();
                let aside = unique_aside(&e.dir);
                fs::rename(&e.dir, &aside)
                    .with_context(|| format!("cannot move the unreadable directory {} aside ({err})", e.dir.display()))?;
                make_private_dir(&e.dir).with_context(|| format!("cannot create {}", e.dir.display()))?;
                tracing::warn!(
                    service = e.name,
                    error = %err,
                    moved_to = %aside.display(),
                    "state of this service was unreadable; moved aside, starting it empty"
                );
                let mut st = e.status.lock();
                st.saved_gen = e.service.persist_generation();
                st.discarded_to = Some(aside.display().to_string());
                Ok(())
            }
            Err(err) => Err(anyhow!(
                "cannot start: the saved state of the {name} service in {dir} is unreadable: {err}\n\
                 To start with this service empty, move the directory away (mv {dir} {dir}.old) or start with \
                 --data-discard-unreadable, which does the same and tells you where it went. \
                 To keep the data, fix or restore the file named above.",
                name = e.name,
                dir = e.dir.display(),
            )),
        }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// Release the OS lock. Called when the emulator has stopped.
    pub(crate) fn release_lock(&self) {
        self.lock.lock().take();
    }

    /// Save every service that has unsaved changes, one at a time. With `force` all of them.
    pub(crate) async fn save_all(self: &Arc<Self>, force: bool) -> Vec<ServiceSave> {
        let mut out = Vec::with_capacity(self.entries.len());
        for i in 0..self.entries.len() {
            let outcome = self.save_entry(i, force).await;
            out.push(ServiceSave { service: self.entries[i].name, outcome });
        }
        out
    }

    async fn save_entry(self: &Arc<Self>, idx: usize, force: bool) -> SaveOutcome {
        let e = &self.entries[idx];
        let serial = e.serial.clone().lock_owned().await;
        let Some(generation) = e.service.persist_generation() else {
            return SaveOutcome::Unchanged;
        };
        if !force && e.status.lock().saved_gen == Some(generation) {
            return SaveOutcome::Unchanged;
        }
        let started = std::time::Instant::now();
        let this = self.clone();
        let result = tokio::task::spawn_blocking(move || {
            let _held = serial; // released when this save is over, even if the caller stopped waiting
            let e = &this.entries[idx];
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                make_private_dir(&e.dir)?;
                e.service.save(&e.dir)
            }))
            .unwrap_or_else(|_| Err(io::Error::other("the service panicked while saving")));
            if r.is_ok() {
                // the files are on disk; a failure to update the MANIFEST is logged, not a failed save
                if let Err(err) = this.write_manifest(Some(idx)) {
                    tracing::warn!(error = %err, "cannot update MANIFEST");
                }
            }
            r
        })
        .await;
        let result = match result {
            Ok(r) => r,
            Err(join) => Err(io::Error::other(format!("the save task failed: {join}"))),
        };
        let t = now();
        let mut st = e.status.lock();
        st.last_attempt = Some(t);
        match result {
            Ok(()) => {
                // The generation was read before the save: a change made during the save leaves the
                // service ahead of it, and the next save picks that change up.
                st.saved_gen = Some(generation);
                st.last_ok = Some(t);
                st.last_error = None;
                tracing::debug!(service = e.name, ms = started.elapsed().as_millis() as u64, "state saved");
                SaveOutcome::Saved
            }
            Err(err) => {
                let msg = err.to_string();
                tracing::error!(service = e.name, dir = %e.dir.display(), error = %msg, "saving state failed; will retry");
                st.last_error = Some(msg.clone());
                SaveOutcome::Failed(msg)
            }
        }
    }

    /// The background task: every interval save what changed. Ends when `stop` turns true.
    pub(crate) fn spawn_background(self: &Arc<Self>, mut stop: watch::Receiver<bool>) -> Option<tokio::task::JoinHandle<()>> {
        if self.interval.is_zero() || self.entries.is_empty() {
            return None;
        }
        let this = self.clone();
        Some(tokio::spawn(async move {
            let mut tick = tokio::time::interval(this.interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await; // the first tick is immediate
            loop {
                tokio::select! {
                    _ = tick.tick() => { this.save_all(false).await; }
                    _ = async { let _ = stop.wait_for(|s| *s).await; } => break,
                }
            }
        }))
    }

    /// The `persistence` object of `GET /_azemu/health`.
    pub(crate) fn health(&self) -> serde_json::Value {
        let mut services = serde_json::Map::new();
        let mut any_unsaved = false;
        for e in &self.entries {
            let st = e.status.lock().clone();
            let unsaved = e.service.persist_generation() != st.saved_gen;
            any_unsaved |= unsaved;
            let result = match (&st.last_error, st.last_attempt) {
                (Some(_), _) => "error",
                (None, Some(_)) => "ok",
                (None, None) => "none",
            };
            let mut o = serde_json::json!({
                "directory": e.dir.display().to_string(),
                "lastSave": st.last_ok.map(stamp),
                "lastAttempt": st.last_attempt.map(stamp),
                "lastResult": result,
                "unsavedChanges": unsaved,
            });
            if let Some(err) = st.last_error {
                o["error"] = err.into();
            }
            if let Some(d) = st.discarded_to {
                o["unreadableStateMovedTo"] = d.into();
            }
            services.insert(e.name.to_string(), o);
        }
        serde_json::json!({
            "directory": self.root.display().to_string(),
            "saveIntervalSeconds": self.interval.as_secs(),
            "unsavedChanges": any_unsaved,
            "services": services,
        })
    }

    /// Write MANIFEST. `saved` is the service whose files were just written, if any.
    fn write_manifest(&self, saved: Option<usize>) -> io::Result<()> {
        let mut m = self.manifest.lock();
        let t = stamp(now());
        let v = env!("CARGO_PKG_VERSION");
        m.layout = LAYOUT_VERSION;
        m.updated_by = v.to_string();
        m.updated_at = t.clone();
        if let Some(i) = saved {
            let e = &self.entries[i];
            m.services.insert(e.name.to_string(), ManifestService { saved_by: v.to_string(), saved_at: t, files: scan_headers(&e.dir) });
        }
        let bytes = serde_json::to_vec_pretty(&*m).map_err(io::Error::other)?;
        crate::persist::write_atomic(&self.root.join(MANIFEST_FILE), &bytes)
    }
}

/// The state files directly inside `dir`: those that start with the `AZEMU-STATE <kind> <version>` header line.
fn scan_headers(dir: &Path) -> BTreeMap<String, ManifestFile> {
    let mut out = BTreeMap::new();
    let Ok(rd) = fs::read_dir(dir) else { return out };
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || !ent.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let mut buf = [0u8; 120];
        let Ok(mut f) = fs::File::open(ent.path()) else { continue };
        let n = f.read(&mut buf).unwrap_or(0);
        let Some(end) = buf[..n].iter().position(|b| *b == b'\n') else { continue };
        let Ok(line) = std::str::from_utf8(&buf[..end]) else { continue };
        let mut p = line.split(' ');
        if p.next() != Some("AZEMU-STATE") {
            continue;
        }
        if let (Some(kind), Some(Ok(version))) = (p.next(), p.next().map(str::parse::<u32>)) {
            out.insert(name, ManifestFile { kind: kind.to_string(), version });
        }
    }
    out
}

/// Take the OS lock on `<root>/LOCK` and write our process id into it. The lock goes away with the
/// process, so a lock file left by a process that died is simply taken over.
fn take_lock(root: &Path) -> Result<fs::File> {
    let path = root.join(LOCK_FILE);
    let mut f = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("the data directory {} cannot be written (cannot open {})", root.display(), path.display()))?;
    match f.try_lock() {
        Ok(()) => {}
        Err(fs::TryLockError::WouldBlock) => {
            let mut who = String::new();
            let _ = f.read_to_string(&mut who);
            let who = who.trim();
            bail!(
                "the data directory {} is in use by another running azemu (process {}). \
                 Stop it, or give this one its own --data-dir.",
                root.display(),
                if who.is_empty() { "unknown" } else { who }
            );
        }
        Err(fs::TryLockError::Error(e)) => {
            return Err(anyhow!("cannot lock {}: {e}", path.display()));
        }
    }
    f.set_len(0)
        .and_then(|_| f.write_all(format!("{}\n", std::process::id()).as_bytes()))
        .and_then(|_| f.sync_all())
        .with_context(|| format!("the data directory {} cannot be written (cannot write {})", root.display(), path.display()))?;
    // prove the directory accepts new files (a read-only mount can still hold an existing LOCK)
    let probe = root.join(format!(".write-test-{}", std::process::id()));
    fs::write(&probe, b"x")
        .and_then(|_| fs::remove_file(&probe))
        .with_context(|| format!("the data directory {} cannot be written", root.display()))?;
    Ok(f)
}

fn read_manifest(root: &Path) -> Result<Manifest> {
    let path = root.join(MANIFEST_FILE);
    let fresh = || {
        let t = stamp(now());
        let v = env!("CARGO_PKG_VERSION").to_string();
        Manifest {
            layout: LAYOUT_VERSION,
            created_by: v.clone(),
            created_at: t.clone(),
            updated_by: v,
            updated_at: t,
            services: BTreeMap::new(),
        }
    };
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(fresh()),
        Err(e) => return Err(anyhow!("cannot read {}: {e}", path.display())),
    };
    match serde_json::from_slice::<Manifest>(&bytes) {
        Ok(m) if m.layout > LAYOUT_VERSION => bail!(
            "{} has layout {}; this azemu understands layout {LAYOUT_VERSION} (the directory was written by a newer azemu, {})",
            path.display(),
            m.layout,
            m.updated_by
        ),
        Ok(m) => Ok(m),
        Err(e) => {
            // The state files carry their own headers; a damaged MANIFEST is replaced, not fatal.
            tracing::warn!(file = %path.display(), error = %e, "MANIFEST is unreadable; writing a new one");
            Ok(fresh())
        }
    }
}

/// `<dir>.unreadable-<UTC time>` (a counter is added when that name exists).
fn unique_aside(dir: &Path) -> PathBuf {
    let t = now();
    let base = format!(
        "{}.unreadable-{:04}{:02}{:02}T{:02}{:02}{:02}",
        dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    );
    let parent = dir.parent().unwrap_or(Path::new("."));
    let mut candidate = parent.join(&base);
    let mut n = 1;
    while candidate.exists() {
        candidate = parent.join(format!("{base}-{n}"));
        n += 1;
    }
    candidate
}

/// A kill in the middle of writing the `MANIFEST` leaves `.MANIFEST.tmp-<pid>` in the data directory itself, which the cleaning of the service
/// directories does not reach. Only a temporary file whose process no longer runs is removed: its name ends in the pid that was writing.
fn remove_stale_temp_files_of_dead_processes(root: &Path) {
    let Ok(rd) = fs::read_dir(root) else { return };
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        let Some((_, pid)) = name.rsplit_once(".tmp-") else { continue };
        if !name.starts_with('.') || !ent.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let Ok(pid) = pid.parse::<u32>() else { continue };
        if !process_is_running(pid) {
            let _ = fs::remove_file(ent.path());
        }
    }
}

/// Does a process with this id exist? Where `/proc` exists it is asked; elsewhere only this process is known to run (the data directory is
/// locked, so no other emulator writes to it).
fn process_is_running(pid: u32) -> bool {
    if pid == std::process::id() {
        return true;
    }
    Path::new("/proc/self").exists() && Path::new("/proc").join(pid.to_string()).exists()
}

/// A crash in the middle of `persist::write_atomic` leaves `.<name>.tmp-<pid>` behind; they are never state.
fn remove_stale_temp_files(dir: &Path) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') && name.contains(".tmp-") && ent.file_type().map(|t| t.is_file()).unwrap_or(false) {
            let _ = fs::remove_file(ent.path());
        }
    }
}
