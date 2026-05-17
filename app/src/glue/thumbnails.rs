//! Background thumbnail generation for the current directory listing.
//!
//! Architecture (Cedinia "generation counter" pattern + ordered queue):
//! 1. At install time we spawn N persistent worker threads pulling from a
//!    shared FIFO queue. Workers run for the lifetime of the app.
//! 2. `submit_for` snapshots `state.entries` (already in current sort order),
//!    bumps the generation, **replaces** the queue contents with the new
//!    list, and notifies the workers. Replacing instead of appending discards
//!    any pending work from the previous folder so workers immediately move
//!    onto the new directory.
//! 3. Workers process jobs in queue order. Each result tuple
//!    `(generation, source_path, cache_path)` goes over an mpsc channel.
//! 4. A Slint `Timer` on the UI thread drains the channel in capped batches
//!    (see `MAX_APPLY_PER_TICK`) and patches matching rows in `rows_model`.
//!    Late results from a previous generation are dropped silently.
//!
//! Why a manual queue instead of `rayon::par_iter`: rayon's work-stealing
//! splits input into chunks and picks them in roughly LIFO/striped order, so
//! thumbnails appeared in a scattered pattern. A shared FIFO with N workers
//! preserves both parallelism and visible top-to-bottom fill order.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use mykrut_core::{ThumbSize, generate_or_fail};
use slint::{ComponentHandle, Image, Model, Rgba8Pixel, SharedPixelBuffer, Timer, TimerMode};
use tracing::{debug, warn};

use crate::MainWindow;
use crate::state::AppStateRc;

const APP_NAME: &str = "mykrut";

/// Cap on results we drain per UI-pump tick. Each apply is now O(constant)
/// thanks to off-thread PNG decoding (the worker hands us a ready-to-use
/// pixel buffer), so we can go through many more per tick without stalling.
const MAX_APPLY_PER_TICK: usize = 64;
/// Hard wall-clock budget per tick — defence-in-depth in case some slot
/// goes slow (e.g. very large model). At one frame@60Hz worth of work the
/// UI stays smooth.
const TICK_BUDGET: Duration = Duration::from_millis(12);

/// Extensions we'll attempt audio-art extraction on when MIME-guess doesn't
/// classify the file as `audio/*` (e.g. older systems without a complete
/// mime database). Mirrors AUDIO_EXT in mykrut_core::thumbnails.
const AUDIO_EXT: &[&str] = &[
    "mp3", "flac", "m4a", "m4b", "mp4a", "aac", "ogg", "opus", "wav", "wv", "ape", "aiff", "aif",
];

fn has_thumbnailable_mime(mime: Option<&str>) -> bool {
    matches!(
        mime,
        Some(m) if m.starts_with("image/")
            || m.starts_with("video/")
            || m.starts_with("audio/")
            || m == "application/pdf"
    )
}

fn has_thumbnailable_ext(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| AUDIO_EXT.contains(&e.to_ascii_lowercase().as_str()))
}

pub struct ThumbnailController {
    generation: Arc<AtomicU64>,
    queue: Arc<JobQueue>,
    /// Generated thumbnail resolution as a `ThumbSize` index (0..=3). Read by
    /// every worker per-job so a settings change takes effect immediately.
    thumb_size: Arc<AtomicU8>,
    /// Memory accounting for loaded thumbnails, shared with workers + the UI
    /// pump. `mem_used` is the running total of bytes held by applied
    /// thumbnails for the current generation; `mem_budget` is the cap (0 means
    /// unlimited). Once `mem_used >= mem_budget` workers stop generating and the
    /// pump stops applying, so a folder of thousands of images can't OOM us.
    mem_used: Arc<AtomicU64>,
    mem_budget: Arc<AtomicU64>,
}

/// Bytes a decoded thumbnail occupies in RAM: 4 bytes/pixel (RGBA8).
fn pixels_bytes(buf: &SharedPixelBuffer<Rgba8Pixel>) -> u64 {
    u64::from(buf.width()) * u64::from(buf.height()) * 4
}

