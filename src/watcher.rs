//! Native macOS FSEvents integration.
//!
//! `notify` is a good cross-platform crate, but its FSEvents backend does not
//! expose `FSEventStreamEventId`, and therefore cannot persist a resumable
//! cursor or distinguish dropped history.  This small wrapper calls the
//! documented CoreServices FSEvents API directly.  It uses one run-loop thread
//! and the `objc2` framework bindings; no private API, XPC call, or Spotlight
//! store is touched.

use anyhow::{Context, Result, bail};
use objc2_core_foundation::{
    CFArray, CFRetained, CFRunLoop, CFString, kCFAllocatorDefault, kCFRunLoopDefaultMode,
};
#[allow(deprecated)]
use objc2_core_services::{
    ConstFSEventStreamRef, FSEventStreamContext, FSEventStreamCreate, FSEventStreamEventFlags,
    FSEventStreamEventId, FSEventStreamInvalidate, FSEventStreamRelease,
    FSEventStreamScheduleWithRunLoop, FSEventStreamStart, FSEventStreamStop,
    FSEventsGetCurrentEventId, kFSEventStreamCreateFlagFileEvents, kFSEventStreamCreateFlagNoDefer,
    kFSEventStreamCreateFlagWatchRoot, kFSEventStreamEventFlagEventIdsWrapped,
    kFSEventStreamEventFlagHistoryDone, kFSEventStreamEventFlagItemCreated,
    kFSEventStreamEventFlagItemIsDir, kFSEventStreamEventFlagItemIsFile,
    kFSEventStreamEventFlagItemIsSymlink, kFSEventStreamEventFlagItemModified,
    kFSEventStreamEventFlagItemRemoved, kFSEventStreamEventFlagItemRenamed,
    kFSEventStreamEventFlagKernelDropped, kFSEventStreamEventFlagMustScanSubDirs,
    kFSEventStreamEventFlagRootChanged, kFSEventStreamEventFlagUnmount,
    kFSEventStreamEventFlagUserDropped,
};
use std::ffi::{CStr, c_char, c_void};
use std::path::PathBuf;
use std::ptr::NonNull;
use std::slice;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

/// Apple's `kFSEventStreamEventIdSinceNow`.
pub const SINCE_NOW: u64 = u64::MAX;

pub const FLAG_MUST_SCAN_SUBDIRS: u32 = kFSEventStreamEventFlagMustScanSubDirs;
pub const FLAG_USER_DROPPED: u32 = kFSEventStreamEventFlagUserDropped;
pub const FLAG_KERNEL_DROPPED: u32 = kFSEventStreamEventFlagKernelDropped;
pub const FLAG_IDS_WRAPPED: u32 = kFSEventStreamEventFlagEventIdsWrapped;
pub const FLAG_HISTORY_DONE: u32 = kFSEventStreamEventFlagHistoryDone;
pub const FLAG_ROOT_CHANGED: u32 = kFSEventStreamEventFlagRootChanged;
pub const FLAG_UNMOUNT: u32 = kFSEventStreamEventFlagUnmount;
pub const FLAG_ITEM_CREATED: u32 = kFSEventStreamEventFlagItemCreated;
pub const FLAG_ITEM_REMOVED: u32 = kFSEventStreamEventFlagItemRemoved;
pub const FLAG_ITEM_RENAMED: u32 = kFSEventStreamEventFlagItemRenamed;
pub const FLAG_ITEM_MODIFIED: u32 = kFSEventStreamEventFlagItemModified;
pub const FLAG_ITEM_IS_DIR: u32 = kFSEventStreamEventFlagItemIsDir;
pub const FLAG_ITEM_IS_FILE: u32 = kFSEventStreamEventFlagItemIsFile;
pub const FLAG_ITEM_IS_SYMLINK: u32 = kFSEventStreamEventFlagItemIsSymlink;

#[derive(Debug, Clone)]
pub struct RawEvent {
    pub path: PathBuf,
    pub event_id: u64,
    pub flags: u32,
}

impl RawEvent {
    pub fn lost_history(&self) -> bool {
        flags_lost(self.flags)
    }

    pub fn is_directory_event(&self) -> bool {
        self.flags & FLAG_ITEM_IS_DIR != 0
    }

    pub fn is_history_done(&self) -> bool {
        self.flags & FLAG_HISTORY_DONE != 0
    }
}

pub fn flags_lost(flags: u32) -> bool {
    flags
        & (FLAG_MUST_SCAN_SUBDIRS
            | FLAG_USER_DROPPED
            | FLAG_KERNEL_DROPPED
            | FLAG_IDS_WRAPPED
            | FLAG_ROOT_CHANGED
            | FLAG_UNMOUNT)
        != 0
}

