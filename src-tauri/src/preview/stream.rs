//! Files copied whole to the computer to be read a part at a time, for devices and servers that
//! can't send part of a file. The copy runs in the background and parts are read from it as soon
//! as they're there, so a video starts playing while it's copied. Up to `KEEP` files at a time
//! (a video, and the thumbnails read beside it). A copy is stopped and deleted when it's let go
//! of: when it hasn't been read for `IDLE`, or when another file needs its place.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a copy may go without growing before reads give up on it.
const STALL: Duration = Duration::from_secs(60);
/// How long a copy is kept unread.
const IDLE: Duration = Duration::from_secs(120);
/// How many copies are kept at once.
const KEEP: usize = 3;

type Done = Arc<Mutex<Option<Result<(), String>>>>;

struct Copy {
    key: String,
    file: PathBuf,
    stop: Arc<AtomicBool>,
    /// Set when the copy ends: Ok, or why it failed.
    done: Done,
}

impl Drop for Copy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = std::fs::remove_file(&self.file);
    }
}

type Copies = Mutex<Vec<(Copy, Instant)>>;

/// The copies, and when each was last read.
static COPIES: Copies = Mutex::new(Vec::new());

/// Lets go of the copies unread for `IDLE`; ends once there are none.
fn watch(copies: &'static Copies) {
    let spawned = std::thread::Builder::new()
        .name("stream-idle".into())
        .spawn(move || loop {
            std::thread::sleep(IDLE / 4);
            let mut all = lock(copies);
            let old: Vec<_> = all
                .extract_if(.., |(_, used)| used.elapsed() >= IDLE)
                .collect();
            let empty = all.is_empty();
            drop(all);
            drop(old);
            if empty {
                return;
            }
        });
    if let Err(e) = spawned {
        eprintln!("Can't start the stream-idle thread: {e}");
    }
}

/// `len` bytes from `start` of the file `key` names (`size` bytes, its name ending in `ext`, such
/// as ".mp4"). The first time, `fill` copies it in the background into the path it's given; it
/// should stop when the flag it's given is set.
pub fn read(
    key: &str,
    ext: &str,
    size: u64,
    start: u64,
    len: u64,
    fill: impl FnOnce(&Path, &AtomicBool) -> Result<(), String> + Send + 'static,
) -> Result<Vec<u8>, String> {
    // Tests stay out of the app's own copies.
    let dir = if cfg!(test) {
        std::env::temp_dir().join(format!("ferry-stream-{}", std::process::id()))
    } else {
        crate::files::opened_dir().join("stream")
    };
    read_in(&COPIES, &dir, key, ext, size, start, len, fill)
}

