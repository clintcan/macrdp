//! Lazy Windows→Mac file paste via `NSFilePresenter` / `NSFileCoordinator`.
//! Default path; opt out with `--no-lazy-paste` to fall back to the
//! eager path in `src/file_promise.rs` (downloads every file the moment
//! Windows announces the descriptor, then auto-fires Cmd-V).
//! This module instead:
//!
//! 1. Creates a **pre-sized empty** temp file per remote entry. Pre-sizing
//!    matters: a zero-byte file gets committed too early and breaks paste
//!    to external disks. We use `FileDescriptor.file_size` from the
//!    descriptor; we will fall back to a synchronous `FileContentsRequest
//!    SIZE` only if the descriptor has no size hint (rare).
//! 2. Registers each pre-sized file as an `NSFilePresenter` via
//!    `NSFileCoordinator.addFilePresenter:`.
//! 3. Writes the temp-file `NSURL`s to NSPasteboard so Finder accepts a
//!    normal Cmd-V.
//! 4. On Cmd-V, Finder asks `NSFileCoordinator` for read access; the
//!    coordinator invokes our presenter's
//!    `relinquishPresentedItemToReader:` block. Inside that callback we
//!    **synchronously** drain `FileContentsRequest` chunks into the
//!    pre-sized file, then call `reader(nil)` to hand control back —
//!    macOS shows its native "Preparing to paste" progress dialog during
//!    the wait.
//!
//! Threading:
//! - Public entry (`spawn_lazy_paste`) is called from a tokio context;
//!   it captures the runtime `Handle` so the presenter callback can
//!   `block_on` the async byte fetch.
//! - All `NSFileCoordinator.addFilePresenter:` / `removeFilePresenter:`
//!   calls are dispatched to `runloop_thread` so they land on a thread
//!   with a pumped CFRunLoop (tokio owns the main thread, so the main
//!   runloop isn't pumped).
//! - The presenter callback itself runs on its
//!   `presentedItemOperationQueue` (a dedicated `NSOperationQueue` we
//!   create), NOT on the runloop thread — so blocking there is safe.
//!
//! Scope: single files AND folder trees. One `NSFilePresenter` per leaf
//! file; directories are just `mkdir -p`'d in the temp dir and only
//! their top-level NSURL goes on the pasteboard. Parallelism for each
//! on-paste fetch is capped at `LAZY_PARALLEL_CHUNKS` (vs eager's
//! `EAGER_PARALLEL_CHUNKS`) because the user is actively interacting at
//! paste time and a higher chunk count visibly stutters RDP input.
//!
//! Cleanup paths: `MacCliprdrBackend::drop` (client disconnect) calls
//! [`cleanup_on_disconnect`] to drain presenters, blow away the temp
//! dir, and clear NSPasteboard if our URLs are still on it. The
//! signal-exit handler in `main.rs` calls [`shutdown_cleanup`] which
//! does the same via a process-global handle registered at factory
//! construction (Drop doesn't run on `std::process::exit`).

#![cfg(target_os = "macos")]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use block2::Block;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{declare_class, msg_send_id, mutability, ClassType, DeclaredClass};
use objc2_app_kit::NSPasteboard;
use objc2_foundation::{
    NSArray, NSFileCoordinator, NSFilePresenter, NSOperationQueue, NSString, NSURL,
};
use tokio::runtime::Handle;
use tracing::{debug, info, warn};

use crate::file_promise::{DownloadRouter, EventSender, RemoteEntry, SelfChangeCount};
use crate::runloop_thread;

/// Per-presenter fetch state. The presenter Obj-C subclass can't carry
/// arbitrary Rust closures in ivars cleanly, so each instance holds a
/// numeric id that indexes into this registry.
struct PresenterState {
    url: Retained<NSURL>,
    queue: Retained<NSOperationQueue>,
    dst: PathBuf,
    index: i32,
    size: u64,
    router: DownloadRouter,
    sender: EventSender,
    rt: Handle,
    /// Set to true after the first successful download so a second
    /// coordinated read doesn't refetch.
    downloaded: Mutex<bool>,
}

