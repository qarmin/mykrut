//! Copy/move with byte-level progress reporting and cancellation.
//!
//! API: caller passes a `CancelFlag` (cheap to clone, can be stored in UI state)
//! and a closure that receives `Progress` snapshots. The closure runs on the
//! worker thread — usually it just pushes to a channel that the UI drains.
//!
//! For large operations a pre-scan computes total bytes + file count so the UI
//! can display a real percentage. The pre-scan itself respects the cancel flag.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use crate::file_ops::{FileOpError, unique_destination};

/// Shared cancellation token. Worker checks this between files (and between
/// 64 KiB chunks inside a file).
#[derive(Clone, Default, Debug)]
pub struct CancelFlag(Arc<AtomicBool>);

impl CancelFlag {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    pub fn reset(&self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Shared pause token. The worker spins (sleeping) while this is set, so the
/// user can hold a long copy without cancelling it. Cancellation always wins
/// over pause.
#[derive(Clone, Default, Debug)]
pub struct PauseFlag(Arc<AtomicBool>);

impl PauseFlag {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub fn set_paused(&self, paused: bool) {
        self.0.store(paused, Ordering::Release);
    }

    pub fn is_paused(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// How to resolve a destination that already exists. The app maps these to a
/// Replace / Skip / Keep-both dialog; `unique_destination` implements Keep-both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conflict {
    /// Remove the existing destination, then write the source in its place.
    Overwrite,
    /// Leave the existing destination untouched; don't copy this source.
    Skip,
    /// Copy alongside under a " (1)"-style unique name.
    KeepBoth,
    /// Abort the whole operation.
    Cancel,
}

#[derive(Clone, Debug)]
pub struct Progress {
    pub op: Op,
    pub current: PathBuf,
    pub files_done: u64,
    pub files_total: u64,
    pub bytes_done: u64,
    pub bytes_total: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Copy,
    Move,
}

/// A single source path that could not be fully copied/moved.
#[derive(Clone, Debug)]
pub struct CopyError {
    pub path: PathBuf,
    pub message: String,
}

/// Result of a copy/move run. The operation is resilient: an error on one
/// entry is recorded here and the rest of the batch still proceeds, so callers
/// can show a Nemo-style "these items could not be copied" summary instead of
/// failing the whole operation.
#[derive(Clone, Debug, Default)]
pub struct CopyOutcome {
    /// Top-level paths processed without error.
    pub succeeded: u64,
    /// Per-path failures (path + human-readable reason).
    pub errors: Vec<CopyError>,
    /// True if the user cancelled before the batch finished.
    pub cancelled: bool,
    /// Top-level paths skipped on the user's request (conflict → Skip).
    pub skipped: u64,
    /// (source, final destination) for each top-level entry that succeeded.
    /// Final destination reflects conflict resolution (unique name / overwrite),
    /// so callers (e.g. undo) can reverse the operation precisely.
    pub dests: Vec<(PathBuf, PathBuf)>,
}

/// Throttle wrapper — only emits at most ~30 events per second so the channel
/// + UI thread don't drown in updates when copying tons of small files.
struct ProgressEmitter<F> {
    inner: F,
    last_emit: Instant,
    min_gap: Duration,
}

impl<F: FnMut(Progress)> ProgressEmitter<F> {
    fn new(inner: F) -> Self {
        Self {
            inner,
            last_emit: Instant::now() - Duration::from_secs(1),
            min_gap: Duration::from_millis(33),
        }
    }

    fn maybe_emit(&mut self, p: Progress, force: bool) {
        let now = Instant::now();
        if force || now.duration_since(self.last_emit) >= self.min_gap {
            (self.inner)(p);
            self.last_emit = now;
        }
    }
}

/// Pre-scan: walk every source path recursively to compute totals.
/// Returns (files_total, bytes_total). Respects `cancel`.
///
/// Unreadable entries (permission denied, races, etc.) are skipped and logged
/// rather than aborting the scan — the totals are only used to drive the
/// progress bar, and the copy phase reports per-item failures itself. A single
/// unreadable sub-directory must not turn the whole copy into a no-op.
pub fn prescan(srcs: &[PathBuf], cancel: &CancelFlag) -> (u64, u64) {
    let mut files = 0u64;
    let mut bytes = 0u64;
    for src in srcs {
        if cancel.is_cancelled() {
            return (files, bytes);
        }
        let meta = match std::fs::symlink_metadata(src) {
            Ok(m) => m,
            Err(err) => {
                warn!(path = %src.display(), ?err, "prescan: skipping unreadable source");
                continue;
            }
        };
        let ft = meta.file_type();
        if ft.is_dir() && !ft.is_symlink() {
            for entry in walkdir::WalkDir::new(src).follow_links(false) {
                if cancel.is_cancelled() {
                    return (files, bytes);
                }
                let entry = match entry {
                    Ok(e) => e,
                    Err(err) => {
                        debug!(?err, "prescan: skipping unreadable entry");
                        continue;
                    }
                };
                match entry.metadata() {
                    Ok(m) if m.is_file() => {
                        files += 1;
                        bytes += m.len();
                    }
                    Ok(_) => {}
                    Err(err) => debug!(?err, "prescan: metadata read failed"),
                }
            }
        } else {
            files += 1;
            bytes += meta.len();
        }
    }
    (files, bytes)
}

/// Copy or move multiple paths into `dest_dir` with progress + cancellation.
///
/// On name collisions, picks a unique name via `unique_destination` (the
/// historic "keep both" behaviour). For interactive Replace/Skip/Keep-both
/// resolution use [`run_with`].
///
/// On EXDEV (cross-FS) during move, falls back to copy+remove for that path.
///
/// Resilient: a failure on one entry is recorded in the returned
/// [`CopyOutcome`] and the rest of the batch still proceeds.
pub fn run<F>(srcs: &[PathBuf], dest_dir: &Path, op: Op, cancel: &CancelFlag, on_progress: F) -> CopyOutcome
where
    F: FnMut(Progress),
{
    run_with(
        srcs,
        dest_dir,
        op,
        cancel,
        &PauseFlag::new(),
        |_| Conflict::KeepBoth,
        on_progress,
    )
}

/// Like [`run`] but with pause support and an interactive conflict `resolver`.
///
/// `resolver(existing_dest)` is called whenever a destination already exists and
/// isn't a directory-into-directory merge (those merge silently). It runs on the
/// worker thread and may block (e.g. waiting on a dialog). Returning
/// [`Conflict::Cancel`] aborts the whole batch.
pub fn run_with<F, R>(
    srcs: &[PathBuf],
    dest_dir: &Path,
    op: Op,
    cancel: &CancelFlag,
    pause: &PauseFlag,
    resolver: R,
    on_progress: F,
) -> CopyOutcome
where
    F: FnMut(Progress),
    R: FnMut(&Path) -> Conflict,
{
    let (files_total, bytes_total) = prescan(srcs, cancel);
    info!(
        files_total,
        bytes_total,
        op = ?op,
        dest = %dest_dir.display(),
        "copy/move begin"
    );

    let mut emitter = ProgressEmitter::new(on_progress);
    let mut state = RunState {
        op,
        cancel: cancel.clone(),
        pause: pause.clone(),
        resolver,
        files_total,
        bytes_total,
        files_done: 0,
        bytes_done: 0,
        errors: Vec::new(),
        skipped: 0,
    };

    let mut count = 0u64;
    let mut cancelled = false;
    let mut dests: Vec<(PathBuf, PathBuf)> = Vec::new();
    for src in srcs {
        if cancel.is_cancelled() {
            warn!("cancelled");
            cancelled = true;
            break;
        }
        let Some(name) = src.file_name() else {
            continue;
        };
        // Pass the natural destination; copy_entry/move_entry resolve any
        // collision via the resolver (default = keep-both → unique name).
        let dest = dest_dir.join(name);

        let res = match op {
            Op::Copy => state.copy_entry(src, &dest, &mut emitter),
            Op::Move => state.move_entry(src, &dest, &mut emitter),
        };

        match res {
            Ok(Some(final_dest)) => {
                count += 1;
                dests.push((src.clone(), final_dest));
            }
            // Skipped on the user's request (conflict → Skip) — not an error.
            Ok(None) => {}
            Err(err) => {
                warn!(path = %src.display(), ?err, "skip");
                state.errors.push(CopyError {
                    path: src.clone(),
                    message: err.to_string(),
                });
            }
        }
    }

    emitter.maybe_emit(
        Progress {
            op,
            current: PathBuf::new(),
            files_done: state.files_done,
            files_total: state.files_total,
            bytes_done: state.bytes_done,
            bytes_total: state.bytes_total,
        },
        true,
    );

    if cancel.is_cancelled() {
        cancelled = true;
    }
    let errors = std::mem::take(&mut state.errors);
    let skipped = state.skipped;
    info!(count, errors = errors.len(), cancelled, skipped, "copy/move done");
    CopyOutcome {
        succeeded: count,
        errors,
        cancelled,
        skipped,
        dests,
    }
}

struct RunState<R> {
    op: Op,
    cancel: CancelFlag,
    pause: PauseFlag,
    resolver: R,
    files_total: u64,
    bytes_total: u64,
    files_done: u64,
    bytes_done: u64,
    /// Failures encountered while recursing into directories (the top-level
    /// loop in `run` records its own; this collects deeper ones).
    errors: Vec<CopyError>,
    /// Count of entries skipped via [`Conflict::Skip`].
    skipped: u64,
}

impl<R: FnMut(&Path) -> Conflict> RunState<R> {
    /// Resolve `dest` against an existing entry. Returns `Some(final_dest)` to
    /// proceed (possibly a renamed path, or the same path after removing what
    /// was there), or `None` to skip this entry. Directory-into-directory is a
    /// silent merge and never reaches here.
    fn resolve_dest(&mut self, dest: &Path) -> Result<Option<PathBuf>, FileOpError> {
        match (self.resolver)(dest) {
            Conflict::Skip => {
                self.skipped += 1;
                Ok(None)
            }
            Conflict::Cancel => {
                self.cancel.cancel();
                Ok(None)
            }
            Conflict::KeepBoth => {
                let parent = dest.parent().unwrap_or_else(|| Path::new("."));
                let name = dest
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                Ok(Some(unique_destination(parent, &name)))
            }
            Conflict::Overwrite => {
                remove_recursive(dest)?;
                Ok(Some(dest.to_path_buf()))
            }
        }
    }

    /// Block while paused, returning early if cancelled. Cheap when not paused.
    fn wait_if_paused(&self) {
        while self.pause.is_paused() && !self.cancel.is_cancelled() {
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Returns the final destination written on success, or `None` if the entry
    /// was skipped (conflict → Skip, or cancelled).
    fn copy_entry<F: FnMut(Progress)>(
        &mut self,
        src: &Path,
        dest: &Path,
        emitter: &mut ProgressEmitter<F>,
    ) -> Result<Option<PathBuf>, FileOpError> {
        self.wait_if_paused();
        if self.cancel.is_cancelled() {
            return Ok(None);
        }
        let meta = std::fs::symlink_metadata(src)?;
        let ft = meta.file_type();

        // Resolve a pre-existing destination. Directory-into-existing-directory
        // is a silent merge (children get resolved individually as we recurse);
        // anything else consults the resolver (Replace / Skip / Keep both).
        let dest_meta = std::fs::symlink_metadata(dest).ok();
        let dest_is_dir = dest_meta.as_ref().is_some_and(|m| m.file_type().is_dir());
        let merge_dirs = dest_is_dir && ft.is_dir() && !ft.is_symlink();
        let dest_buf;
        let dest: &Path = if dest_meta.is_some() && !merge_dirs {
            match self.resolve_dest(dest)? {
                Some(d) => {
                    dest_buf = d;
                    &dest_buf
                }
                None => return Ok(None),
            }
        } else {
            dest
        };

        if ft.is_symlink() {
            Self::copy_symlink(src, dest)?;
            self.files_done += 1;
            // Force on file boundary so the UI count stays accurate even for fast tiny files.
            self.emit(emitter, src.to_path_buf(), true);
            return Ok(Some(dest.to_path_buf()));
        }

        if ft.is_dir() {
            std::fs::create_dir_all(dest)?;
            for entry in std::fs::read_dir(src)? {
                let entry = match entry {
                    Ok(e) => e,
                    Err(err) => {
                        warn!(dir = %src.display(), ?err, "skip unreadable child");
                        self.errors.push(CopyError {
                            path: src.to_path_buf(),
                            message: err.to_string(),
                        });
                        continue;
                    }
                };
                let child_src = entry.path();
                let child_dest = dest.join(entry.file_name());
                if self.cancel.is_cancelled() {
                    return Ok(Some(dest.to_path_buf()));
                }
                // Record the failure and keep going with the siblings instead
                // of aborting the whole directory on the first bad child.
                if let Err(err) = self.copy_entry(&child_src, &child_dest, emitter) {
                    warn!(path = %child_src.display(), ?err, "skip");
                    self.errors.push(CopyError {
                        path: child_src,
                        message: err.to_string(),
                    });
                }
            }
            return Ok(Some(dest.to_path_buf()));
        }

        self.copy_file_chunked(src, dest, meta.len(), emitter)?;
        self.files_done += 1;
        self.emit(emitter, src.to_path_buf(), true);
        Ok(Some(dest.to_path_buf()))
    }

    fn copy_file_chunked<F: FnMut(Progress)>(
        &mut self,
        src: &Path,
        dest: &Path,
        size: u64,
        emitter: &mut ProgressEmitter<F>,
    ) -> Result<(), FileOpError> {
        use std::io::{Read, Write};

        let mut input = std::fs::File::open(src)?;
        // `create_new` (O_CREAT|O_EXCL) rather than `create`: by here the
        // destination has already been resolved (kept-both → unique name, or
        // overwrite → existing removed), so it must not exist. This also refuses
        // to follow/truncate a symlink that raced into place at `dest`, keeping
        // the write inside `dest_dir`.
        let mut output = std::fs::OpenOptions::new().write(true).create_new(true).open(dest)?;
        // Permissions are restored after the data copy (see set_permissions
        // below); the freshly created file starts at the process umask.
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            self.wait_if_paused();
            if self.cancel.is_cancelled() {
                drop(output);
                let _ = std::fs::remove_file(dest);
                return Ok(());
            }
            let n = input.read(&mut buf)?;
            if n == 0 {
                break;
            }
            #[expect(
                clippy::indexing_slicing,
                reason = "`n` is the byte count from read(), always <= buf.len()"
            )]
            output.write_all(&buf[..n])?;
            self.bytes_done += n as u64;
            self.emit(emitter, src.to_path_buf(), false);
        }
        output.flush()?;
        drop(output);
        // Preserve permissions (best effort).
        if let Ok(meta) = std::fs::metadata(src) {
            let _ = std::fs::set_permissions(dest, meta.permissions());
        }
        debug!(src = %src.display(), dest = %dest.display(), size, "file copied");
        Ok(())
    }

    fn copy_symlink(src: &Path, dest: &Path) -> Result<(), FileOpError> {
        let target = std::fs::read_link(src)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, dest)?;
        #[cfg(windows)]
        {
            let _ = target;
            return Err(FileOpError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "symlink copy not implemented on Windows",
            )));
        }
        Ok(())
    }

    /// Returns the final destination on success, or `None` if skipped.
    fn move_entry<F: FnMut(Progress)>(
        &mut self,
        src: &Path,
        dest: &Path,
        emitter: &mut ProgressEmitter<F>,
    ) -> Result<Option<PathBuf>, FileOpError> {
        self.wait_if_paused();
        if self.cancel.is_cancelled() {
            return Ok(None);
        }

        // Resolve a pre-existing destination before attempting the rename.
        let src_is_dir = std::fs::symlink_metadata(src).is_ok_and(|m| m.file_type().is_dir());
        let dest_meta = std::fs::symlink_metadata(dest).ok();
        let dest_is_dir = dest_meta.as_ref().is_some_and(|m| m.file_type().is_dir());
        let dest_buf;
        let dest: &Path = if dest_meta.is_some() {
            if src_is_dir && dest_is_dir {
                // Directory merge: rename can't merge into an existing dir, so
                // copy (resolving each child) then drop the source on success.
                let errors_before = self.errors.len();
                self.copy_entry(src, dest, emitter)?;
                if !self.cancel.is_cancelled() && self.errors.len() == errors_before {
                    remove_recursive(src)?;
                    return Ok(Some(dest.to_path_buf()));
                }
                return Ok(None);
            }
            match self.resolve_dest(dest)? {
                Some(d) => {
                    dest_buf = d;
                    &dest_buf
                }
                None => return Ok(None),
            }
        } else {
            dest
        };

        // EXDEV = 18 on Linux + macOS + BSD; rename across mounts fails with this.
        const EXDEV: i32 = 18;
        match std::fs::rename(src, dest) {
            Ok(()) => {
                // Same-FS rename is instant — bump counts approximately.
                let meta = std::fs::symlink_metadata(dest)?;
                if meta.is_dir() {
                    // Count files inside the moved dir for accurate totals.
                    if let Ok(walked) = walkdir::WalkDir::new(dest)
                        .into_iter()
                        .filter_map(Result::ok)
                        .filter(|e| e.metadata().is_ok_and(|m| m.is_file()))
                        .try_fold((0u64, 0u64), |(f, b), e| {
                            let m = e.metadata()?;
                            Ok::<_, walkdir::Error>((f + 1, b + m.len()))
                        })
                    {
                        self.files_done += walked.0;
                        self.bytes_done += walked.1;
                    }
                } else {
                    self.files_done += 1;
                    self.bytes_done += meta.len();
                }
                self.emit(emitter, src.to_path_buf(), false);
                Ok(Some(dest.to_path_buf()))
            }
            Err(e) if matches!(e.raw_os_error(), Some(c) if c == EXDEV) => {
                debug!("EXDEV fallback to copy+remove");
                let errors_before = self.errors.len();
                self.copy_entry(src, dest, emitter)?;
                // Delete the source ONLY if the copy fully succeeded and was not
                // cancelled. copy_entry now collects per-child errors instead of
                // returning Err, so a grown error count means part of the tree
                // failed to copy — removing the source would lose that data.
                if self.cancel.is_cancelled() || self.errors.len() != errors_before {
                    warn!(path = %src.display(), "EXDEV move: copy incomplete, source kept");
                    Ok(None)
                } else {
                    remove_recursive(src)?;
                    Ok(Some(dest.to_path_buf()))
                }
            }
            Err(e) => Err(e.into()),
        }
    }

    fn emit<F: FnMut(Progress)>(&self, emitter: &mut ProgressEmitter<F>, current: PathBuf, force: bool) {
        emitter.maybe_emit(
            Progress {
                op: self.op,
                current,
                files_done: self.files_done,
                files_total: self.files_total,
                bytes_done: self.bytes_done,
                bytes_total: self.bytes_total,
            },
            force,
        );
    }
}