/// Convert the configured MiB budget to bytes. 0 (unlimited) maps to `u64::MAX`
/// so the `>=` comparisons never trip.
fn budget_bytes(mb: i32) -> u64 {
    if mb <= 0 { u64::MAX } else { (mb as u64) * 1024 * 1024 }
}

/// Map the persisted 0..=3 index to a `ThumbSize`. Out-of-range falls back to
/// the default X-Large.
fn thumb_size_from_u8(v: u8) -> ThumbSize {
    match v {
        0 => ThumbSize::Normal,
        1 => ThumbSize::Large,
        3 => ThumbSize::XxLarge,
        _ => ThumbSize::XLarge,
    }
}

struct ThumbJob {
    generation: u64,
    src: PathBuf,
}

struct ThumbResult {
    generation: u64,
    source_path: PathBuf,
    /// Pre-decoded RGBA8 pixel buffer. PNG decoding happens on the worker
    /// thread so the UI pump just wraps this in a slint::Image — no I/O,
    /// no zlib, no colour conversion on the UI thread.
    pixels: SharedPixelBuffer<Rgba8Pixel>,
}

/// Shared FIFO between the UI thread (producer) and N worker threads
/// (consumers). Replace-on-submit semantics: each new directory clears any
/// leftover work from the previous one.
struct JobQueue {
    inner: Mutex<VecDeque<ThumbJob>>,
    cv: Condvar,
}

impl JobQueue {
    fn new() -> Self {
        Self {
            inner: Mutex::new(VecDeque::new()),
            cv: Condvar::new(),
        }
    }

    /// Drop everything currently pending and push `jobs` in their given order.
    /// Wakes up all waiting workers.
    fn replace(&self, jobs: Vec<ThumbJob>) {
        let mut q = self.inner.lock().expect("thumb job queue poisoned");
        q.clear();
        q.extend(jobs);
        self.cv.notify_all();
    }

    /// Append jobs to the existing queue without touching what's already
    /// pending. Used by the search pipeline to incrementally feed new hits
    /// without resetting the work that's already in flight.
    fn extend(&self, jobs: Vec<ThumbJob>) {
        if jobs.is_empty() {
            return;
        }
        let mut q = self.inner.lock().expect("thumb job queue poisoned");
        q.extend(jobs);
        self.cv.notify_all();
    }

    /// Block until the next job is available, then return it.
    fn pop(&self) -> ThumbJob {
        let mut q = self.inner.lock().expect("thumb job queue poisoned");
        loop {
            if let Some(job) = q.pop_front() {
                return job;
            }
            q = self.cv.wait(q).expect("thumb job queue poisoned");
        }
    }
}

pub fn install(app: &MainWindow, state: AppStateRc) -> Arc<ThumbnailController> {
    use slint::ComponentHandle;
    let (tx, rx) = channel::<ThumbResult>();
    let generation = Arc::new(AtomicU64::new(0));
    let queue = Arc::new(JobQueue::new());
    let initial_size = app.global::<crate::Settings>().get_thumb_size().clamp(0, 3) as u8;
    let thumb_size = Arc::new(AtomicU8::new(initial_size));
    let mem_used = Arc::new(AtomicU64::new(0));
    let mem_budget = Arc::new(AtomicU64::new(budget_bytes(
        app.global::<crate::Settings>().get_thumb_mem_budget_mb(),
    )));

    spawn_workers(&generation, &queue, &thumb_size, &mem_used, &mem_budget, &tx);
    install_pump(
        app,
        state.clone(),
        generation.clone(),
        mem_used.clone(),
        mem_budget.clone(),
        rx,
    );

    let ctrl = Arc::new(ThumbnailController {
        generation,
        queue,
        thumb_size,
        mem_used,
        mem_budget,
    });

    // Settings combobox → update the live size and re-generate the current view.
    {
        let ctrl = ctrl.clone();
        let state = state.clone();
        app.global::<crate::Callabler>().on_thumb_size_changed(move |idx| {
            let v = idx.clamp(0, 3) as u8;
            debug!(thumb_size = v, "thumbnail size changed — re-submitting");
            ctrl.thumb_size.store(v, Ordering::Release);
            submit_for(&state, &ctrl);
        });
    }

    // Settings: thumbnail memory budget changed → update the cap and re-submit
    // so rows that were skipped under a smaller budget can now fill in.
    {
        let ctrl = ctrl.clone();
        let state = state;
        app.global::<crate::Callabler>().on_thumb_mem_budget_changed(move |mb| {
            ctrl.mem_budget.store(budget_bytes(mb), Ordering::Release);
            debug!(budget_mb = mb, "thumbnail memory budget changed — re-submitting");
            submit_for(&state, &ctrl);
        });
    }

    ctrl
}