/// Wrapping the Retained Obj-C objects in Send/Sync newtypes is sound
/// because every method we invoke on NSURL/NSOperationQueue is documented
/// thread-safe (immutable string-like accessors and queue operations).
struct SendRetained<T>(Retained<T>);
unsafe impl<T> Send for SendRetained<T> {}
unsafe impl<T> Sync for SendRetained<T> {}

static REGISTRY: OnceLock<Mutex<HashMap<u64, Arc<PresenterState>>>> = OnceLock::new();
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Holds Retained presenter objects keyed by the same id, so we can
/// `removeFilePresenter:` on cleanup. Separate from REGISTRY because
/// `LazyPresenter` is not Send/Sync (Obj-C objects with mutable interior).
static LIVE_PRESENTERS: OnceLock<Mutex<HashMap<u64, SendRetained<LazyPresenter>>>> =
    OnceLock::new();

fn registry() -> &'static Mutex<HashMap<u64, Arc<PresenterState>>> {
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn live_presenters() -> &'static Mutex<HashMap<u64, SendRetained<LazyPresenter>>> {
    LIVE_PRESENTERS.get_or_init(|| Mutex::new(HashMap::new()))
}

declare_class!(
    /// Obj-C subclass that conforms to `NSFilePresenter`. One instance per
    /// remote file. Holds only a numeric id in its ivars; the real state
    /// lives in `REGISTRY` so we can manipulate it from Rust without
    /// fighting Obj-C ownership.
    struct LazyPresenter;

    unsafe impl ClassType for LazyPresenter {
        type Super = NSObject;
        type Mutability = mutability::InteriorMutable;
        const NAME: &'static str = "MacrdpLazyFilePresenter";
    }

    impl DeclaredClass for LazyPresenter {
        type Ivars = LazyPresenterIvars;
    }

    unsafe impl LazyPresenter {
        #[method_id(init)]
        fn init(this: Allocated<Self>) -> Option<Retained<Self>> {
            let this = this.set_ivars(LazyPresenterIvars { id: 0 });
            unsafe { msg_send_id![super(this), init] }
        }
    }

    unsafe impl NSFilePresenter for LazyPresenter {
        #[method_id(presentedItemURL)]
        #[allow(non_snake_case)]
        fn presentedItemURL(&self) -> Option<Retained<NSURL>> {
            let id = self.ivars().id;
            registry()
                .lock()
                .unwrap()
                .get(&id)
                .map(|s| s.url.clone())
        }

        #[method_id(presentedItemOperationQueue)]
        #[allow(non_snake_case)]
        fn presentedItemOperationQueue(&self) -> Retained<NSOperationQueue> {
            let id = self.ivars().id;
            // If the entry has been removed (paste superseded), fall back
            // to the main queue so the framework doesn't crash; the
            // reader block we get will be invoked with the queue already
            // gone, but returning Some path is correct.
            registry()
                .lock()
                .unwrap()
                .get(&id)
                .map(|s| s.queue.clone())
                .unwrap_or_else(|| unsafe { NSOperationQueue::mainQueue() })
        }

        #[method(relinquishPresentedItemToReader:)]
        unsafe fn relinquish_to_reader(
            &self,
            reader: *mut Block<dyn Fn(*mut Block<dyn Fn()>)>,
        ) {
            let id = self.ivars().id;
            let state = match registry().lock().unwrap().get(&id).cloned() {
                Some(s) => s,
                None => {
                    // Entry gone; still call back so Finder isn't stuck.
                    if let Some(reader) = reader.as_ref() {
                        reader.call((std::ptr::null_mut(),));
                    }
                    return;
                }
            };

            // Synchronously download (block this presenter's operation
            // queue thread — Apple's coordination model expects us to be
            // able to block arbitrarily long here, while showing native
            // "Preparing to paste" progress UI).
            let mut done = state.downloaded.lock().unwrap();
            if !*done {
                debug!(id, path = ?state.dst, "lazy presenter: downloading on read");
                let res = state.rt.block_on(crate::file_promise::fetch_one_file(
                    state.index,
                    Some(state.size),
                    &state.dst,
                    &state.router,
                    &state.sender,
                    crate::file_promise::LAZY_PARALLEL_CHUNKS,
                    crate::file_promise::LAZY_CHUNK_SIZE,
                ));
                match res {
                    Ok(()) => {
                        *done = true;
                        info!(id, path = ?state.dst, "lazy paste: bytes written");
                    }
                    Err(e) => {
                        warn!(id, error = %e, "lazy paste: download failed; removing temp file");
                        // Delete the pre-sized empty file so Finder's
                        // in-progress copy fails loudly (with a "file
                        // doesn't exist" error) rather than silently
                        // producing a zero-padded ghost with the right
                        // name and size. The reader callback will still
                        // fire below so Finder can finish its
                        // coordination cycle.
                        if let Err(rm_err) = std::fs::remove_file(&state.dst) {
                            debug!(
                                id, path = ?state.dst, error = %rm_err,
                                "lazy paste: cleanup remove_file failed (already gone?)"
                            );
                        }
                        // Drop from registry so a re-coordinated read
                        // doesn't try to re-fetch into the now-missing
                        // file (it'd hit the "Entry gone" path and
                        // immediately return reader(nil)).
                        registry().lock().unwrap().remove(&id);
                    }
                }
            }

            if let Some(reader) = reader.as_ref() {
                reader.call((std::ptr::null_mut(),));
            }
        }
    }
);

