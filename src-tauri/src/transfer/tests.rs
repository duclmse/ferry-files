use crate::device::{Entry, Result, Transport};

/// The computer's files, as if on another device: kind "test", so copies from it go through
/// a temporary file, with the default `read_to` and `replace_from`.
struct Other;

impl Transport for Other {
    fn kind(&self) -> &'static str {
        "test"
    }
    fn label(&self) -> String {
        "test".into()
    }
    fn default_dir(&self) -> String {
        "/".into()
    }
    fn quick_dirs(&self) -> Vec<(String, String)> {
        vec![]
    }
    fn listdir(&self, path: &str) -> Result<Vec<Entry>> {
        crate::local::Local.listdir(path)
    }
    fn stat(&self, path: &str) -> Result<Option<Entry>> {
        crate::local::Local.stat(path)
    }
    fn read(&self, path: &str) -> Result<(Vec<u8>, String)> {
        crate::local::Local.read(path)
    }
    fn read_range(&self, path: &str, start: u64, len: u64) -> Result<Vec<u8>> {
        crate::local::Local.read_range(path, start, len)
    }
    fn version(&self, path: &str) -> Result<Option<String>> {
        crate::local::Local.version(path)
    }
    fn replace(&self, path: &str, data: &[u8], exists: bool) -> Result<String> {
        crate::local::Local.replace(path, data, exists)
    }
    fn delete(&self, path: &str, is_dir: bool) -> Result<()> {
        crate::local::Local.delete(path, is_dir)
    }
    fn make_dir(&self, path: &str) -> Result<()> {
        crate::local::Local.make_dir(path)
    }
    fn rename(&self, path: &str, new_name: &str) -> Result<()> {
        crate::local::Local.rename(path, new_name)
    }
}

