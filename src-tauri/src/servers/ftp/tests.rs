use super::*;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicUsize};

fn lines(l: &[&str]) -> Vec<String> {
    l.iter().map(|s| s.to_string()).collect()
}

#[test]
fn addresses() {
    assert_eq!(
        split_server("ftp.example.com").unwrap(),
        ("ftp.example.com".into(), 21)
    );
    assert_eq!(split_server(" host:2121 ").unwrap(), ("host".into(), 2121));
    assert_eq!(split_server("ftp://host/").unwrap(), ("host".into(), 21));
    assert_eq!(split_server("[::1]:2121").unwrap(), ("::1".into(), 2121));
    assert_eq!(split_server("::1").unwrap(), ("::1".into(), 21));
    assert!(split_server("").is_err());
    assert!(split_server("host:abc").is_err());
    assert!(split_server("host/dir").is_err());
}

#[test]
fn stamps() {
    assert_eq!(parse_stamp("19700101000000"), Some(0));
    assert_eq!(parse_stamp("20240102030405"), Some(1_704_164_645));
    assert_eq!(parse_stamp("20240102030405.123"), Some(1_704_164_645));
    assert_eq!(parse_stamp("2024"), None);
    assert_eq!(parse_stamp("20241302030405"), None);
}

#[test]
fn unix_lists() {
    let l = lines(&[
        "total 12",
        "drwxr-xr-x   2 ftp ftp  4096 Jan 02  2024 My Folder",
        "-rw-r--r--   1 ftp ftp  1234 Jan 02  2024 my file é.txt",
        "lrwxrwxrwx   1 ftp ftp     5 Jan 02  2024 latest -> a.txt",
        "drwxr-xr-x   2 ftp ftp  4096 Jan 02  2024 .",
        "drwxr-xr-x   2 ftp ftp  4096 Jan 02  2024 ..",
    ]);
    let v = parse_list("/pub", &l);
    assert_eq!(v.len(), 3);
    assert_eq!(
        (
            v[0].entry.name.as_str(),
            v[0].entry.dir,
            v[0].entry.path.as_str()
        ),
        ("My Folder", true, "/pub/My Folder")
    );
    assert_eq!(
        (v[1].entry.name.as_str(), v[1].entry.size, v[1].entry.dir),
        ("my file é.txt", 1234, false)
    );
    assert!(v[1].entry.mtime > 0);
    assert!(v[2].link);
    assert_eq!(v[2].entry.name, "latest");
}

#[test]
fn windows_lists() {
    let l = lines(&[
        "10-19-20  03:19PM       <DIR>          pub",
        "04-08-14  03:09PM                  403 read me.txt",
    ]);
    let v = parse_list("/", &l);
    assert_eq!(v.len(), 2);
    assert!(v[0].entry.dir);
    assert_eq!(
        (v[1].entry.name.as_str(), v[1].entry.size, v[1].entry.dir),
        ("read me.txt", 403, false)
    );
    assert!(v[1].entry.mtime > 0);
}

#[test]
fn mlsd_lists() {
    let l = lines(&[
        "type=cdir;modify=20240102030405; /pub",
        "type=pdir;modify=20240102030405; /",
        "type=dir;modify=20240102030405;UNIX.mode=0755; sub dir",
        "Type=file;Size=12;Modify=20240102030405.500; a; b é.txt",
    ]);
    let v = parse_mlsd("/pub", &l);
    assert_eq!(v.len(), 2);
    assert_eq!(
        (v[0].entry.name.as_str(), v[0].entry.dir, v[0].entry.mtime),
        ("sub dir", true, 1_704_164_645)
    );
    assert_eq!(
        (
            v[1].entry.name.as_str(),
            v[1].entry.size,
            v[1].entry.path.as_str()
        ),
        ("a; b é.txt", 12, "/pub/a; b é.txt")
    );
}

#[test]
fn errors_in_words() {
    let r = |code: u32, text: &str| {
        FtpError::UnexpectedResponse(suppaftp::types::Response::new(
            Status::from(code),
            text.as_bytes().to_vec(),
        ))
    };
    assert_eq!(
        say(&r(530, "Login incorrect"), ""),
        "Wrong user name or password."
    );
    assert!(say(&r(550, "No such file"), "/a").starts_with("/a: not found"));
    assert_eq!(
        say(&FtpError::ConnectionError(std::io::Error::other("x")), ""),
        "Lost the connection to the server."
    );
}