pub fn flags_interesting(flags: u32) -> bool {
    flags
        & (FLAG_MUST_SCAN_SUBDIRS
            | FLAG_USER_DROPPED
            | FLAG_KERNEL_DROPPED
            | FLAG_IDS_WRAPPED
            | FLAG_HISTORY_DONE
            | FLAG_ROOT_CHANGED
            | FLAG_UNMOUNT
            | FLAG_ITEM_CREATED
            | FLAG_ITEM_REMOVED
            | FLAG_ITEM_RENAMED
            | FLAG_ITEM_MODIFIED)
        != 0
}

pub fn current_event_id() -> u64 {
    // SAFETY: FSEventsGetCurrentEventId has no arguments and no preconditions.
    unsafe { FSEventsGetCurrentEventId() }
}

struct CallbackContext {
    sender: Sender<RawEvent>,
}

struct SendableRunLoop(CFRetained<CFRunLoop>);
// SAFETY: CFRunLoop is documented as thread-safe for stop/wakeup operations and
// the reference is immutable.  This mirrors the approach used by the fsevent
// crate; the run loop itself is only run on its owning thread.
unsafe impl Send for SendableRunLoop {}

pub struct StreamHandle {
    run_loop: Option<SendableRunLoop>,
    join: Option<thread::JoinHandle<()>>,
}