#[test]
fn big_files_are_copied_through_a_file() {
    use std::fs;
    let root = std::env::temp_dir().join(format!("ferry-big-copy-test-{}", std::process::id()));
    let (src, dst) = (root.join("src"), root.join("dst"));
    fs::create_dir_all(&src).unwrap();
    fs::create_dir_all(&dst).unwrap();
    let big: Vec<u8> = (0..super::PIECE + 12345).map(|i| (i % 251) as u8).collect();
    fs::write(src.join("big.bin"), &big).unwrap();
    let s = |p: &std::path::Path| crate::platform::app_path(p);
    let stop = std::sync::atomic::AtomicBool::new(false);
    let mac = crate::Device::new(Box::new(crate::local::Local));
    let other = crate::Device::new(Box::new(Other));
    let opts = super::Opts {
        verify: true,
        ..Default::default()
    };
    // From the other device (through a temporary file) to this computer, and back as a move.
    let r = super::run_transfer(
        &other,
        &mac,
        false,
        1,
        &[s(&src.join("big.bin"))],
        &s(&dst),
        opts,
        &stop,
        |_| {},
    )
    .unwrap();
    assert_eq!((r.copied, r.skipped.len()), (1, 0), "{:?}", r.skipped);
    assert!(fs::read(dst.join("big.bin")).unwrap() == big);
    fs::create_dir(root.join("back")).unwrap();
    let r = super::run_transfer(
        &mac,
        &other,
        false,
        1,
        &[s(&dst.join("big.bin"))],
        &s(&root.join("back")),
        super::Opts {
            moving: true,
            ..opts
        },
        &stop,
        |_| {},
    )
    .unwrap();
    assert_eq!(
        (r.copied, r.skipped.len(), r.backups.len()),
        (1, 0, 1),
        "{:?}",
        r.skipped
    );
    assert!(fs::read(root.join("back/big.bin")).unwrap() == big);
    assert!(
        fs::read(&r.backups[0]).unwrap() == big,
        "the moved original was backed up whole"
    );
    assert!(!dst.join("big.bin").exists());
    // Replacing a big file backs the old one up through a file too.
    let r = super::run_transfer(
        &other,
        &mac,
        false,
        1,
        &[s(&src.join("big.bin"))],
        &s(&root.join("back")),
        super::Opts {
            replace: true,
            ..opts
        },
        &stop,
        |_| {},
    )
    .unwrap();
    assert_eq!((r.copied, r.backups.len()), (1, 1));
    assert!(fs::read(&r.backups[0]).unwrap() == big);
    for b in r.backups {
        fs::remove_file(b).unwrap();
    }
    let left: Vec<_> = fs::read_dir(std::env::temp_dir())
        .unwrap()
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(&format!("ferry-files-copy-{}-", std::process::id()))
        })
        .collect();
    assert!(left.is_empty(), "temporary copies are deleted");
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn transfer_copies_and_moves_folders() {
    use std::fs;
    let root = std::env::temp_dir().join(format!("ferry-transfer-test-{}", std::process::id()));
    let (src, dst) = (root.join("src"), root.join("dst"));
    fs::create_dir_all(src.join("book/inner")).unwrap();
    fs::create_dir_all(dst.join("book")).unwrap();
    fs::write(src.join("book/a.txt"), b"a").unwrap();
    fs::write(src.join("book/inner/b.txt"), b"bb").unwrap();
    fs::write(dst.join("book/a.txt"), b"old").unwrap();
    let mac = crate::Device::new(Box::new(crate::local::Local));
    let s = |p: &std::path::Path| crate::platform::app_path(p);
    let stop = std::sync::atomic::AtomicBool::new(false);
    let paths = [s(&src.join("book"))];
    let r = super::run_transfer(
        &mac,
        &mac,
        true,
        1,
        &paths,
        &s(&dst),
        super::Opts {
            moving: false,
            resume: false,
            verify: true,
            replace: false,
        },
        &stop,
        |_| {},
    )
    .unwrap();
    assert_eq!(r.copied, 1);
    assert_eq!(r.skipped.len(), 1, "the existing a.txt is skipped");
    assert_eq!(fs::read(dst.join("book/a.txt")).unwrap(), b"old");
    assert_eq!(fs::read(dst.join("book/inner/b.txt")).unwrap(), b"bb");
    assert!(
        r.made.is_empty(),
        "book was there before: merged into, not made"
    );
    // A copy into a fresh folder makes book there; undoing it removes all of it.
    fs::create_dir(root.join("copied")).unwrap();
    let r = super::run_transfer(
        &mac,
        &mac,
        true,
        1,
        &paths,
        &s(&root.join("copied")),
        super::Opts::default(),
        &stop,
        |_| {},
    )
    .unwrap();
    assert_eq!(r.made, vec![s(&root.join("copied/book"))]);
    assert_eq!(
        crate::files::remove_all(&mac, &r.made[0]).unwrap(),
        2,
        "a.txt and b.txt backed up"
    );
    assert!(!root.join("copied/book").exists());
    // Resuming: b.txt is there with the same size (done); a.txt differs (skipped, kept).
    let r = super::run_transfer(
        &mac,
        &mac,
        true,
        1,
        &paths,
        &s(&dst),
        super::Opts {
            moving: false,
            resume: true,
            verify: true,
            replace: false,
        },
        &stop,
        |_| {},
    )
    .unwrap();
    assert_eq!((r.copied, r.already, r.skipped.len()), (0, 1, 1));
    assert!(r.skipped[0].1.contains("different"));
    assert_eq!(fs::read(dst.join("book/a.txt")).unwrap(), b"old");
    // Into itself is refused.
    let r = super::run_transfer(
        &mac,
        &mac,
        true,
        1,
        &paths,
        &s(&src.join("book/inner")),
        super::Opts {
            moving: false,
            resume: false,
            verify: true,
            replace: false,
        },
        &stop,
        |_| {},
    )
    .unwrap();
    assert_eq!((r.copied, r.skipped.len()), (0, 1));
    // Move to a fresh folder on the same disk: moved whole, nothing copied or backed up.
    fs::create_dir(root.join("moved")).unwrap();
    let r = super::run_transfer(
        &mac,
        &mac,
        true,
        1,
        &paths,
        &s(&root.join("moved")),
        super::Opts {
            moving: true,
            resume: false,
            verify: true,
            replace: false,
        },
        &stop,
        |_| {},
    )
    .unwrap();
    assert_eq!(
        (r.moved, r.copied, r.skipped.len(), r.backups.len()),
        (1, 0, 0, 0)
    );
    assert_eq!(r.made, vec![s(&root.join("moved/book"))]);
    assert!(!src.join("book").exists());
    assert_eq!(
        fs::read(root.join("moved/book/inner/b.txt")).unwrap(),
        b"bb"
    );
    // Into a folder that has one by that name: merged file by file (a.txt is there, skipped).
    fs::create_dir_all(root.join("merge/book")).unwrap();
    fs::write(root.join("merge/book/a.txt"), b"there").unwrap();
    let r = super::run_transfer(
        &mac,
        &mac,
        true,
        1,
        &[s(&root.join("moved/book"))],
        &s(&root.join("merge")),
        super::Opts {
            moving: true,
            ..Default::default()
        },
        &stop,
        |_| {},
    )
    .unwrap();
    assert_eq!((r.moved, r.copied, r.skipped.len()), (0, 1, 1));
    assert_eq!(fs::read(root.join("merge/book/a.txt")).unwrap(), b"there");
    assert_eq!(
        fs::read(root.join("moved/book/a.txt")).unwrap(),
        b"a",
        "the skipped original stays"
    );
    for b in r.backups {
        fs::remove_file(b).unwrap();
    }
    // Cancelled before starting: nothing happens.
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let r = super::run_transfer(
        &mac,
        &mac,
        true,
        1,
        &[s(&dst.join("book"))],
        &s(&root.join("moved")),
        super::Opts {
            moving: true,
            ..Default::default()
        },
        &stop,
        |_| {},
    )
    .unwrap();
    assert!(r.cancelled && r.copied == 0 && dst.join("book/a.txt").exists());
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn sync_copies_only_what_is_missing() {
    use std::fs;
    let root = std::env::temp_dir().join(format!("ferry-sync-test-{}", std::process::id()));
    let (a, b) = (root.join("a"), root.join("b"));
    fs::create_dir_all(a.join("x/y")).unwrap();
    fs::create_dir_all(b.join("x")).unwrap();
    fs::write(a.join("same.txt"), b"1").unwrap();
    fs::write(b.join("same.txt"), b"2").unwrap();
    fs::write(a.join("big.txt"), b"long").unwrap();
    fs::write(b.join("big.txt"), b"s").unwrap();
    fs::write(a.join("x/new.txt"), b"n").unwrap();
    fs::write(a.join("x/y/deep.txt"), b"dd").unwrap();
    fs::write(a.join(".hid"), b"").unwrap();
    fs::write(b.join("x/only-b.txt"), b"").unwrap();
    let mac = crate::Device::new(Box::new(crate::local::Local));
    let s = |p: &std::path::Path| crate::platform::app_path(p);
    let mut c = super::Comparison::default();
    super::compare_dirs(&mac, &s(&a), &mac, &s(&b), "", false, &mut c).unwrap();
    let mut missing: Vec<_> = c
        .missing
        .iter()
        .map(|i| i.to.strip_prefix(&s(&b)).unwrap().to_string())
        .collect();
    missing.sort();
    assert_eq!(missing, ["/x/new.txt", "/x/y", "/x/y/deep.txt"]);
    assert_eq!(
        (c.differ, c.extra, c.same),
        (
            vec!["big.txt".to_string()],
            vec!["x/only-b.txt".to_string()],
            1
        )
    );
    let stop = std::sync::atomic::AtomicBool::new(false);
    let report = super::TransferReport::default();
    let r = super::run_items(
        &mac,
        &mac,
        1,
        &c.missing,
        super::Opts::default(),
        &stop,
        |_| {},
        report,
    )
    .unwrap();
    assert_eq!((r.copied, r.skipped.len()), (2, 0));
    assert_eq!(fs::read(b.join("x/y/deep.txt")).unwrap(), b"dd");
    assert_eq!(
        fs::read(b.join("big.txt")).unwrap(),
        b"s",
        "different files are left alone"
    );
    assert!(!b.join(".hid").exists(), "hidden files only when asked");
    assert!(super::refuse_nested(true, &s(&a), &s(&a.join("x"))).is_err());

    // Update replaces the file that differs; Mirror also removes what was listed.
    fs::write(b.join("x/also-b.txt"), b"").unwrap();
    let r = super::run_sync(
        &mac,
        &mac,
        &s(&a),
        &s(&b),
        false,
        super::SyncMode::Update,
        &[],
        1,
        super::Opts::default(),
        &stop,
        |_| {},
    )
    .unwrap();
    assert_eq!((r.copied, r.backups.len(), r.removed), (1, 1, 0));
    assert_eq!(fs::read(b.join("big.txt")).unwrap(), b"long");
    assert!(b.join("x/only-b.txt").exists());
    let listed = ["x/only-b.txt".to_string()];
    let r = super::run_sync(
        &mac,
        &mac,
        &s(&a),
        &s(&b),
        false,
        super::SyncMode::Mirror,
        &listed,
        1,
        super::Opts::default(),
        &stop,
        |_| {},
    )
    .unwrap();
    assert_eq!((r.copied, r.removed), (0, 1));
    assert!(!b.join("x/only-b.txt").exists());
    assert!(b.join("x/also-b.txt").exists(), "only what was listed goes");
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn marks_and_replace() {
    let root = std::env::temp_dir().join(format!("ff-marks-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (a, b) = (root.join("a"), root.join("b"));
    for d in [&a, &b] {
        std::fs::create_dir_all(d.join("dir")).unwrap();
        std::fs::write(d.join("dir/x.txt"), "x").unwrap();
        std::fs::write(d.join("same.txt"), "same").unwrap();
        std::fs::write(d.join("tie.txt"), if d == &a { "aaaa" } else { "bbbb" }).unwrap();
    }
    std::fs::write(a.join("only.txt"), "only").unwrap();
    std::fs::write(b.join("theirs.txt"), "t").unwrap();
    std::fs::write(b.join("grew.txt"), "old").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(a.join("grew.txt"), "newer").unwrap();
    let s = |p: &std::path::Path| crate::platform::app_path(p);
    let mac = crate::Device::new(Box::new(crate::local::Local));
    let m = super::mark_dirs(&mac, &s(&a), &mac, &s(&b), false, false).unwrap();
    assert_eq!(m.a["only.txt"], "only");
    assert_eq!(m.b["theirs.txt"], "only");
    assert_eq!(
        (m.a["same.txt"], m.a["dir"], m.a["tie.txt"]),
        ("same", "same", "same"),
        "by size"
    );
    assert_eq!((m.a["grew.txt"], m.b["grew.txt"]), ("newer", "older"));
    let m = super::mark_dirs(&mac, &s(&a), &mac, &s(&b), false, true).unwrap();
    assert_ne!(m.a["tie.txt"], "same", "by content");
    assert_eq!(m.a["same.txt"], "same");

    let stop = std::sync::atomic::AtomicBool::new(false);
    let paths = [s(&a.join("grew.txt"))];
    let opts = super::Opts {
        replace: true,
        verify: true,
        ..Default::default()
    };
    let r = super::run_transfer(&mac, &mac, true, 1, &paths, &s(&b), opts, &stop, |_| {}).unwrap();
    assert_eq!((r.copied, r.backups.len(), r.made.len()), (1, 1, 0));
    assert_eq!(
        std::fs::read_to_string(b.join("grew.txt")).unwrap(),
        "newer"
    );
    let _ = std::fs::remove_dir_all(&root);
}