unsafe impl NSObjectProtocol for LazyPresenter {}

struct LazyPresenterIvars {
    id: u64,
}

/// Entry point analogous to `crate::file_promise::spawn_remote_paste`.
/// Handles both single-file and folder-tree copies. Falls back to the
/// eager path (by returning `false`) only when a file entry has no size
/// hint — pre-allocating requires a known size, and that case is rare
/// enough not to warrant a synchronous SIZE roundtrip up front.
///
/// Folder strategy: one `NSFilePresenter` per LEAF FILE (not per
/// folder). A folder-level presenter doesn't tell us which inner file is
/// being read, so we'd be forced to eagerly download everything on the
/// first coordinated access — defeating the point. Per-leaf presenters
/// give true lazy semantics: Finder reads each file as it copies it out
/// of the temp dir, and each read triggers a single-file fetch.
pub fn spawn_lazy_paste(
    entries: Vec<RemoteEntry>,
    router: DownloadRouter,
    sender: EventSender,
    current_temp_dir: Arc<Mutex<Option<PathBuf>>>,
    self_change_count: SelfChangeCount,
    rt_handle: Handle,
) -> bool {
    if entries.is_empty() {
        return true;
    }
    // The one remaining fall-through: a file (not dir) without a declared
    // size. Pre-allocating an unknown-size file would require an extra
    // SIZE round-trip per entry; rare enough that letting eager handle
    // it is cleaner than pessimizing the common path.
    for e in &entries {
        if !e.is_dir && e.size.is_none() {
            debug!(
                name = %e.name,
                "lazy paste: entry missing size hint — falling back to eager"
            );
            return false;
        }
    }

    // Clean up any prior paste's temp dir and presenters before starting.
    cleanup_previous(&current_temp_dir);

    let dir = match make_temp_dir() {
        Ok(d) => d,
        Err(e) => {
            warn!("lazy paste: failed to create temp dir: {e}");
            return false;
        }
    };
    *current_temp_dir.lock().unwrap() = Some(dir.clone());

    // Plan every entry's on-disk destination up front, applying the same
    // path-traversal rejection rules as the eager path. If any entry's
    // relative_path is unsafe (`..`, embedded `/`), abort the whole
    // paste rather than try to recover.
    let mut planned: Vec<(RemoteEntry, PathBuf)> = Vec::with_capacity(entries.len());
    for e in entries {
        let dst = match crate::file_promise::resolve_dest(&dir, &e) {
            Some(p) => p,
            None => {
                warn!(name = %e.name, "lazy paste: rejecting entry with unsafe relative_path");
                return false;
            }
        };
        planned.push((e, dst));
    }

    // Create directories first, parent-before-child, so subsequent file
    // pre-allocations don't race against mkdir.
    {
        let mut dirs: Vec<&PathBuf> = planned
            .iter()
            .filter(|(e, _)| e.is_dir)
            .map(|(_, p)| p)
            .collect();
        dirs.sort_by_key(|p| p.components().count());
        for d in dirs {
            if let Err(err) = std::fs::create_dir_all(d) {
                warn!(?d, "lazy paste: create_dir_all failed: {err}");
                return false;
            }
        }
    }

    let mut top_level_urls: Vec<SendRetained<NSURL>> = Vec::new();
    let mut to_register: Vec<(u64, SendRetained<LazyPresenter>)> = Vec::new();

    for (e, dst) in planned {
        // Top-level entries (no relative_path) are what Finder pastes —
        // we publish their NSURL to the pasteboard whether they're a
        // file or a directory. Subdirectory/file entries don't go on
        // the pasteboard; Finder discovers them via the parent dir.
        let is_top_level = e.relative_path.is_none();

        if e.is_dir {
            // Directories don't need a presenter (no bytes to fetch),
            // but if they're top-level we still need their URL on the
            // pasteboard so Finder gets the whole tree.
            if is_top_level {
                let path_ns = NSString::from_str(dst.to_str().unwrap());
                let url = unsafe { NSURL::fileURLWithPath(&path_ns) };
                top_level_urls.push(SendRetained(url));
            }
            continue;
        }

        // Leaf file: pre-allocate, build URL, register presenter.
        let size = e.size.expect("checked above");
        // Some clients (and our own folder-with-only-leaves edge case)
        // may omit intermediate directory entries. Belt-and-braces:
        // ensure the parent dir exists before File::create.
        if let Some(parent) = dst.parent() {
            if let Err(err) = std::fs::create_dir_all(parent) {
                warn!(
                    ?parent,
                    "lazy paste: create_dir_all for file parent failed: {err}"
                );
                return false;
            }
        }
        match std::fs::File::create(&dst).and_then(|f| f.set_len(size)) {
            Ok(()) => {}
            Err(err) => {
                warn!(?dst, "lazy paste: pre-allocate failed: {err}");
                return false;
            }
        }

        let path_ns = NSString::from_str(dst.to_str().unwrap());
        let url = unsafe { NSURL::fileURLWithPath(&path_ns) };
        let queue = unsafe { NSOperationQueue::new() };

        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let state = PresenterState {
            url: url.clone(),
            queue: queue.clone(),
            dst: dst.clone(),
            index: e.index,
            size,
            router: router.clone(),
            sender: sender.clone(),
            rt: rt_handle.clone(),
            downloaded: Mutex::new(false),
        };
        registry().lock().unwrap().insert(id, Arc::new(state));

        let presenter: Retained<LazyPresenter> =
            unsafe { msg_send_id![LazyPresenter::class(), new] };
        unsafe {
            let ivars_ptr =
                (*presenter).ivars() as *const LazyPresenterIvars as *mut LazyPresenterIvars;
            (*ivars_ptr).id = id;
        }

        if is_top_level {
            top_level_urls.push(SendRetained(url));
        }
        to_register.push((id, SendRetained(presenter)));
    }

    let presenter_count = to_register.len();
    let self_cc = self_change_count.clone();
    runloop_thread::submit(move || {
        for (id, presenter) in to_register {
            unsafe {
                let proto: &ProtocolObject<dyn NSFilePresenter> =
                    ProtocolObject::from_ref(&*presenter.0);
                NSFileCoordinator::addFilePresenter(proto);
            }
            live_presenters().lock().unwrap().insert(id, presenter);
        }

        publish_to_pasteboard(&top_level_urls, &self_cc);
        info!(
            roots = top_level_urls.len(),
            presenters = presenter_count,
            "lazy paste: presenters registered and pasteboard published"
        );
    });

    true
}