// The tests keep their own `copies`: other modules' tests read through `read` at the same time.
#[allow(clippy::too_many_arguments)]
fn read_in(
    copies: &'static Copies,
    dir: &Path,
    key: &str,
    ext: &str,
    size: u64,
    start: u64,
    len: u64,
    fill: impl FnOnce(&Path, &AtomicBool) -> Result<(), String> + Send + 'static,
) -> Result<Vec<u8>, String> {
    if start >= size || len == 0 {
        return Ok(vec![]);
    }
    let (file, stop, done) = {
        let mut all = lock(copies);
        // A failed copy is tried again.
        let failed: Vec<_> = all
            .extract_if(.., |(c, _)| {
                c.key == key && matches!(*lock(&c.done), Some(Err(_)))
            })
            .collect();
        let at = match all.iter().position(|(c, _)| c.key == key) {
            Some(at) => at,
            None => {
                let watched = !all.is_empty();
                // Makes room: the copy read longest ago is stopped and deleted.
                while all.len() >= KEEP {
                    let oldest = (0..all.len()).min_by_key(|&i| all[i].1).expect("not empty");
                    all.remove(oldest);
                }
                all.push((start_copy(dir, key, ext, size, fill)?, Instant::now()));
                if !watched {
                    watch(copies);
                }
                all.len() - 1
            }
        };
        drop(failed);
        let (c, used) = &mut all[at];
        *used = Instant::now();
        (c.file.clone(), c.stop.clone(), c.done.clone())
    };
    let want = start.saturating_add(len).min(size);
    let (mut had, mut since) = (0, Instant::now());
    loop {
        let finished = lock(&done).clone();
        if let Some(Err(e)) = finished {
            return Err(e);
        }
        let have = std::fs::metadata(&file).map_or(0, |m| m.len());
        if have >= want || finished.is_some() {
            break;
        }
        if stop.load(Ordering::Relaxed) {
            return Err("Stopped: other files are being read.".into());
        }
        if have > had {
            (had, since) = (have, Instant::now());
        } else if since.elapsed() > STALL {
            return Err("The copy stopped: nothing came for a minute.".into());
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    let io = |e: std::io::Error| format!("Couldn't read the copy: {e}");
    let mut f = std::fs::File::open(&file).map_err(io)?;
    f.seek(SeekFrom::Start(start)).map_err(io)?;
    let mut out = Vec::with_capacity((want - start) as usize);
    f.take(want - start).read_to_end(&mut out).map_err(io)?;
    Ok(out)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Starts a copy of `key`.
fn start_copy(
    dir: &Path,
    key: &str,
    ext: &str,
    size: u64,
    fill: impl FnOnce(&Path, &AtomicBool) -> Result<(), String> + Send + 'static,
) -> Result<Copy, String> {
    // Each copy has its own name: one being stopped may still be writing (or deleting) its file.
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let file = dir.join(format!("{:x}-{n}{ext}", md5::compute(key)));
    let (stop, done): (Arc<AtomicBool>, Done) = Default::default();
    // Copies left behind (by a crash) are deleted, once.
    static SWEPT: std::sync::Once = std::sync::Once::new();
    SWEPT.call_once(|| {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let _ = std::fs::remove_file(e.path());
            }
        }
    });
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("Couldn't make a folder for the copy: {e}"))?;
    let (f, s, d) = (file.clone(), stop.clone(), done.clone());
    std::thread::Builder::new()
        .name("stream-copy".into())
        .spawn(move || {
            let r = fill(&f, &s).and_then(|_| match std::fs::metadata(&f).map_or(0, |m| m.len()) {
                n if n == size => Ok(()),
                n => Err(format!("Copied {n} bytes but the file has {size}.")),
            });
            if r.is_err() || s.load(Ordering::Relaxed) {
                let _ = std::fs::remove_file(&f);
            }
            *lock(&d) = Some(r);
        })
        .map_err(|e| format!("Couldn't start the copy: {e}"))?;
    Ok(Copy {
        key: key.to_string(),
        file,
        stop,
        done,
    })
}

