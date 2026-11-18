//! EPUB editor smoke test against a real phone: cargo run --example epub_smoke [-- adb|mtp]
//! Finds an .epub on the phone and copies it to a scratch file in Download. All edits go to the
//! copy, never to the original. Then it edits the info, finds and replaces, round-trips every
//! chapter through the visual view's body, saves with the version check, reads the file back,
//! and deletes the copy and the backups it made.
use yomi_files_app_lib::device::{self, join, Device, Entry};
use yomi_files_app_lib::epub::{Book, Query};

fn find_epub(dev: &Device, dir: &str, depth: u32) -> Option<Entry> {
    let entries = dev.listdir(dir).ok()?;
    if let Some(e) = entries.iter().find(|e| !e.dir && e.name.to_lowercase().ends_with(".epub") && !e.name.starts_with("yomi-epub-smoke")) {
        return Some(e.clone());
    }
    if depth == 0 {
        return None;
    }
    entries.iter().filter(|e| e.dir && !e.name.starts_with('.') && e.name != "Android").find_map(|e| find_epub(dev, &e.path, depth - 1))
}

fn main() {
    let transport = std::env::args().nth(1).unwrap_or_else(|| "auto".into());
    let t0 = std::time::Instant::now();
    let dev = device::connect(&transport, None).unwrap_or_else(|e| panic!("connect: {e}"));
    println!("connected: {} over {}", dev.label(), dev.kind());
    let start = dev.default_dir();
    let src = find_epub(&dev, &start, 2).expect("no .epub found within two folders of the start folder");
    println!("using {} ({} bytes) — only a copy is edited", src.path, src.size);
    let (data, _) = dev.read(&src.path).expect("read original");

    let scratch = join(&join(&start, "Download"), &format!("yomi-epub-smoke-{}.epub", std::process::id()));
    let created = dev.write(&scratch, &data, None, true).expect("create copy");
    let mut backups = vec![];
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let (bytes, version) = dev.read(&scratch).expect("read copy");
        assert_eq!(version, created.version);
        let mut book = Book::open(&bytes, scratch.clone(), version.clone()).expect("open");
        let chapters = book.chapters().expect("chapters");
        let stats = book.stats().expect("stats");
        println!("EPUB {}: {} chapters, {} files, cover {:?}", stats.version, chapters.len(), stats.files, book.cover_path());
        for c in chapters.iter().take(5) {
            println!("  {} — {}", c.path, c.label);
        }

        // Every chapter: build the visual page, then put its body back unchanged.
        let (mut shown, mut broken) = (0, vec![]);
        for c in &chapters {
            match book.visual_html(&c.path, "yomi://localhost/book/1/") {
                Ok(_) => {
                    let before = book.text(&c.path).unwrap();
                    let inner = book.body_inner(&c.path).unwrap();
                    book.set_body(&c.path, &inner).unwrap();
                    assert_eq!(book.text(&c.path).unwrap(), before, "body round trip changed {}", c.path);
                    shown += 1;
                }
                Err(e) => broken.push(format!("{}: {e}", c.path)),
            }
        }
        println!("visual view: {shown} chapters OK, {} not well-formed {:?}", broken.len(), broken);

        let mut meta = book.meta().expect("meta");
        println!("meta: {:?} by {:?}, lang {}", meta.title, meta.creators, meta.language);
        meta.title = format!("{} [yomi smoke]", meta.title);
        meta.subjects.push("yomi-smoke-subject".into());
        book.set_meta(&meta).expect("set meta");

        // Replace a common word with a marker, if the book has one.
        let word = ["the", "và", "の", "a"].into_iter().find(|w| {
            book.find(&Query { find: w.to_string(), replace: String::new(), match_case: false, whole_word: true }).map_or(false, |f| f.total > 0)
        });
        let mut replaced = 0;
        if let Some(w) = word {
            let q = Query { find: w.to_string(), replace: "YOMISMOKE".into(), match_case: false, whole_word: true };
            let f = book.find(&q).unwrap();
            replaced = book.replace_all(&q).expect("replace");
            println!("replaced {replaced} of {} matches of {w:?} in {} chapters", f.total, f.hits.len());
            assert_eq!(replaced, f.total);
            assert_eq!(book.find(&q).unwrap().total, 0);
        }

        book.touch_modified();
        let out = book.to_bytes().expect("zip");
        let saved = dev.write(&scratch, &out, Some(&version), false).expect("save");
        backups.extend(saved.backup.clone());
        println!("saved {} bytes, backup {:?}", out.len(), saved.backup);
        match dev.write(&scratch, &out, Some(&version), false) {
            Err(device::DevError::Conflict(_)) => println!("stale save refused as expected"),
            other => panic!("expected a conflict, got {:?}", other.map(|r| r.version)),
        }

        let (back, _) = dev.read(&scratch).expect("read back");
        let again = Book::open(&back, scratch.clone(), saved.version).expect("reopen");
        let m2 = again.meta().unwrap();
        assert_eq!(m2.title, meta.title);
        assert!(m2.subjects.contains(&"yomi-smoke-subject".to_string()));
        if replaced > 0 {
            let q = Query { find: "YOMISMOKE".into(), replace: String::new(), match_case: true, whole_word: false };
            assert_eq!(again.find(&q).unwrap().total, replaced);
        }
        assert_eq!(again.chapters().unwrap().len(), chapters.len());
        println!("re-read from the phone: title {:?}, edits present", m2.title);
    }));
    let removed = dev.remove(&scratch).expect("delete copy");
    backups.extend(removed);
    for b in &backups {
        let _ = std::fs::remove_file(b);
    }
    assert!(dev.stat(&scratch).unwrap().is_none());
    println!("deleted the copy and {} backup(s) of it", backups.len());
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
    println!("OK in {:?}", t0.elapsed());
}