fn remove_recursive(p: &Path) -> Result<(), std::io::Error> {
    let meta = std::fs::symlink_metadata(p)?;
    if meta.file_type().is_dir() && !meta.file_type().is_symlink() {
        std::fs::remove_dir_all(p)
    } else {
        std::fs::remove_file(p)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn tmp() -> PathBuf {
        let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let p = std::env::temp_dir().join(format!("fm-test-cp-{n:x}"));
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn prescan_counts_recursive() {
        let dir = tmp();
        let sub = dir.join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(dir.join("a.txt"), b"12345").unwrap();
        fs::write(sub.join("b.txt"), b"abc").unwrap();

        let cancel = CancelFlag::new();
        let (files, bytes) = prescan(std::slice::from_ref(&dir), &cancel);
        assert_eq!(files, 2);
        assert_eq!(bytes, 8);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn copy_with_progress_emits_at_least_one_event() {
        let src_dir = tmp();
        let dst_dir = tmp();
        fs::write(src_dir.join("a.txt"), b"hello world").unwrap();

        let cancel = CancelFlag::new();
        let events = std::cell::RefCell::new(Vec::<Progress>::new());
        let outcome = run(&[src_dir.join("a.txt")], &dst_dir, Op::Copy, &cancel, |p| {
            events.borrow_mut().push(p);
        });
        assert_eq!(outcome.succeeded, 1);
        assert!(outcome.errors.is_empty());
        assert!(!events.borrow().is_empty(), "expected ≥1 progress event");
        let last = events.borrow().last().unwrap().clone();
        assert_eq!(last.files_done, 1);
        assert_eq!(last.bytes_done, 11);
        assert!(dst_dir.join("a.txt").exists());

        let _ = fs::remove_dir_all(&src_dir);
        let _ = fs::remove_dir_all(&dst_dir);
    }

    #[test]
    fn cancel_stops_copy_mid_run() {
        let src_dir = tmp();
        let dst_dir = tmp();
        for i in 0..50 {
            fs::write(src_dir.join(format!("f{i}.txt")), vec![0u8; 8192]).unwrap();
        }

        let cancel = CancelFlag::new();
        let cancel_clone = cancel.clone();
        let copied = std::cell::RefCell::new(0u64);
        let srcs: Vec<PathBuf> = (0..50).map(|i| src_dir.join(format!("f{i}.txt"))).collect();

        let outcome = run(&srcs, &dst_dir, Op::Copy, &cancel, |p| {
            *copied.borrow_mut() = p.files_done;
            if p.files_done >= 5 {
                cancel_clone.cancel();
            }
        });

        assert!(
            outcome.succeeded < 50,
            "expected early termination, got {}",
            outcome.succeeded
        );
        assert!(outcome.cancelled, "outcome should be marked cancelled");
        let _ = fs::remove_dir_all(&src_dir);
        let _ = fs::remove_dir_all(&dst_dir);
    }

    #[test]
    fn move_same_fs_uses_rename() {
        let src_dir = tmp();
        let dst_dir = tmp();
        // Ensure src and dst are on the same FS (both in /tmp).
        fs::write(src_dir.join("a.txt"), b"hi").unwrap();

        let cancel = CancelFlag::new();
        let outcome = run(&[src_dir.join("a.txt")], &dst_dir, Op::Move, &cancel, |_| {});
        assert_eq!(outcome.succeeded, 1);
        assert!(!src_dir.join("a.txt").exists());
        assert!(dst_dir.join("a.txt").exists());

        let _ = fs::remove_dir_all(&src_dir);
        let _ = fs::remove_dir_all(&dst_dir);
    }

    #[test]
    fn conflict_skip_keeps_existing() {
        let src_dir = tmp();
        let dst_dir = tmp();
        fs::write(src_dir.join("a.txt"), b"new").unwrap();
        fs::write(dst_dir.join("a.txt"), b"old").unwrap();

        let cancel = CancelFlag::new();
        let outcome = run_with(
            &[src_dir.join("a.txt")],
            &dst_dir,
            Op::Copy,
            &cancel,
            &PauseFlag::new(),
            |_| Conflict::Skip,
            |_| {},
        );
        assert_eq!(outcome.skipped, 1);
        assert_eq!(
            fs::read(dst_dir.join("a.txt")).unwrap(),
            b"old",
            "existing file untouched"
        );
        let _ = fs::remove_dir_all(&src_dir);
        let _ = fs::remove_dir_all(&dst_dir);
    }

    #[test]
    fn conflict_overwrite_replaces() {
        let src_dir = tmp();
        let dst_dir = tmp();
        fs::write(src_dir.join("a.txt"), b"new").unwrap();
        fs::write(dst_dir.join("a.txt"), b"old").unwrap();

        let cancel = CancelFlag::new();
        run_with(
            &[src_dir.join("a.txt")],
            &dst_dir,
            Op::Copy,
            &cancel,
            &PauseFlag::new(),
            |_| Conflict::Overwrite,
            |_| {},
        );
        assert_eq!(
            fs::read(dst_dir.join("a.txt")).unwrap(),
            b"new",
            "existing file replaced"
        );
        let _ = fs::remove_dir_all(&src_dir);
        let _ = fs::remove_dir_all(&dst_dir);
    }

    #[test]
    fn conflict_keep_both_makes_unique_name() {
        let src_dir = tmp();
        let dst_dir = tmp();
        fs::write(src_dir.join("a.txt"), b"new").unwrap();
        fs::write(dst_dir.join("a.txt"), b"old").unwrap();

        let cancel = CancelFlag::new();
        run_with(
            &[src_dir.join("a.txt")],
            &dst_dir,
            Op::Copy,
            &cancel,
            &PauseFlag::new(),
            |_| Conflict::KeepBoth,
            |_| {},
        );
        assert_eq!(fs::read(dst_dir.join("a.txt")).unwrap(), b"old", "original kept");
        assert!(dst_dir.join("a (1).txt").exists(), "copy placed under a unique name");
        let _ = fs::remove_dir_all(&src_dir);
        let _ = fs::remove_dir_all(&dst_dir);
    }

    #[test]
    fn copy_directory_recursive() {
        let src_dir = tmp();
        let dst_dir = tmp();
        let sub = src_dir.join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("f.txt"), b"x").unwrap();
        fs::write(src_dir.join("g.txt"), b"yy").unwrap();

        let cancel = CancelFlag::new();
        let outcome = run(std::slice::from_ref(&src_dir), &dst_dir, Op::Copy, &cancel, |_| {});
        assert_eq!(outcome.succeeded, 1);
        let dest = dst_dir.join(src_dir.file_name().unwrap());
        assert!(dest.join("sub/f.txt").exists());
        assert!(dest.join("g.txt").exists());

        let _ = fs::remove_dir_all(&src_dir);
        let _ = fs::remove_dir_all(&dst_dir);
    }
}