/// Copies `from` into the file at `to`, stopping when `stop` is set.
pub fn copy_until(from: &mut impl Read, to: &Path, stop: &AtomicBool) -> Result<(), String> {
    use std::io::Write;
    let mut f = std::fs::File::create(to).map_err(|e| format!("Couldn't make the copy: {e}"))?;
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        if stop.load(Ordering::Relaxed) {
            return Err("Stopped.".into());
        }
        let n = from
            .read(&mut buf)
            .map_err(|e| format!("Couldn't read the file: {e}"))?;
        if n == 0 {
            return Ok(());
        }
        f.write_all(&buf[..n])
            .map_err(|e| format!("Couldn't write the copy: {e}"))?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static MINE: Copies = Mutex::new(Vec::new());

    /// A reader that hands out `data` slowly, a piece at a time.
    struct Slow(Vec<u8>, usize);
    impl Read for Slow {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            std::thread::sleep(Duration::from_millis(5));
            let n = buf.len().min(1000).min(self.0.len() - self.1);
            buf[..n].copy_from_slice(&self.0[self.1..self.1 + n]);
            self.1 += n;
            Ok(n)
        }
    }

    // One test, as the copy under way is shared.
    #[test]
    fn reads_parts_while_copying() {
        let dir = std::env::temp_dir().join(format!("ferry-stream-test-{}", std::process::id()));
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let size = data.len() as u64;
        let slow = |d: &Vec<u8>| {
            let d = d.clone();
            move |to: &Path, stop: &AtomicBool| copy_until(&mut Slow(d, 0), to, stop)
        };

        // The start is there long before the whole file.
        let t = Instant::now();
        assert_eq!(
            read_in(&MINE, &dir, "a", ".bin", size, 10, 5, slow(&data)).unwrap(),
            &data[10..15]
        );
        assert!(
            t.elapsed() < Duration::from_millis(500),
            "{:?}",
            t.elapsed()
        );
        // Later parts come from the same copy (this fill isn't used), and stop at the end.
        let unused = |_: &Path, _: &AtomicBool| -> Result<(), String> { panic!("copied twice") };
        assert_eq!(
            read_in(&MINE, &dir, "a", ".bin", size, 150_000, 100_000, unused).unwrap(),
            &data[150_000..]
        );
        assert_eq!(
            read_in(&MINE, &dir, "a", ".bin", size, size, 10, unused).unwrap(),
            b""
        );

        // Copies of other files are kept beside it, up to KEEP; then the one read longest ago
        // is stopped and deleted.
        let find = |key: &str| {
            lock(&MINE)
                .iter()
                .find(|(c, _)| c.key == key)
                .map(|(c, _)| (c.stop.clone(), c.file.clone()))
        };
        let (first, first_file) = find("a").unwrap();
        let other: Vec<u8> = vec![9; 3000];
        assert_eq!(
            read_in(&MINE, &dir, "b", ".bin", 3000, 0, 4, slow(&other)).unwrap(),
            [9; 4]
        );
        assert!(!first.load(Ordering::Relaxed));
        assert_eq!(
            read_in(&MINE, &dir, "a", ".bin", size, 0, 2, unused).unwrap(),
            &data[..2]
        );
        assert_eq!(
            read_in(&MINE, &dir, "b2", ".bin", 3000, 0, 4, slow(&other)).unwrap(),
            [9; 4]
        );
        let (second, _) = find("b").unwrap();
        assert_eq!(
            read_in(&MINE, &dir, "b3", ".bin", 3000, 0, 4, slow(&other)).unwrap(),
            [9; 4]
        );
        assert!(second.load(Ordering::Relaxed), "b was read longest ago");
        assert!(!first.load(Ordering::Relaxed));
        assert_eq!(lock(&MINE).len(), KEEP);

        // Letting go of a copy stops and deletes it.
        lock(&MINE).retain(|(c, _)| c.key != "a");
        assert!(first.load(Ordering::Relaxed));
        assert!(!first_file.exists());

        // A failed copy says why, and the next read tries again.
        let fails = |_: &Path, _: &AtomicBool| -> Result<(), String> { Err("unplugged".into()) };
        assert_eq!(
            read_in(&MINE, &dir, "c", ".bin", 10, 0, 4, fails).unwrap_err(),
            "unplugged"
        );
        let ok = |to: &Path, _: &AtomicBool| {
            std::fs::write(to, b"0123456789").map_err(|e| e.to_string())
        };
        assert_eq!(
            read_in(&MINE, &dir, "c", ".bin", 10, 2, 3, ok).unwrap(),
            b"234"
        );

        // A copy of the wrong size fails.
        let short =
            |to: &Path, _: &AtomicBool| std::fs::write(to, b"0123").map_err(|e| e.to_string());
        assert!(read_in(&MINE, &dir, "d", ".bin", 10, 8, 2, short)
            .unwrap_err()
            .contains("4 bytes"));

        lock(&MINE).clear();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
