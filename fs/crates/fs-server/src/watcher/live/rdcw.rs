//! `watcher.backend = "native"`: one ReadDirectoryChangesW watch per root.
//!
//! Windows queues changes for a watch in a kernel buffer the size of the
//! one passed to the call, and fills the caller's buffer from it when a read
//! completes. When the kernel buffer fills up between two reads, the read
//! completes with zero bytes (or `ERROR_NOTIFY_ENUM_DIR`): its changes are
//! gone, and only a scan can find them. This reads with a large buffer
//! (`watcher.buffer_kib`), issues the next read before it parses the last,
//! and reports an overflow as lost changes rather than dropping it.

#![cfg_attr(not(windows), allow(dead_code))]

use std::path::{Path, PathBuf};

use super::Batch;
use crate::indexer::crawler::IndexEvent;

const ACTION_ADDED: u32 = 1;
const ACTION_REMOVED: u32 = 2;
const ACTION_MODIFIED: u32 = 3;
const ACTION_RENAMED_OLD_NAME: u32 = 4;
const ACTION_RENAMED_NEW_NAME: u32 = 5;

/// Parses a chain of `FILE_NOTIFY_INFORMATION` records into
/// `(action, name relative to the watched folder)`. `None` when the chain
/// runs past the buffer.
pub(super) fn parse(buf: &[u8], out: &mut Vec<(u32, String)>) -> Option<()> {
    let u32_at = |at: usize| -> Option<u32> {
        Some(u32::from_le_bytes(buf.get(at..at + 4)?.try_into().ok()?))
    };
    let mut at = 0;
    loop {
        let next = u32_at(at)? as usize;
        let action = u32_at(at + 4)?;
        let len = u32_at(at + 8)? as usize;
        let units: Vec<u16> = buf
            .get(at + 12..at + 12 + len)?
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        out.push((action, String::from_utf16_lossy(&units)));
        if next == 0 {
            return Some(());
        }
        at += next;
    }
}

/// Turns notifications into index events, pairing the two halves of a
/// rename. Windows reports them back to back; an old name with no new one
/// (moved out of the root) is a deletion.
#[derive(Default)]
pub(super) struct Pairing {
    old: Option<PathBuf>,
}

impl Pairing {
    pub(super) fn apply(&mut self, root: &Path, action: u32, name: &str, batch: &mut Batch) {
        let path = super::super::long_path(root.join(name));
        if action != ACTION_RENAMED_NEW_NAME {
            self.flush(batch);
        }
        match action {
            ACTION_ADDED => batch.push(IndexEvent::Created(path)),
            ACTION_REMOVED => batch.push(IndexEvent::Deleted(path)),
            ACTION_MODIFIED => batch.push(IndexEvent::Modified(path)),
            ACTION_RENAMED_OLD_NAME => self.old = Some(path),
            ACTION_RENAMED_NEW_NAME => match self.old.take() {
                Some(from) => batch.push(IndexEvent::Renamed { from, to: path }),
                None => batch.push(IndexEvent::Created(path)),
            },
            _ => {}
        }
    }

    /// An unpaired old name, as a deletion.
    pub(super) fn flush(&mut self, batch: &mut Batch) {
        if let Some(old) = self.old.take() {
            batch.push(IndexEvent::Deleted(old));
        }
    }
}

#[cfg(windows)]
pub(super) use imp::start;