fn cleanup_previous(current_temp_dir: &Arc<Mutex<Option<PathBuf>>>) {
    if let Some(old) = current_temp_dir.lock().unwrap().take() {
        let _ = std::fs::remove_dir_all(&old);
    }
    // Drain LIVE_PRESENTERS and removeFilePresenter: each on the runloop
    // thread. Drop the REGISTRY entries here so any in-flight read sees
    // "entry gone" and exits cleanly.
    let drained: Vec<(u64, SendRetained<LazyPresenter>)> = {
        let mut g = live_presenters().lock().unwrap();
        g.drain().collect()
    };
    {
        let mut reg = registry().lock().unwrap();
        for (id, _) in &drained {
            reg.remove(id);
        }
    }
    if drained.is_empty() {
        return;
    }
    runloop_thread::submit(move || unsafe {
        for (_, p) in drained {
            let proto: &ProtocolObject<dyn NSFilePresenter> = ProtocolObject::from_ref(&*p.0);
            NSFileCoordinator::removeFilePresenter(proto);
        }
    });
}

/// Process-global handle to the live `MacCliprdr` factory's cleanup
/// state, populated once at startup via [`register_shutdown_cleanup`].
/// The signal-exit handler in `main.rs` reads from this to flush state
/// before `std::process::exit(0)` (which would otherwise skip Drop on
/// the cliprdr backend).
///
/// Single-session assumption: macrdp serves one client at a time and
/// keeps one MacCliprdr factory for the process lifetime, so a single
/// global is sufficient.
static SHUTDOWN_STATE: OnceLock<(Arc<Mutex<Option<PathBuf>>>, SelfChangeCount)> = OnceLock::new();