/// A small FTP server on 127.0.0.1 that keeps its files in memory.
struct Fake {
    port: u16,
    files: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    dirs: Arc<Mutex<Vec<String>>>,
    /// The next command is answered by closing the connection.
    drop_next: Arc<AtomicBool>,
    logins: Arc<AtomicUsize>,
    knobs: Arc<Knobs>,
}

/// What the fake does when renaming.
#[derive(Default)]
struct Knobs {
    /// RNTO may replace a file that's there.
    over: AtomicBool,
    /// The RNTO that counts down to zero from this number is refused (0: none).
    fail_rnto: AtomicUsize,
    /// Every RNTO as "from -> to".
    renames: Mutex<Vec<String>>,
}

const STAMP: &str = "20240102030405";

impl Fake {
    fn start(mlsd: bool) -> Fake {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let f = Fake {
            port: l.local_addr().unwrap().port(),
            files: Arc::default(),
            dirs: Arc::new(Mutex::new(vec!["/".into()])),
            drop_next: Arc::default(),
            logins: Arc::default(),
            knobs: Arc::default(),
        };
        let (files, dirs, drop_next, logins, knobs) = (
            f.files.clone(),
            f.dirs.clone(),
            f.drop_next.clone(),
            f.logins.clone(),
            f.knobs.clone(),
        );
        std::thread::spawn(move || {
            for c in l.incoming().flatten() {
                let (files, dirs, drop_next, logins, knobs) = (
                    files.clone(),
                    dirs.clone(),
                    drop_next.clone(),
                    logins.clone(),
                    knobs.clone(),
                );
                std::thread::spawn(move || serve(c, files, dirs, drop_next, logins, knobs, mlsd));
            }
        });
        f
    }

    fn put(&self, path: &str, data: &[u8]) {
        self.files
            .lock()
            .unwrap()
            .insert(path.into(), data.to_vec());
    }
}

fn say_to(w: &Mutex<TcpStream>, text: &str) {
    let _ = w
        .lock()
        .unwrap()
        .write_all(format!("{text}\r\n").as_bytes());
}

fn fact_line(name: &str, dir: bool, size: usize) -> String {
    if dir {
        format!("type=dir;modify={STAMP}; {name}")
    } else {
        format!("type=file;size={size};modify={STAMP}; {name}")
    }
}

/// The fake's LIST line in Unix style.
fn unix_line(name: &str, dir: bool, size: usize) -> String {
    format!(
        "{}rw-r--r-- 1 ftp ftp {size} Jan 02 2024 {name}",
        if dir { "d" } else { "-" }
    )
}

fn children(
    files: &BTreeMap<String, Vec<u8>>,
    dirs: &[String],
    dir: &str,
    line: fn(&str, bool, usize) -> String,
) -> Vec<String> {
    let prefix = if dir == "/" {
        "/".to_string()
    } else {
        format!("{dir}/")
    };
    let mut out = vec![];
    for (p, d) in files {
        if let Some(n) = p.strip_prefix(&prefix) {
            if !n.contains('/') {
                out.push(line(n, false, d.len()));
            }
        }
    }
    for p in dirs {
        if let Some(n) = p.strip_prefix(&prefix) {
            if !n.is_empty() && !n.contains('/') {
                out.push(line(n, true, 0));
            }
        }
    }
    out
}