#[cfg(windows)]
mod imp {
    use std::os::windows::ffi::OsStrExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::mpsc::UnboundedSender;
    use windows::core::{HRESULT, PCWSTR};
    use windows::Win32::Foundation::{
        CloseHandle, ERROR_INVALID_PARAMETER, ERROR_IO_INCOMPLETE, ERROR_NOTIFY_ENUM_DIR, HANDLE,
        WAIT_TIMEOUT,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, ReadDirectoryChangesW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED,
        FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE, FILE_NOTIFY_CHANGE_ATTRIBUTES,
        FILE_NOTIFY_CHANGE_CREATION, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME,
        FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::Threading::{CreateEventW, ResetEvent};
    use windows::Win32::System::IO::{
        CancelIoEx, GetOverlappedResult, GetOverlappedResultEx, OVERLAPPED,
    };

    use super::super::{pause, unix_now, wait_for, Batch, Msg, RETRY};
    use super::{parse, Pairing};

    /// The most a network share accepts for one read.
    const NETWORK_BUFFER: usize = 64 * 1024;

    const FILTER: FILE_NOTIFY_CHANGE = FILE_NOTIFY_CHANGE(
        FILE_NOTIFY_CHANGE_FILE_NAME.0
            | FILE_NOTIFY_CHANGE_DIR_NAME.0
            | FILE_NOTIFY_CHANGE_ATTRIBUTES.0
            | FILE_NOTIFY_CHANGE_SIZE.0
            | FILE_NOTIFY_CHANGE_LAST_WRITE.0
            | FILE_NOTIFY_CHANGE_CREATION.0,
    );

    struct Owned(HANDLE);

    impl Drop for Owned {
        fn drop(&mut self) {
            // SAFETY: the handle came from a successful open and is closed
            // only here.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    enum Wait {
        Done(usize),
        Timeout,
        Overflow,
    }

    /// One root's open watch with a read in flight.
    struct Watch {
        dir: Owned,
        event: Owned,
        // Boxed: the kernel writes to both while a read is in flight, so
        // neither may move.
        ov: Box<OVERLAPPED>,
        // u64 for the DWORD alignment the records need.
        bufs: [Vec<u64>; 2],
        cur: usize,
        pending: bool,
    }

    impl Watch {
        fn open(root: &std::path::Path, bytes: usize) -> windows::core::Result<Self> {
            let wide: Vec<u16> = root
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            // SAFETY: `wide` is NUL-terminated and outlives the call.
            let dir = Owned(unsafe {
                CreateFileW(
                    PCWSTR(wide.as_ptr()),
                    FILE_LIST_DIRECTORY.0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    None,
                    OPEN_EXISTING,
                    FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                    None,
                )
            }?);
            // SAFETY: plain manual-reset event with no name or attributes.
            let event = Owned(unsafe { CreateEventW(None, true, false, PCWSTR::null()) }?);
            let ov = Box::new(OVERLAPPED {
                hEvent: event.0,
                ..Default::default()
            });
            let words = bytes.div_ceil(8);
            let mut w = Self {
                dir,
                event,
                ov,
                bufs: [vec![0; words], vec![0; words]],
                cur: 0,
                pending: false,
            };
            match w.issue() {
                Err(e)
                    if e.code() == ERROR_INVALID_PARAMETER.to_hresult()
                        && bytes > NETWORK_BUFFER =>
                {
                    w.bufs = [vec![0; NETWORK_BUFFER / 8], vec![0; NETWORK_BUFFER / 8]];
                    w.issue()?;
                }
                other => other?,
            }
            Ok(w)
        }

        fn issue(&mut self) -> windows::core::Result<()> {
            let buf = &mut self.bufs[self.cur];
            // SAFETY: the event is ours; the read writes into `buf` and
            // `ov`, which stay put until it completes or `drop` cancels it.
            unsafe {
                ResetEvent(self.event.0)?;
                ReadDirectoryChangesW(
                    self.dir.0,
                    buf.as_mut_ptr().cast(),
                    (buf.len() * 8) as u32,
                    true,
                    FILTER,
                    None,
                    Some(&raw mut *self.ov),
                    None,
                )?;
            }
            self.pending = true;
            Ok(())
        }

        fn wait(&mut self, timeout: Duration) -> windows::core::Result<Wait> {
            let mut n = 0u32;
            // SAFETY: `ov` belongs to the read in flight on `dir`.
            let res = unsafe {
                GetOverlappedResultEx(
                    self.dir.0,
                    &*self.ov,
                    &mut n,
                    timeout.as_millis() as u32,
                    false,
                )
            };
            match res {
                Ok(()) => {
                    self.pending = false;
                    Ok(if n == 0 {
                        Wait::Overflow
                    } else {
                        Wait::Done(n as usize)
                    })
                }
                Err(e)
                    if e.code() == HRESULT::from_win32(WAIT_TIMEOUT.0)
                        || e.code() == ERROR_IO_INCOMPLETE.to_hresult() =>
                {
                    Ok(Wait::Timeout)
                }
                Err(e) if e.code() == ERROR_NOTIFY_ENUM_DIR.to_hresult() => {
                    self.pending = false;
                    Ok(Wait::Overflow)
                }
                Err(e) => {
                    self.pending = false;
                    Err(e)
                }
            }
        }

        /// The buffer that just completed, as bytes; then the read goes on
        /// into the other one.
        fn swap(&mut self) -> usize {
            let done = self.cur;
            self.cur ^= 1;
            done
        }

        fn bytes(&self, which: usize, n: usize) -> &[u8] {
            let buf = &self.bufs[which];
            // SAFETY: a u64 slice viewed as the bytes it holds; `n` is what
            // the read reported, at most the buffer's size.
            let all =
                unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), buf.len() * 8) };
            &all[..n.min(all.len())]
        }
    }

    impl Drop for Watch {
        fn drop(&mut self) {
            if self.pending {
                let mut n = 0u32;
                // SAFETY: cancels the read in flight and waits for it to
                // finish, so the kernel is done with `ov` and the buffers
                // before they are freed.
                unsafe {
                    let _ = CancelIoEx(self.dir.0, Some(&*self.ov));
                    let _ = GetOverlappedResult(self.dir.0, &*self.ov, &mut n, true);
                }
            }
        }
    }

    pub(in crate::watcher::live) fn start(
        root: PathBuf,
        window: Duration,
        bytes: usize,
        tx: UnboundedSender<Msg>,
        stop: Arc<AtomicBool>,
    ) -> Result<std::thread::JoinHandle<()>, String> {
        std::thread::Builder::new()
            .name("fs-watch".into())
            .spawn(move || run(&root, window, bytes, &tx, &stop))
            .map_err(|e| format!("watcher thread: {e}"))
    }

    fn run(
        root: &std::path::Path,
        window: Duration,
        bytes: usize,
        tx: &UnboundedSender<Msg>,
        stop: &AtomicBool,
    ) {
        // Changes before this are in the index (the crawl or catch-up);
        // a watch that has to be reopened has missed what came between.
        let mut since = unix_now();
        let mut failed = false;
        while !stop.load(Ordering::Relaxed) {
            let mut watch = match Watch::open(root, bytes) {
                Ok(w) => w,
                Err(e) => {
                    if !failed {
                        tracing::warn!("watch {}: {e}; retrying", root.display());
                        failed = true;
                    }
                    if !pause(RETRY, stop) {
                        return;
                    }
                    continue;
                }
            };
            if failed {
                tracing::info!("watching {} again", root.display());
                let _ = tx.send(Msg::Lost {
                    since,
                    reason: format!("watch on {} was reopened", root.display()),
                });
            }
            match pump(&mut watch, root, window, tx, stop, &mut since) {
                Ok(()) => return,
                Err(e) => {
                    tracing::warn!("watch {}: {e}; reopening", root.display());
                    failed = true;
                    if !pause(RETRY, stop) {
                        return;
                    }
                }
            }
        }
    }

    fn pump(
        watch: &mut Watch,
        root: &std::path::Path,
        window: Duration,
        tx: &UnboundedSender<Msg>,
        stop: &AtomicBool,
        since: &mut u64,
    ) -> windows::core::Result<()> {
        let mut batch = Batch::default();
        let mut pairing = Pairing::default();
        let mut records = Vec::new();
        loop {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            match watch.wait(wait_for(&batch, window))? {
                Wait::Timeout => {}
                Wait::Done(n) => {
                    // Everything up to now has been handed over.
                    let now = unix_now();
                    let done = watch.swap();
                    watch.issue()?;
                    *since = now;
                    records.clear();
                    if parse(watch.bytes(done, n), &mut records).is_none() {
                        tracing::warn!("watch {}: malformed change records", root.display());
                    }
                    for (action, name) in records.drain(..) {
                        pairing.apply(root, action, &name, &mut batch);
                    }
                }
                Wait::Overflow => {
                    watch.issue()?;
                    let _ = tx.send(Msg::Lost {
                        since: *since,
                        reason: format!("change buffer of {} overflowed", root.display()),
                    });
                    *since = unix_now();
                }
            }
            if batch
                .deadline(window)
                .is_some_and(|d| d <= std::time::Instant::now())
            {
                pairing.flush(&mut batch);
                if tx.send(Msg::Events(batch.take())).is_err() {
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(next: u32, action: u32, name: &str) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut r = Vec::new();
        r.extend(next.to_le_bytes());
        r.extend(action.to_le_bytes());
        r.extend(((units.len() * 2) as u32).to_le_bytes());
        for u in units {
            r.extend(u.to_le_bytes());
        }
        if next != 0 {
            r.resize(next as usize, 0);
        }
        r
    }

    #[test]
    fn parses_a_record_chain() {
        let mut buf = record(24, ACTION_ADDED, "a.txt");
        buf.extend(record(0, ACTION_REMOVED, r"sub\dïr"));
        let mut out = Vec::new();
        assert_eq!(parse(&buf, &mut out), Some(()));
        assert_eq!(
            out,
            vec![
                (ACTION_ADDED, "a.txt".to_owned()),
                (ACTION_REMOVED, r"sub\dïr".to_owned())
            ]
        );
        assert_eq!(parse(&buf[..30], &mut Vec::new()), None);
    }

    #[test]
    fn pairs_renames_and_treats_a_lone_old_name_as_deleted() {
        let root = Path::new("/w");
        let mut batch = Batch::default();
        let mut pairing = Pairing::default();
        pairing.apply(root, ACTION_RENAMED_OLD_NAME, "a", &mut batch);
        pairing.apply(root, ACTION_RENAMED_NEW_NAME, "b", &mut batch);
        pairing.apply(root, ACTION_RENAMED_OLD_NAME, "gone", &mut batch);
        pairing.apply(root, ACTION_MODIFIED, "c", &mut batch);
        pairing.apply(root, ACTION_RENAMED_NEW_NAME, "moved_in", &mut batch);
        pairing.apply(root, ACTION_RENAMED_OLD_NAME, "last", &mut batch);
        pairing.flush(&mut batch);
        let got: Vec<String> = batch.take().iter().map(|e| format!("{e:?}")).collect();
        let want: Vec<String> = [
            IndexEvent::Renamed {
                from: root.join("a"),
                to: root.join("b"),
            },
            IndexEvent::Deleted(root.join("gone")),
            IndexEvent::Modified(root.join("c")),
            IndexEvent::Created(root.join("moved_in")),
            IndexEvent::Deleted(root.join("last")),
        ]
        .iter()
        .map(|e| format!("{e:?}"))
        .collect();
        assert_eq!(got, want);
    }
}