/// Called once by `MacCliprdr::new` to publish the factory's shared
/// temp-dir + self-change-count tracking into the global so the signal
/// handler can find them. Subsequent calls are no-ops (OnceLock).
pub fn register_shutdown_cleanup(
    paste_temp_dir: Arc<Mutex<Option<PathBuf>>>,
    self_change_count: SelfChangeCount,
) {
    let _ = SHUTDOWN_STATE.set((paste_temp_dir, self_change_count));
}

/// Best-effort cleanup invoked from the SIGINT/SIGTERM watcher in
/// `main.rs` immediately before `std::process::exit(0)`. Equivalent to
/// what `MacCliprdrBackend::drop` would do if Drop ran, but Drop doesn't
/// run on signal exit. Safe to call multiple times.
pub fn shutdown_cleanup() {
    if let Some((temp_dir, self_cc)) = SHUTDOWN_STATE.get() {
        cleanup_on_disconnect(temp_dir, self_cc);
    }
}

/// Clear NSPasteboard *if* its current contents are still the ones we
/// published last (changeCount == self_change_count). Unlike
/// [`cleanup_on_disconnect`], this leaves LIVE_PRESENTERS and the temp
/// dir alone — any in-flight `relinquishPresentedItemToReader:` paste
/// is still allowed to complete, and the presenter objects only become
/// truly unreferenced when superseded by the next remote file list.
///
/// Used by `MacCliprdrBackend::on_outgoing_locks_expired` to handle the
/// case where Windows announces a clipboard change but the new
/// FormatList doesn't include `FileGroupDescriptorW` (e.g. a shell
/// extension for `.zip`/`.gz`/`.7z`/`.rar` swallowed the file
/// representation, so Explorer only published a non-file format).
/// Without this clear, the previous Windows file's URL would stay on the
/// Mac pasteboard and Cmd-V in Finder would silently paste the wrong
/// file; with it, Cmd-V beeps and the user knows the .zip Ctrl-C
/// produced nothing copyable.
///
/// Safe to call repeatedly: the change-count guard ensures we never
/// stomp on something the user (or another app) put on the clipboard
/// between our writes.
pub fn clear_pasteboard_if_stale(self_change_count: &SelfChangeCount) {
    let our_cc = self_change_count.load(Ordering::Relaxed);
    if our_cc < 0 {
        return;
    }
    // Hold the pasteboard guard across the changeCount check AND the clear so
    // it's atomic (no other writer can bump changeCount between them) and race-
    // free against the advertise poller (NSPasteboard is not thread-safe).
    let _pb_guard = crate::clipboard::pasteboard_guard();
    let current_cc = unsafe { NSPasteboard::generalPasteboard().changeCount() } as i64;
    if current_cc != our_cc {
        // Something else owns the clipboard now — leave it alone.
        return;
    }
    unsafe {
        let pb = NSPasteboard::generalPasteboard();
        pb.clearContents();
        let new_cc = pb.changeCount() as i64;
        // Record as a self-write so the change-count poller doesn't see
        // the empty pasteboard and bounce it back to Windows as a fresh
        // Mac-side copy.
        self_change_count.store(new_cc, Ordering::Relaxed);
    }
    info!(
        cleared_cc = our_cc,
        "pasteboard cleared: Windows clipboard transitioned but no FileGroupDescriptorW \
         followed (likely a shell-extension archive type — .zip/.gz/.7z/etc)"
    );
}