fn spawn_workers(
    generation: &Arc<AtomicU64>,
    queue: &Arc<JobQueue>,
    thumb_size: &Arc<AtomicU8>,
    mem_used: &Arc<AtomicU64>,
    mem_budget: &Arc<AtomicU64>,
    tx: &Sender<ThumbResult>,
) {
    // One worker per logical core, capped to keep things sensible on big
    // hosts where disk/decode contention isn't worth more parallelism.
    let n = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(2, 8);
    debug!(workers = n, "spawning thumbnail workers");
    for i in 0..n {
        let queue = queue.clone();
        let tx = tx.clone();
        let generation = generation.clone();
        let thumb_size = thumb_size.clone();
        let mem_used = mem_used.clone();
        let mem_budget = mem_budget.clone();
        std::thread::Builder::new()
            .name(format!("fm-thumb-{i}"))
            .spawn(move || worker_loop(&generation, &queue, &thumb_size, &mem_used, &mem_budget, &tx))
            .expect("spawn thumbnail worker");
    }
}

fn worker_loop(
    generation: &Arc<AtomicU64>,
    queue: &Arc<JobQueue>,
    thumb_size: &Arc<AtomicU8>,
    mem_used: &Arc<AtomicU64>,
    mem_budget: &Arc<AtomicU64>,
    tx: &Sender<ThumbResult>,
) {
    loop {
        let job = queue.pop();
        // Skip jobs whose generation has already been superseded. The queue
        // is normally cleared on a new submit, but a job may already have
        // been popped and be in-flight when the generation bumped.
        if job.generation != generation.load(Ordering::Acquire) {
            continue;
        }
        // Memory cap: once loaded thumbnails fill the budget, stop generating
        // (the expensive full-image decode) entirely. Remaining rows keep their
        // generic icon. Checking here also avoids the transient decode spike.
        if mem_used.load(Ordering::Acquire) >= mem_budget.load(Ordering::Acquire) {
            continue;
        }
        if mykrut_core::thumbnails::is_marked_failed(&job.src, APP_NAME) {
            continue;
        }
        // Resolution is user-configurable (Settings → Thumbnail size); default
        // X-Large (512 px) matches what Nemo / modern HiDPI file managers cache.
        let size = thumb_size_from_u8(thumb_size.load(Ordering::Acquire));
        let Some(thumb_path) = generate_or_fail(&job.src, size, APP_NAME) else {
            continue;
        };
        // Decode here so the UI thread just wraps the pixel buffer in a
        // slint::Image (cheap). For a folder of ~hundreds of cached
        // thumbnails this moves the heavy lifting (PNG zlib, colour
        // conversion) onto N workers running in parallel instead of
        // serialising on the UI thread.
        let Some(pixels) = decode_cached_png(&thumb_path) else {
            warn!(path = %thumb_path.display(), "decode cached thumbnail failed");
            continue;
        };
        if tx
            .send(ThumbResult {
                generation: job.generation,
                source_path: job.src,
                pixels,
            })
            .is_err()
        {
            // Receiver gone → app shutting down.
            return;
        }
    }
}