impl StreamHandle {
    pub fn stop(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        if let Some(run_loop) = self.run_loop.take() {
            run_loop.0.stop();
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for StreamHandle {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

/// Start a live FSEvents stream.  `since_when` is a persisted event id or
/// `SINCE_NOW`.  Events are delivered as individual callbacks and should be
/// debounced by the caller.
pub fn start(
    paths: &[PathBuf],
    since_when: u64,
    latency: f64,
    sender: Sender<RawEvent>,
) -> Result<StreamHandle> {
    if paths.is_empty() {
        bail!("cannot start FSEvents without at least one path");
    }
    let path_strings: Vec<String> = paths
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    let context = Box::new(CallbackContext { sender });
    let context_ptr = Box::into_raw(context) as usize;
    let (ready_tx, ready_rx) = mpsc::channel::<std::result::Result<SendableRunLoop, String>>();

    let join = thread::Builder::new()
        .name("fsevents".into())
        .spawn(move || {
            let strings: Vec<CFRetained<CFString>> =
                path_strings.iter().map(|p| CFString::from_str(p)).collect();
            let array = CFArray::from_retained_objects(&strings);

            let mut stream_context = FSEventStreamContext {
                version: 0,
                info: context_ptr as *mut c_void,
                retain: None,
                release: None,
                copyDescription: None,
            };

            let stream = unsafe {
                FSEventStreamCreate(
                    kCFAllocatorDefault,
                    Some(callback),
                    &mut stream_context,
                    array.as_opaque(),
                    since_when,
                    latency,
                    kFSEventStreamCreateFlagFileEvents
                        | kFSEventStreamCreateFlagNoDefer
                        | kFSEventStreamCreateFlagWatchRoot,
                )
            };
            if stream.is_null() {
                let _ = ready_tx.send(Err("FSEventStreamCreate returned null".into()));
                // SAFETY: pointer came from Box::into_raw above and was never shared.
                unsafe { drop(Box::from_raw(context_ptr as *mut CallbackContext)) };
                return;
            }

            let Some(run_loop) = CFRunLoop::current() else {
                let _ = ready_tx.send(Err("CFRunLoop::current() returned None".into()));
                unsafe {
                    FSEventStreamInvalidate(stream);
                    FSEventStreamRelease(stream);
                    drop(Box::from_raw(context_ptr as *mut CallbackContext));
                }
                return;
            };

            #[allow(deprecated)]
            unsafe {
                FSEventStreamScheduleWithRunLoop(stream, &run_loop, kCFRunLoopDefaultMode.unwrap());
            }

            if !unsafe { FSEventStreamStart(stream) } {
                let _ = ready_tx.send(Err("FSEventStreamStart returned false".into()));
                unsafe {
                    FSEventStreamInvalidate(stream);
                    FSEventStreamRelease(stream);
                    drop(Box::from_raw(context_ptr as *mut CallbackContext));
                }
                return;
            }

            let _ = ready_tx.send(Ok(SendableRunLoop(run_loop.clone())));
            // Blocks until StreamHandle::stop() stops this run loop.
            CFRunLoop::run();

            unsafe {
                FSEventStreamStop(stream);
                FSEventStreamInvalidate(stream);
                FSEventStreamRelease(stream);
                drop(Box::from_raw(context_ptr as *mut CallbackContext));
            }
        })
        .context("spawning FSEvents thread")?;

    let run_loop = ready_rx
        .recv()
        .context("FSEvents thread exited before initialising")?
        .map_err(|message| anyhow::anyhow!(message))?;

    Ok(StreamHandle {
        run_loop: Some(run_loop),
        join: Some(join),
    })
}

unsafe extern "C-unwind" fn callback(
    _stream: ConstFSEventStreamRef,
    info: *mut c_void,
    num_events: usize,
    event_paths: NonNull<c_void>,
    event_flags: NonNull<FSEventStreamEventFlags>,
    event_ids: NonNull<FSEventStreamEventId>,
) {
    if info.is_null() || num_events == 0 {
        return;
    }
    // SAFETY: FSEvents guarantees these arrays have num_events elements and
    // the context pointer remains valid until the stream is released.
    let context = unsafe { &*(info as *const CallbackContext) };
    let paths =
        unsafe { slice::from_raw_parts(event_paths.as_ptr() as *const *const c_char, num_events) };
    let flags = unsafe { slice::from_raw_parts(event_flags.as_ptr(), num_events) };
    let ids = unsafe { slice::from_raw_parts(event_ids.as_ptr(), num_events) };

    for index in 0..num_events {
        let path_ptr = paths[index];
        if path_ptr.is_null() {
            continue;
        }
        // FSEvents paths are normally UTF-8.  Non-UTF-8 paths are extremely
        // rare; lossy conversion keeps the daemon alive instead of panicking.
        let path = unsafe { CStr::from_ptr(path_ptr) }
            .to_string_lossy()
            .into_owned();
        let _ = context.sender.send(RawEvent {
            path: PathBuf::from(path),
            event_id: ids[index],
            flags: flags[index],
        });
    }
}

#[derive(Debug, Default)]
pub struct CatchUp {
    pub events: Vec<RawEvent>,
    pub latest_event_id: Option<u64>,
    pub history_done: bool,
    pub lost: bool,
}

/// Replay persisted FSEvents history for a bounded time.  Used by the CLI's
/// incremental scan; the daemon simply keeps a live stream open and processes
/// the same history in its event loop.
pub fn catch_up(
    paths: &[PathBuf],
    since_when: u64,
    budget: Duration,
    latency: f64,
) -> Result<CatchUp> {
    let (sender, receiver): (Sender<RawEvent>, Receiver<RawEvent>) = mpsc::channel();
    let handle = start(paths, since_when, latency, sender)?;
    let started = Instant::now();
    let mut result = CatchUp {
        latest_event_id: Some(since_when),
        ..Default::default()
    };
    let idle_after = Duration::from_millis(700);

    loop {
        let elapsed = started.elapsed();
        if elapsed >= budget || result.history_done {
            break;
        }
        let remaining = budget.saturating_sub(elapsed);
        match receiver.recv_timeout(remaining.min(Duration::from_millis(250))) {
            Ok(event) => {
                if let Some(latest) = result.latest_event_id {
                    result.latest_event_id = Some(latest.max(event.event_id));
                }
                if event.is_history_done() {
                    result.history_done = true;
                }
                if event.lost_history() {
                    result.lost = true;
                }
                result.events.push(event);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if result.history_done
                    || started.elapsed() >= idle_after && result.events.is_empty()
                {
                    // No historical events and no HistoryDone marker within the
                    // idle window.  This is a normal "nothing happened" result.
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    handle.stop();
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loss_flags_are_recognised() {
        assert!(flags_lost(FLAG_MUST_SCAN_SUBDIRS));
        assert!(flags_lost(FLAG_USER_DROPPED));
        assert!(flags_lost(FLAG_KERNEL_DROPPED));
        assert!(flags_lost(FLAG_IDS_WRAPPED));
        assert!(!flags_lost(FLAG_ITEM_CREATED));
    }

    #[test]
    fn fs_events_deliver_created_directories() {
        let temp = tempfile::tempdir().unwrap();
        let (sender, receiver) = mpsc::channel();
        let handle = start(&[temp.path().to_path_buf()], SINCE_NOW, 0.05, sender).unwrap();
        let new_dir = temp.path().join("node_modules");
        std::fs::create_dir(&new_dir).unwrap();

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut saw = false;
        let expected = new_dir.canonicalize().unwrap_or_else(|_| new_dir.clone());
        while Instant::now() < deadline {
            if let Ok(event) = receiver.recv_timeout(Duration::from_millis(200)) {
                let observed = event.path.canonicalize().unwrap_or(event.path.clone());
                if observed == expected {
                    saw = true;
                    break;
                }
            }
        }
        handle.stop();
        assert!(saw, "expected an FSEvents notification for {new_dir:?}");
    }

    #[test]
    fn history_done_is_not_a_loss_flag() {
        assert!(!flags_lost(FLAG_HISTORY_DONE));
        assert!(flags_interesting(FLAG_HISTORY_DONE));
    }
}