/// Called from `MacCliprdrBackend::drop` when an RDP client disconnects.
/// Drains LIVE_PRESENTERS, removes the shared paste temp dir, and clears
/// NSPasteboard *if* its current contents are still the ones we wrote
/// (changeCount == self_change_count) — otherwise the user or another
/// app has put something else on the pasteboard since, and stomping it
/// would be wrong.
///
/// Safe to call even when lazy paste is disabled: the presenter drain is
/// a no-op for the eager path (LIVE_PRESENTERS is only populated by
/// `spawn_lazy_paste`), and the temp dir + pasteboard cleanup applies to
/// both paths equally since they share `paste_temp_dir` and
/// `self_change_count` on the backend.
pub fn cleanup_on_disconnect(
    current_temp_dir: &Arc<Mutex<Option<PathBuf>>>,
    self_change_count: &SelfChangeCount,
) {
    cleanup_previous(current_temp_dir);

    // Only clear the pasteboard if its current changeCount still matches
    // what we set the last time we wrote — otherwise some other app (or
    // the user) has overwritten the clipboard since, and we'd be
    // clobbering their content.
    let our_cc = self_change_count.load(Ordering::Relaxed);
    if our_cc < 0 {
        // We never published; nothing to clear.
        return;
    }
    // Atomic check-then-clear under the shared pasteboard guard (NSPasteboard is
    // not thread-safe; this Drop path races the advertise poller during churn).
    let _pb_guard = crate::clipboard::pasteboard_guard();
    let current_cc = unsafe { NSPasteboard::generalPasteboard().changeCount() } as i64;
    if current_cc == our_cc {
        unsafe {
            // clearContents bumps changeCount; record it as a self-write
            // so the change-count poller doesn't read the now-empty
            // pasteboard and bounce it back to Windows as a fresh copy.
            let pb = NSPasteboard::generalPasteboard();
            pb.clearContents();
            let new_cc = pb.changeCount() as i64;
            self_change_count.store(new_cc, Ordering::Relaxed);
        }
        info!(
            cleared_cc = our_cc,
            "lazy paste: pasteboard cleared on disconnect (our URLs were stale)"
        );
    } else {
        debug!(
            our_cc,
            current_cc,
            "lazy paste: skipping pasteboard clear (something else owns the clipboard now)"
        );
    }
}

fn make_temp_dir() -> std::io::Result<PathBuf> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir =
        std::env::temp_dir().join(format!("macrdp-lazy-paste-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Reap lazy-paste temp dirs (`$TMPDIR/macrdp-lazy-paste-<pid>-<nanos>`) left by
/// a PRIOR macrdp process that died uncleanly (SIGKILL/panic skip the backend's
/// Drop + the signal handler's [`shutdown_cleanup`]). Dead-pid-gated, never our
/// own, so it's safe with another instance live. Best-effort; called at startup.
pub fn reap_stale() {
    crate::reaper::for_each_stale(
        &std::env::temp_dir(),
        "macrdp-lazy-paste-",
        false,
        std::process::id() as i32,
        &crate::reaper::process_is_alive,
        &mut |dir| {
            let _ = std::fs::remove_dir_all(dir);
            debug!(stale_dir = ?dir, "lazy paste: reaped temp dir from a dead prior macrdp process");
        },
    );
}

fn publish_to_pasteboard(urls: &[SendRetained<NSURL>], self_change_count: &SelfChangeCount) {
    if urls.is_empty() {
        return;
    }
    let writers: Vec<Retained<ProtocolObject<dyn objc2_app_kit::NSPasteboardWriting>>> = urls
        .iter()
        .map(|u| ProtocolObject::from_retained(u.0.clone()))
        .collect();
    let array = NSArray::from_vec(writers);
    // Serialize against the advertise poller + other pasteboard writers, and
    // keep the changeCount capture inside the guard so it's our write's count.
    let _pb_guard = crate::clipboard::pasteboard_guard();
    let new_change_count = unsafe {
        let pb = NSPasteboard::generalPasteboard();
        pb.clearContents();
        pb.writeObjects(&array);
        pb.changeCount() as i64
    };
    self_change_count.store(new_change_count, Ordering::Relaxed);
}
