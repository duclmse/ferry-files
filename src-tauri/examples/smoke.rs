//! Smoke test against a real phone: cargo run --example smoke [-- adb|mtp]
//! Lists the start folder and Download, reads one text file, then writes, re-reads, overwrites
//! (with a version check), checks conflict detection, and deletes a scratch file it creates.
use yomi_files_app_lib::device::{self, join};

fn main() {
    let transport = std::env::args().nth(1).unwrap_or_else(|| "auto".into());
    let t0 = std::time::Instant::now();
    let dev = device::connect(&transport, None).unwrap_or_else(|e| panic!("connect: {e}"));
    println!("connected: {} over {} in {:?}", dev.label(), dev.kind(), t0.elapsed());
    let start = dev.default_dir();
    let root = dev.listdir(&start).expect("list start");
    println!("{start}: {} entries: {:?}", root.len(), root.iter().take(8).map(|e| &e.name).collect::<Vec<_>>());
    let dl = join(&start, "Download");
    let files = dev.listdir(&dl).expect("list Download");
    println!("{dl}: {} entries", files.len());
    if let Some(f) = files.iter().find(|e| !e.dir && e.size < 200_000 && [".txt", ".xml", ".json", ".md"].iter().any(|x| e.name.ends_with(x))) {
        let (data, v) = dev.read(&f.path).expect("read");
        println!("read {} ({} bytes, version {v})", f.name, data.len());
    }

    let scratch = join(&dl, &format!("yomi-files-smoke-{}.txt", std::process::id()));
    let r1 = dev.write(&scratch, b"hello from yomi-files\n", None, true).expect("create");
    println!("created {scratch} version {} backup {:?}", r1.version, r1.backup);
    let (data, v) = dev.read(&scratch).expect("read back");
    assert_eq!(data, b"hello from yomi-files\n");
    assert_eq!(v, r1.version, "version after read should match version after write");
    let r2 = dev.write(&scratch, b"edited\n", Some(&v), false).expect("overwrite");
    println!("overwrote, version {} backup {:?}", r2.version, r2.backup);
    assert!(r2.backup.is_some());
    match dev.write(&scratch, b"stale\n", Some(&v), false) {
        Err(device::DevError::Conflict(m)) => println!("conflict detected as expected: {m}"),
        other => panic!("expected a conflict, got {:?}", other.map(|r| r.version)),
    }
    assert_eq!(dev.read(&scratch).unwrap().0, b"edited\n");
    let b = dev.remove(&scratch).expect("delete");
    println!("deleted scratch file, backup {b:?}");
    assert!(dev.stat(&scratch).unwrap().is_none());
    println!("OK in {:?}", t0.elapsed());
}