/// Decode a cached thumbnail PNG into a SharedPixelBuffer<Rgba8Pixel>. The
/// pixel layout of `image::RgbaImage` (4 packed u8) matches Slint's
/// `Rgba8Pixel` byte-for-byte, so we copy via `from_byte_slice` rather than
/// touching individual struct fields.
fn decode_cached_png(path: &Path) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let img = image::ImageReader::open(path).ok()?.decode().ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    Some(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(rgba.as_raw(), w, h))
}

fn install_pump(
    app: &MainWindow,
    state: AppStateRc,
    generation: Arc<AtomicU64>,
    mem_used: Arc<AtomicU64>,
    mem_budget: Arc<AtomicU64>,
    rx: Receiver<ThumbResult>,
) {
    let weak = app.as_weak();
    let timer = Timer::default();
    // Faster tick (was 80 ms) so capped per-tick batches still fill a folder
    // quickly while leaving the event loop time to repaint and process input.
    timer.start(TimerMode::Repeated, Duration::from_millis(20), move || {
        let Some(app) = weak.upgrade() else { return };
        let cur_gen = generation.load(Ordering::Acquire);
        let mut applied = 0usize;
        let mut skipped_stale = 0usize;
        let started = Instant::now();
        // path → row-index maps, built at most once per tick (and only when
        // there's a result to apply) so each apply is an O(1) lookup instead
        // of an O(N) linear scan — O(N²) over a large folder otherwise.
        let mut index_maps: Option<(HashMap<PathBuf, usize>, HashMap<PathBuf, usize>)> = None;
        // Bounded drain: stop after MAX_APPLY_PER_TICK successful applies
        // OR when we've burned through TICK_BUDGET. Anything left in the
        // channel waits for the next tick.
        while applied < MAX_APPLY_PER_TICK && started.elapsed() < TICK_BUDGET {
            let Ok(res) = rx.try_recv() else { break };
            if res.generation != cur_gen {
                skipped_stale += 1;
                continue;
            }
            // Honour the memory cap: drop this (already decoded) buffer instead
            // of retaining it in the model once we're at budget.
            if mem_used.load(Ordering::Acquire) >= mem_budget.load(Ordering::Acquire) {
                continue;
            }
            let bytes = pixels_bytes(&res.pixels);
            let (folder_idx, search_idx) = index_maps.get_or_insert_with(|| build_index_maps(&state));
            if apply_one(&app, &state, res, folder_idx, search_idx) {
                mem_used.fetch_add(bytes, Ordering::AcqRel);
            }
            applied += 1;
        }
        if applied > 0 || skipped_stale > 0 {
            debug!(applied, skipped_stale, "thumbnail batch applied");
        }
    });
    Box::leak(Box::new(timer));
}

/// Snapshot path → row-index lookups for the current listing and search hits.
/// Cheap to build once per drain tick; reused for every result that tick.
fn build_index_maps(state: &AppStateRc) -> (HashMap<PathBuf, usize>, HashMap<PathBuf, usize>) {
    let s = state.borrow();
    let folder = s.entries.iter().enumerate().map(|(i, e)| (e.path.clone(), i)).collect();
    let search = s
        .search_hit_paths
        .iter()
        .enumerate()
        .map(|(i, p)| (p.clone(), i))
        .collect();
    (folder, search)
}

