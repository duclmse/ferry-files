use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::*;
use crate::store::memory_store::MemoryStore;
use crate::store::sqlite_store::SqliteStore;

const HEADER: &str = "Filename,Size,Date Modified,Date Created,Attributes\r\n";
// 2024-01-01T00:00:00Z as FILETIME.
const JAN_2024: &str = "133485408000000000";

fn stores(data: &Path) -> Vec<FileStore> {
    vec![
        FileStore::Memory(Arc::new(MemoryStore::new(data).unwrap())),
        FileStore::Sqlite(Arc::new(SqliteStore::new(&data.join("index.db")).unwrap())),
    ]
}

fn write_list(path: &Path, rows: &[&str]) {
    let mut text = HEADER.to_owned();
    for r in rows {
        text.push_str(r);
        text.push_str("\r\n");
    }
    std::fs::write(path, text).unwrap();
}

fn has(store: &FileStore, path: &str) -> bool {
    store.get_by_path(path).unwrap().is_some()
}

struct Setup {
    _tmp: tempfile::TempDir,
    data: PathBuf,
    list: PathBuf,
    other: PathBuf,
}

fn setup() -> Setup {
    let tmp = tempfile::TempDir::new().unwrap();
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    Setup {
        list: tmp.path().join("offline.efu"),
        other: tmp.path().join("other.efu"),
        data,
        _tmp: tmp,
    }
}

#[test]
fn a_list_becomes_searchable_entries() {
    for kind in 0..2 {
        let s = setup();
        let store = stores(&s.data).remove(kind);
        write_list(
            &s.list,
            &[
                &format!(r#""D:\Photos\",0,{JAN_2024},{JAN_2024},16"#),
                &format!(r#""D:\Photos\Beach.JPG",2048,{JAN_2024},,34"#),
                r#""D:\Photos\.thumbs",10,,,32"#,
                r#""C:\watched\a.txt",1,,,32"#,
            ],
        );
        let roots = [PathBuf::from(r"C:\Watched")];
        let report = sync(&store, std::slice::from_ref(&s.list), &roots, &s.data).unwrap();
        let loaded = &report.loaded[0];
        assert_eq!(
            (loaded.entries, loaded.under_watch_paths),
            (3, 1),
            "kind {kind}"
        );

        let dir = store.get_by_path(r"D:\Photos").unwrap().unwrap();
        assert!(dir.is_dir);
        assert_eq!(dir.name, "Photos");
        assert_eq!(dir.extension, None);
        assert_eq!(dir.mtime, 1_704_067_200);
        let photo = store.get_by_path(r"D:\Photos\Beach.JPG").unwrap().unwrap();
        assert_eq!(photo.size_bytes, 2048);
        assert_eq!(photo.extension.as_deref(), Some("jpg"));
        assert!(photo.is_hidden, "attribute 2 is hidden");
        assert_eq!(
            photo.attrib_bitmask,
            attrib::ARCHIVE | attrib::HIDDEN,
            "kind {kind}"
        );
        assert_eq!(photo.ctime, 0);
        let dotfile = store.get_by_path(r"D:\Photos\.thumbs").unwrap().unwrap();
        assert!(dotfile.is_hidden);
        assert_eq!(dotfile.extension, None);
        assert!(
            !has(&store, r"C:\watched\a.txt"),
            "the crawl owns watch paths"
        );

        // Unchanged, it is not read again.
        let again = sync(&store, std::slice::from_ref(&s.list), &roots, &s.data).unwrap();
        assert!(again.loaded.is_empty());
    }
}

#[test]
fn a_changed_list_replaces_its_entries() {
    for kind in 0..2 {
        let s = setup();
        let store = stores(&s.data).remove(kind);
        write_list(&s.list, &[r#""/disk/a",1,,,32"#, r#""/disk/b",1,,,32"#]);
        write_list(&s.other, &[r#""/disk/b",1,,,32"#]);
        let lists = [s.list.clone(), s.other.clone()];
        sync(&store, &lists, &[], &s.data).unwrap();

        write_list(&s.list, &[r#""/disk/c",1,,,32"#]);
        let report = sync(&store, &lists, &[], &s.data).unwrap();
        assert_eq!(report.loaded.len(), 1);
        assert_eq!(
            report.loaded[0].removed, 1,
            "a goes; b is the other list's too"
        );
        assert!(!has(&store, "/disk/a"), "kind {kind}");
        assert!(has(&store, "/disk/b"));
        assert!(has(&store, "/disk/c"));
    }
}

#[test]
fn a_list_dropped_from_the_config_takes_its_entries() {
    for kind in 0..2 {
        let s = setup();
        let store = stores(&s.data).remove(kind);
        write_list(&s.list, &[r#""/disk/a",1,,,32"#, r#""/w/real",1,,,32"#]);
        sync(&store, std::slice::from_ref(&s.list), &[], &s.data).unwrap();
        assert!(has(&store, "/w/real"));

        // /w is now a watch path, so /w/real is the crawl's and stays.
        let report = sync(&store, &[], &[PathBuf::from("/w")], &s.data).unwrap();
        assert_eq!(report.dropped, [(s.list.clone(), 1)]);
        assert!(!has(&store, "/disk/a"), "kind {kind}");
        assert!(has(&store, "/w/real"));
        assert_eq!(
            std::fs::read_dir(s.data.join(DIR)).unwrap().count(),
            0,
            "nothing kept for a list that is gone"
        );
    }
}

#[test]
fn a_missing_list_keeps_its_entries() {
    for kind in 0..2 {
        let s = setup();
        let store = stores(&s.data).remove(kind);
        write_list(&s.list, &[r#""/disk/a",1,,,32"#]);
        sync(&store, std::slice::from_ref(&s.list), &[], &s.data).unwrap();
        std::fs::remove_file(&s.list).unwrap();
        let report = sync(&store, std::slice::from_ref(&s.list), &[], &s.data).unwrap();
        assert_eq!(report.unreadable.len(), 1);
        assert!(has(&store, "/disk/a"), "kind {kind}");
    }
}

#[test]
fn memory_store_entries_survive_a_restart() {
    let s = setup();
    {
        let store = FileStore::Memory(Arc::new(MemoryStore::new(&s.data).unwrap()));
        write_list(&s.list, &[r#""/disk/a",1,,,32"#]);
        sync(&store, std::slice::from_ref(&s.list), &[], &s.data).unwrap();
    }
    // No clean shutdown in between: the sync saved the store itself.
    let store = FileStore::Memory(Arc::new(MemoryStore::new(&s.data).unwrap()));
    assert!(has(&store, "/disk/a"));
    let report = sync(&store, std::slice::from_ref(&s.list), &[], &s.data).unwrap();
    assert!(report.loaded.is_empty());
}

#[test]
fn watch_paths_match_by_component_and_ignore_case() {
    let roots = [r"C:\Users".to_owned(), "/".to_owned()];
    assert!(under_any(r"c:\users\x", &roots[..1]));
    assert!(under_any(r"C:\Users", &roots[..1]));
    assert!(!under_any(r"C:\UsersX\y", &roots[..1]));
    assert!(under_any("/anything", &roots[1..]));
    assert_eq!(trim_separators(r"D:\"), "D:");
    assert_eq!(trim_separators("/"), "/");
}