fn serve(
    c: TcpStream,
    files: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    dirs: Arc<Mutex<Vec<String>>>,
    drop_next: Arc<AtomicBool>,
    logins: Arc<AtomicUsize>,
    knobs: Arc<Knobs>,
    mlsd: bool,
) {
    let w = Arc::new(Mutex::new(c.try_clone().unwrap()));
    let mut r = BufReader::new(c);
    let mut data: Option<TcpListener> = None;
    let mut rest = 0usize;
    let mut from = String::new();
    // A RETR still sending. Held while its 226 is said, so an ABOR answers as a real server
    // does: 426 and 226 while it's sending, 226 alone once it's done. (On Windows the whole
    // file can fit in the socket's buffers, and be sent before the client aborts.)
    let sending = Arc::new(Mutex::new(false));
    say_to(&w, "220 fake ready");
    let mut line = String::new();
    loop {
        line.clear();
        if r.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        if drop_next.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let l = line.trim_end();
        let (cmd, arg) = l.split_once(' ').unwrap_or((l, ""));
        match cmd.to_uppercase().as_str() {
            "USER" => say_to(&w, "331 password please"),
            "PASS" => {
                logins.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                say_to(&w, "230 ok")
            }
            "FEAT" => {
                let mut t = "211-Features:\r\n UTF8\r\n".to_string();
                if mlsd {
                    t += " MLST type*;size*;modify*;\r\n";
                }
                t += "211 End";
                say_to(&w, &t)
            }
            "OPTS" | "TYPE" => say_to(&w, "200 ok"),
            "PWD" => say_to(&w, "257 \"/\" is the folder"),
            "PASV" => {
                let l = TcpListener::bind("127.0.0.1:0").unwrap();
                let p = l.local_addr().unwrap().port();
                data = Some(l);
                say_to(
                    &w,
                    &format!(
                        "227 Entering Passive Mode (127,0,0,1,{},{})",
                        p / 256,
                        p % 256
                    ),
                );
            }
            "REST" => {
                rest = arg.parse().unwrap();
                say_to(&w, "350 restarting");
            }
            "MLSD" | "LIST" => {
                let dir = if arg.is_empty() { "/" } else { arg };
                if !dirs.lock().unwrap().iter().any(|d| d == dir) {
                    say_to(&w, "550 no such folder");
                    continue;
                }
                say_to(&w, "150 here it comes");
                let (mut d, _) = data.take().unwrap().accept().unwrap();
                let mut rows = children(
                    &files.lock().unwrap(),
                    &dirs.lock().unwrap(),
                    dir,
                    if cmd == "MLSD" { fact_line } else { unix_line },
                );
                if cmd == "MLSD" {
                    rows.insert(0, format!("type=cdir;modify={STAMP}; {dir}"));
                }
                for row in rows {
                    let _ = d.write_all(format!("{row}\r\n").as_bytes());
                }
                drop(d);
                say_to(&w, "226 done");
            }
            "MLST" => {
                let f = files.lock().unwrap();
                match f.get(arg) {
                    Some(d) => say_to(
                        &w,
                        &format!(
                            "250-Listing\r\n type=file;size={};modify={STAMP}; {arg}\r\n250 End",
                            d.len()
                        ),
                    ),
                    None if dirs.lock().unwrap().iter().any(|d| d == arg) => say_to(
                        &w,
                        &format!("250-Listing\r\n type=dir;modify={STAMP}; {arg}\r\n250 End"),
                    ),
                    None => say_to(&w, "550 not found"),
                }
            }
            "MDTM" => say_to(&w, &format!("213 {STAMP}")),
            "RETR" => {
                let Some(body) = files.lock().unwrap().get(arg).cloned() else {
                    say_to(&w, "550 not found");
                    continue;
                };
                say_to(&w, "150 sending");
                let (mut d, _) = data.take().unwrap().accept().unwrap();
                let from = std::mem::take(&mut rest).min(body.len());
                let (w2, sending2) = (w.clone(), sending.clone());
                *sending.lock().unwrap() = true;
                std::thread::spawn(move || {
                    // Whoever closes the data connection early gets no 226 from here.
                    let sent = d.write_all(&body[from..]).is_ok();
                    drop(d);
                    let mut still = sending2.lock().unwrap();
                    if sent && *still {
                        say_to(&w2, "226 done");
                    }
                    *still = false;
                });
            }
            "ABOR" => {
                let mut still = sending.lock().unwrap();
                say_to(
                    &w,
                    if *still {
                        "426 aborted\r\n226 ok"
                    } else {
                        "226 ok"
                    },
                );
                *still = false;
            }
            "STOR" => {
                say_to(&w, "150 go on");
                let (mut d, _) = data.take().unwrap().accept().unwrap();
                let mut body = vec![];
                d.read_to_end(&mut body).unwrap();
                files.lock().unwrap().insert(arg.into(), body);
                say_to(&w, "226 stored");
            }
            "RNFR" => {
                from = arg.into();
                say_to(&w, "350 ready");
            }
            "RNTO" => {
                let mut f = files.lock().unwrap();
                knobs
                    .renames
                    .lock()
                    .unwrap()
                    .push(format!("{from} -> {arg}"));
                let left = knobs.fail_rnto.load(std::sync::atomic::Ordering::SeqCst);
                if left > 0 {
                    knobs
                        .fail_rnto
                        .store(left - 1, std::sync::atomic::Ordering::SeqCst);
                }
                if left == 1 {
                    say_to(&w, "550 refused");
                } else if f.contains_key(arg)
                    && !knobs.over.load(std::sync::atomic::Ordering::SeqCst)
                {
                    say_to(&w, "550 exists");
                } else if let Some(b) = f.remove(&from) {
                    f.insert(arg.into(), b);
                    say_to(&w, "250 renamed");
                } else {
                    say_to(&w, "550 no such file");
                }
            }
            "DELE" => {
                let gone = files.lock().unwrap().remove(arg).is_some();
                say_to(
                    &w,
                    if gone {
                        "250 deleted"
                    } else {
                        "550 no such file"
                    },
                );
            }
            "MKD" => {
                dirs.lock().unwrap().push(arg.into());
                say_to(&w, &format!("257 \"{arg}\" created"));
            }
            "RMD" => {
                dirs.lock().unwrap().retain(|d| d != arg);
                say_to(&w, "250 removed");
            }
            "CWD" => {
                let ok = dirs.lock().unwrap().iter().any(|d| d == arg);
                say_to(&w, if ok { "250 ok" } else { "550 no" });
            }
            "QUIT" => {
                say_to(&w, "221 bye");
                return;
            }
            _ => say_to(&w, "502 not here"),
        }
    }
}