/// Apply a decoded thumbnail to the matching row(s). Returns `true` if it was
/// actually stored somewhere (so the caller can charge its bytes against the
/// memory budget); `false` when the row is gone or already had a thumbnail.
fn apply_one(
    app: &MainWindow,
    state: &AppStateRc,
    res: ThumbResult,
    folder_idx_map: &HashMap<PathBuf, usize>,
    search_idx_map: &HashMap<PathBuf, usize>,
) -> bool {
    // The same path can live in both the regular listing AND in the active
    // search-result set — patch both models.
    let folder_idx = folder_idx_map.get(&res.source_path).copied();
    let search_idx = search_idx_map.get(&res.source_path).copied();
    if folder_idx.is_none() && search_idx.is_none() {
        return false;
    }
    // Decoding already happened on the worker; this is just an Arc-clone.
    let img = Image::from_rgba8(res.pixels);
    let mut stored = false;
    if let Some(idx) = folder_idx {
        let model = state.borrow().rows_model.clone();
        if let Some(mut row) = model.row_data(idx)
            && !row.has_thumbnail
        {
            row.has_thumbnail = true;
            row.thumbnail = img.clone();
            model.set_row_data(idx, row);
            stored = true;
        }
    }
    if let Some(idx) = search_idx {
        let model = app.get_search_rows();
        if let Some(mut row) = model.row_data(idx)
            && !row.has_thumbnail
        {
            row.has_thumbnail = true;
            row.thumbnail = img;
            model.set_row_data(idx, row);
            stored = true;
        }
    }
    stored
}

/// Submit thumbnail jobs for every thumbnailable entry in the current
/// listing, **in entries order**. Since `rebuild_rows` keeps `state.entries`
/// aligned with the current sort, this means workers start with whatever is
/// at the top of the user's view and progress downward.
pub fn submit_for(state: &AppStateRc, ctrl: &Arc<ThumbnailController>) {
    let generation = ctrl.generation.fetch_add(1, Ordering::AcqRel) + 1;
    // New listing → the old rows model (and its thumbnails) is being replaced,
    // so reset the memory tally for the fresh generation.
    ctrl.mem_used.store(0, Ordering::Release);

    let jobs: Vec<ThumbJob> = state
        .borrow()
        .entries
        .iter()
        .filter(|e| has_thumbnailable_mime(e.mime.as_deref()) || has_thumbnailable_ext(&e.path))
        .map(|e| ThumbJob {
            generation,
            src: e.path.clone(),
        })
        .collect();

    debug!(count = jobs.len(), generation, "submitting thumbnail batch");
    // Always call replace() — even with no new jobs — so that any leftover
    // work from the previous directory is dropped.
    ctrl.queue.replace(jobs);
}

/// Bump the generation and drop everything currently queued. Called when
/// search becomes active or a new query starts: any pending folder/old-
/// search jobs are now invisible to the user, so we don't want workers
/// chewing through them while fresh search hits are coming in.
pub fn cancel_all(ctrl: &Arc<ThumbnailController>) {
    let generation = ctrl.generation.fetch_add(1, Ordering::AcqRel) + 1;
    ctrl.mem_used.store(0, Ordering::Release);
    ctrl.queue.replace(Vec::new());
    debug!(generation, "thumbnail queue cancelled");
}

/// Append a batch of paths to the queue at the current generation. Used by
/// the search pump as each batch of hits arrives so the gallery fills with
/// thumbnails progressively, in result order.
pub fn enqueue_paths(paths: Vec<PathBuf>, ctrl: &Arc<ThumbnailController>) {
    if paths.is_empty() {
        return;
    }
    let generation = ctrl.generation.load(Ordering::Acquire);
    let jobs: Vec<ThumbJob> = paths.into_iter().map(|src| ThumbJob { generation, src }).collect();
    let count = jobs.len();
    ctrl.queue.extend(jobs);
    debug!(count, generation, "thumbnail batch enqueued (search)");
}

pub fn is_thumbnailable_path(path: &std::path::Path, mime: Option<&str>) -> bool {
    if has_thumbnailable_mime(mime) {
        return true;
    }
    if has_thumbnailable_ext(path) {
        return true;
    }
    // Callers that don't have the MIME handy (e.g. search results — the
    // walker computes it but doesn't propagate it to the thumbnail check)
    // fall back to inferring it from the path. Without this, mp4/jpg/png
    // search hits weren't getting thumbnails because the audio-only
    // extension table couldn't identify them.
    let guessed = mime_guess::from_path(path).first();
    let guessed_str = guessed.as_ref().map(|m| m.essence_str());
    has_thumbnailable_mime(guessed_str)
}