fn connect(f: &Fake) -> Ftp<NoTlsStream> {
    let port = f.port;
    Ftp::new(
        "127.0.0.1".into(),
        Box::new(move || dial_plain("127.0.0.1", port, "anonymous", "a@")),
    )
    .unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn lists_and_stats() {
    for mlsd in [true, false] {
        let f = Fake::start(mlsd);
        f.dirs.lock().unwrap().push("/My Dir".into());
        f.put("/My Dir/ünï cöde.txt", b"hello");
        f.put("/top.txt", b"abc");
        let t = connect(&f);
        assert_eq!(t.default_dir(), "/");
        let mut root = t.listdir("/").unwrap();
        root.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(
            root.iter()
                .map(|e| (e.name.as_str(), e.dir, e.size))
                .collect::<Vec<_>>(),
            [("My Dir", true, 0), ("top.txt", false, 3)]
        );
        assert!(root[1].mtime > 0);
        let sub = t.listdir("/My Dir").unwrap();
        assert_eq!(
            (sub[0].name.as_str(), sub[0].path.as_str(), sub[0].size),
            ("ünï cöde.txt", "/My Dir/ünï cöde.txt", 5)
        );
        assert!(t.stat("/My Dir/ünï cöde.txt").unwrap().is_some());
        assert!(t.stat("/My Dir").unwrap().unwrap().dir);
        assert!(t.stat("/nope.txt").unwrap().is_none());
        assert!(t.stat("/nodir/x").unwrap().is_none());
        assert!(t.listdir("/nodir").is_err());
        let (data, v) = t.read("/My Dir/ünï cöde.txt").unwrap();
        assert_eq!(data, b"hello");
        assert_eq!(t.version("/My Dir/ünï cöde.txt").unwrap(), Some(v));
    }
}

#[test]
fn reads_ranges() {
    let f = Fake::start(true);
    let big: Vec<u8> = (0..4_000_000u32).map(|i| (i % 251) as u8).collect();
    f.put("/big.bin", &big);
    f.put("/small.bin", b"0123456789");
    let t = connect(&f);
    assert_eq!(
        t.read_range("/big.bin", 1000, 16).unwrap(),
        &big[1000..1016]
    );
    // The connection still works after the abort, and is the same one.
    assert_eq!(t.read_range("/big.bin", 0, 5).unwrap(), &big[..5]);
    assert_eq!(t.read_range("/small.bin", 4, 100).unwrap(), b"456789");
    assert_eq!(t.read_range("/small.bin", 20, 5).unwrap(), b"");
    assert_eq!(t.listdir("/").unwrap().len(), 2);
    assert_eq!(f.logins.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn copies_to_and_from_files_on_this_computer() {
    let f = Fake::start(true);
    let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
    f.put("/big.bin", &big);
    let t = connect(&f);
    let dir = std::env::temp_dir().join(format!("ferry-ftp-files-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (to, back) = (dir.join("got"), dir.join("send"));
    let mut seen = 0u64;
    let v = t
        .read_to(
            "/big.bin",
            &to,
            &mut |n| seen += n,
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(
        (std::fs::read(&to).unwrap(), seen, v),
        (
            big.clone(),
            big.len() as u64,
            t.version("/big.bin").unwrap().unwrap()
        )
    );
    // Cancelled, then the connection still works.
    assert!(t
        .read_to(
            "/big.bin",
            &to,
            &mut |_| {},
            &std::sync::atomic::AtomicBool::new(true)
        )
        .is_err());
    std::fs::write(&back, &big[..1_000_000]).unwrap();
    let v = t.replace_from("/big.bin", &back, true).unwrap();
    assert_eq!(t.read("/big.bin").unwrap(), (big[..1_000_000].to_vec(), v));
    assert!(f
        .files
        .lock()
        .unwrap()
        .keys()
        .all(|k| !k.contains("ferry-")));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn replaces_and_edits() {
    let f = Fake::start(true);
    f.put("/a.txt", b"old");
    let t = connect(&f);
    let v = t.replace("/a.txt", b"new contents", true).unwrap();
    assert_eq!(t.read("/a.txt").unwrap(), (b"new contents".to_vec(), v));
    assert!(f
        .files
        .lock()
        .unwrap()
        .keys()
        .all(|k| !k.contains("ferry-tmp")));
    t.replace("/b b.txt", b"fresh", false).unwrap();
    assert_eq!(f.files.lock().unwrap()["/b b.txt"], b"fresh");
    t.rename("/b b.txt", "c c.txt").unwrap();
    assert!(t.stat("/b b.txt").unwrap().is_none());
    assert!(t.stat("/c c.txt").unwrap().is_some());
    t.make_dir("/new dir").unwrap();
    assert!(t.stat("/new dir").unwrap().unwrap().dir);
    t.delete("/c c.txt", false).unwrap();
    assert!(t.delete("/c c.txt", false).is_err());
    f.put("/new dir/x.txt", b"x");
    t.delete("/new dir", true).unwrap();
    assert!(t.stat("/new dir").unwrap().is_none());
    assert!(!f.files.lock().unwrap().contains_key("/new dir/x.txt"));
}

#[test]
fn replaces_by_renaming_over_when_the_server_allows() {
    let f = Fake::start(true);
    f.knobs
        .over
        .store(true, std::sync::atomic::Ordering::SeqCst);
    f.put("/a.txt", b"old");
    let t = connect(&f);
    let v = t.replace("/a.txt", b"new!", true).unwrap();
    assert_eq!(t.read("/a.txt").unwrap(), (b"new!".to_vec(), v));
    // One rename, and the old file was never taken away first.
    assert_eq!(
        *f.knobs.renames.lock().unwrap(),
        ["/.a.txt.ferry-tmp -> /a.txt"]
    );
    assert_eq!(
        f.files.lock().unwrap().keys().collect::<Vec<_>>(),
        ["/a.txt"]
    );
}

#[test]
fn replaces_through_a_backup_when_rename_over_is_refused() {
    let f = Fake::start(true);
    f.put("/a.txt", b"old");
    let t = connect(&f);
    t.replace("/a.txt", b"new!", true).unwrap();
    assert_eq!(
        *f.knobs.renames.lock().unwrap(),
        [
            "/.a.txt.ferry-tmp -> /a.txt",
            "/a.txt -> /.a.txt.ferry-old",
            "/.a.txt.ferry-tmp -> /a.txt"
        ]
    );
    // The backup and the temporary file are gone.
    assert_eq!(
        f.files
            .lock()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_slice()))
            .collect::<Vec<_>>(),
        [("/a.txt", &b"new!"[..])]
    );
}

#[test]
fn puts_the_old_file_back_when_the_new_one_cant_go_in_place() {
    let f = Fake::start(true);
    f.put("/a.txt", b"old");
    // Renames: 1 refused (exists), 2 old to backup, 3 new into place: refused.
    f.knobs
        .fail_rnto
        .store(3, std::sync::atomic::Ordering::SeqCst);
    let t = connect(&f);
    assert!(t.replace("/a.txt", b"new!", true).is_err());
    assert_eq!(
        f.files
            .lock()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_slice()))
            .collect::<Vec<_>>(),
        [("/a.txt", &b"old"[..])]
    );
    assert_eq!(
        f.knobs.renames.lock().unwrap().last().unwrap(),
        "/.a.txt.ferry-old -> /a.txt"
    );
}

/// What a verifier keeping its fingerprints in `file` says about a certificate (here just
/// some bytes: it never passes the normal check).
fn check(trust: bool, file: &std::path::Path, cert: &[u8]) -> std::result::Result<(), String> {
    let why = Why::default();
    let v = Pinning {
        normal: WebPkiServerVerifier::builder_with_provider(
            Arc::new(RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            }),
            Arc::new(rustls::crypto::ring::default_provider()),
        )
        .build()
        .unwrap(),
        trust,
        key: "ftp.test:990".into(),
        file: Some(file.to_path_buf()),
        why: why.clone(),
    };
    let name = ServerName::try_from("ftp.test").unwrap();
    let r = v.verify_server_cert(
        &CertificateDer::from(cert.to_vec()),
        &[],
        &name,
        &[],
        UnixTime::now(),
    );
    r.map(|_| ())
        .map_err(|_| why.lock().unwrap().clone().unwrap())
}

#[test]
fn trusts_a_certificate_on_first_use_and_then_only_that_one() {
    let dir = std::env::temp_dir().join(format!("ferry-pin-test-{}", std::process::id()));
    let file = dir.join("sub").join("ftp_certs.json");
    let _ = std::fs::remove_dir_all(&dir);
    let (first, second) = (
        b"first certificate".as_slice(),
        b"another certificate".as_slice(),
    );
    // Not asked to trust: refused, and the message says what to tick.
    let e = check(false, &file, first).unwrap_err();
    assert!(
        e.contains("isn't trusted")
            && e.contains("Trust this server's certificate")
            && e.contains(&fingerprint(first)),
        "{e}"
    );
    assert!(!file.exists());
    // First use: accepted and kept.
    check(true, &file, first).unwrap();
    assert_eq!(pin_get(&file, "ftp.test:990"), Some(fingerprint(first)));
    // Again: fine. Another one: refused, with both fingerprints.
    check(true, &file, first).unwrap();
    let e = check(true, &file, second).unwrap_err();
    assert!(
        e.contains(&fingerprint(first))
            && e.contains(&fingerprint(second))
            && e.contains("changed"),
        "{e}"
    );
    // Another port is another server.
    assert_eq!(pin_get(&file, "ftp.test:21"), None);
    // Without the tick a kept one doesn't count.
    assert!(check(false, &file, first).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fingerprints_look_like_a_sha256() {
    // SHA-256 of nothing.
    assert_eq!(fingerprint(b""), "E3:B0:C4:42:98:FC:1C:14:9A:FB:F4:C8:99:6F:B9:24:27:AE:41:E4:64:9B:93:4C:A4:95:99:1B:78:52:B8:55");
}

#[test]
fn reconnects_when_dropped() {
    let f = Fake::start(true);
    f.put("/a.txt", b"x");
    let t = connect(&f);
    assert_eq!(t.listdir("/").unwrap().len(), 1);
    f.drop_next.store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(t.listdir("/").unwrap().len(), 1);
    assert_eq!(f.logins.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[test]
fn says_when_it_cant_connect() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    let e = dial_plain("127.0.0.1", port, "a", "b").err().unwrap();
    assert!(e.starts_with("Couldn't connect to 127.0.0.1"), "{e}");
    // A plain server doesn't offer AUTH TLS.
    let f = Fake::start(true);
    let cfg = tls_config(false, "127.0.0.1", f.port, None).unwrap();
    let e = dial_tls("127.0.0.1", f.port, "a", "b", cfg).err().unwrap();
    assert_eq!(e, "The server doesn't offer FTPS.");
    assert_eq!(f.logins.load(std::sync::atomic::Ordering::SeqCst), 0);
}
