use super::*;
use std::io::Write;

const NEXTCLOUD: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:s="http://sabredav.org/ns" xmlns:oc="http://owncloud.org/ns">
 <d:response>
  <d:href>/remote.php/dav/files/me/</d:href>
  <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype><d:getetag>&quot;root1&quot;</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  <d:propstat><d:prop><d:getcontentlength/></d:prop><d:status>HTTP/1.1 404 Not Found</d:status></d:propstat>
 </d:response>
 <d:response>
  <d:href>/remote.php/dav/files/me/My%20Books/</d:href>
  <d:propstat><d:prop><d:getlastmodified>Mon, 10 Oct 2022 08:00:00 GMT</d:getlastmodified><d:resourcetype><d:collection/></d:resourcetype><d:getetag>&quot;f1&quot;</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
 </d:response>
 <d:response>
  <d:href>/remote.php/dav/files/me/caf%C3%A9.epub</d:href>
  <d:propstat><d:prop><d:getlastmodified>Tue, 15 Nov 2022 08:12:31 GMT</d:getlastmodified><d:getcontentlength>1234</d:getcontentlength><d:resourcetype/><d:getetag>&quot;abc123&quot;</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
 </d:response>
</d:multistatus>"#;

const APACHE: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:"><D:response xmlns:lp1="DAV:"><D:href>http://nas:5005/share/a.txt</D:href>
<D:propstat><D:prop><lp1:resourcetype/><lp1:getcontentlength>7</lp1:getcontentlength><lp1:getlastmodified>Tue, 15 Nov 2022 08:12:31 GMT</lp1:getlastmodified></D:prop></D:propstat></D:response></D:multistatus>"#;

/// A server on a free port giving `answers` (status, extra headers, body) in turn: what it
/// was asked (the request with its body), and the address to ask it at.
fn serve(
    answers: Vec<(&'static str, &'static str, &'static str)>,
) -> (std::thread::JoinHandle<Vec<String>>, String) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let at = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let asked = std::thread::spawn(move || {
        let mut lines = vec![];
        for (status, headers, body) in answers {
            let (mut s, _) = listener.accept().unwrap();
            let mut got = vec![];
            let mut buf = [0u8; 4096];
            loop {
                let n = s.read(&mut buf).unwrap();
                got.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&got).to_string();
                if let Some(h) = text.find("\r\n\r\n") {
                    let want = text[..h]
                        .lines()
                        .find_map(|l| {
                            l.to_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if got.len() >= h + 4 + want {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            let reply = format!("HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            s.write_all(reply.as_bytes()).unwrap();
            lines.push(String::from_utf8_lossy(&got).to_string());
        }
        lines
    });
    (asked, at)
}

fn first(req: &str) -> &str {
    req.lines().next().unwrap_or_default()
}

fn has_header(req: &str, header: &str) -> bool {
    req.lines()
        .any(|l| l.to_lowercase() == header.to_lowercase())
}

#[test]
fn addresses() {
    let p = parse_server(" cloud.example.com/remote.php/dav/files/my%20me/ ").unwrap();
    assert_eq!(
        p.base,
        "https://cloud.example.com/remote.php/dav/files/my%20me"
    );
    assert_eq!(p.prefix, "/remote.php/dav/files/my me");
    assert_eq!(p.label, "cloud.example.com/remote.php/dav/files/my me");
    let p = parse_server("http://nas:5005").unwrap();
    assert_eq!(
        (p.base.as_str(), p.prefix.as_str(), p.label.as_str()),
        ("http://nas:5005", "", "nas:5005")
    );
    assert!(parse_server("").is_err());
    assert!(parse_server("ftp://x").is_err());
    assert!(parse_server("https://me@x/dav").is_err());
}

#[test]
fn reads_a_nextcloud_answer() {
    let items = parse(NEXTCLOUD, "/remote.php/dav/files/me").unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!((items[0].path.as_str(), items[0].dir), ("/", true));
    assert_eq!(items[1].path, "/My Books");
    assert!(items[1].dir);
    assert_eq!(items[1].mtime, 1_665_388_800);
    assert_eq!(items[2].path, "/café.epub");
    assert_eq!(
        (items[2].dir, items[2].size, items[2].mtime),
        (false, 1234, 1_668_499_951)
    );
    assert_eq!(items[2].version(), "\"abc123\"");
    // Outside the folder: left out.
    assert!(parse(NEXTCLOUD, "/other").unwrap().is_empty());
}

#[test]
fn reads_an_answer_with_other_prefixes() {
    let items = parse(APACHE, "/share").unwrap();
    assert_eq!(
        items,
        vec![Item {
            path: "/a.txt".into(),
            dir: false,
            size: 7,
            mtime: 1_668_499_951,
            etag: None
        }]
    );
    assert_eq!(items[0].version(), "7:1668499951");
    assert!(parse("<html>nope</html>", "").is_err());
}

#[test]
fn lists_and_stats() {
    let (asked, at) = serve(vec![
        ("207 Multi-Status", "", NEXTCLOUD),
        ("207 Multi-Status", "", NEXTCLOUD),
        ("404 Not Found", "", ""),
    ]);
    // The first answer is the check at connect.
    let dav = Dav::open(&format!("{at}/remote.php/dav/files/me"), Some(("me", "pw"))).unwrap();
    let names: Vec<String> = dav
        .listdir("/")
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, ["My Books", "café.epub"]);
    assert!(dav.stat("/nope").unwrap().is_none());
    let reqs = asked.join().unwrap();
    assert!(
        first(&reqs[0]).starts_with("PROPFIND /remote.php/dav/files/me/ "),
        "{}",
        reqs[0]
    );
    assert!(has_header(&reqs[0], "Depth: 0"), "{}", reqs[0]);
    assert!(has_header(&reqs[1], "Depth: 1"), "{}", reqs[1]);
    // "me:pw" in base64.
    assert!(
        has_header(&reqs[0], "Authorization: Basic bWU6cHc="),
        "{}",
        reqs[0]
    );
    assert!(reqs[0].contains("getetag"));
    assert!(
        first(&reqs[2]).starts_with("PROPFIND /remote.php/dav/files/me/nope "),
        "{}",
        reqs[2]
    );
}

#[test]
fn asks_again_at_the_address_with_a_slash() {
    let (asked, at) = serve(vec![
        ("207 Multi-Status", "", APACHE),
        ("301 Moved Permanently", "Location: /share/a.txt/\r\n", ""),
        ("207 Multi-Status", "", APACHE),
    ]);
    let dav = Dav::open(&format!("{at}/share"), None).unwrap();
    assert_eq!(dav.stat("/a.txt").unwrap().unwrap().size, 7);
    let reqs = asked.join().unwrap();
    assert!(
        first(&reqs[2]).starts_with("PROPFIND /share/a.txt/ "),
        "{}",
        reqs[2]
    );
}

#[test]
fn reads_ranges() {
    let (asked, at) = serve(vec![
        ("207 Multi-Status", "", NEXTCLOUD),
        (
            "206 Partial Content",
            "Content-Range: bytes 1-3/5\r\n",
            "ell",
        ),
        ("200 OK", "", "hello"),
        ("416 Range Not Satisfiable", "", ""),
    ]);
    let dav = Dav::open(&format!("{at}/remote.php/dav/files/me"), None).unwrap();
    assert_eq!(dav.read_range("/a b.txt", 1, 3).unwrap(), b"ell");
    // The server sent it all: copied to the computer, and read from there.
    assert_eq!(dav.read_range("/c.txt", 1, 3).unwrap(), b"ell");
    assert!(dav.read_range("/c.txt", 99, 3).unwrap().is_empty());
    let reqs = asked.join().unwrap();
    assert!(
        first(&reqs[1]).starts_with("GET /remote.php/dav/files/me/a%20b.txt "),
        "{}",
        reqs[1]
    );
    assert!(has_header(&reqs[1], "Range: bytes=1-3"), "{}", reqs[1]);
}

#[test]
fn reads_a_file_with_its_version() {
    let (asked, at) = serve(vec![
        ("207 Multi-Status", "", NEXTCLOUD),
        ("207 Multi-Status", "", NEXTCLOUD),
        ("200 OK", "", "bytes"),
    ]);
    let dav = Dav::open(&format!("{at}/remote.php/dav/files/me"), None).unwrap();
    assert_eq!(
        dav.read("/café.epub").unwrap(),
        (b"bytes".to_vec(), "\"abc123\"".to_string())
    );
    asked.join().unwrap();
}

#[test]
fn writes_through_a_temporary_file() {
    let (asked, at) = serve(vec![
        ("207 Multi-Status", "", NEXTCLOUD),
        ("201 Created", "", ""),
        ("204 No Content", "", ""),
        ("207 Multi-Status", "", NEXTCLOUD),
    ]);
    let dav = Dav::open(&format!("{at}/remote.php/dav/files/me"), None).unwrap();
    let v = dav.replace("/café.epub", b"new text", true).unwrap();
    assert_eq!(v, "\"abc123\"");
    let reqs = asked.join().unwrap();
    assert!(
        first(&reqs[1]).starts_with("PUT /remote.php/dav/files/me/.caf%C3%A9.epub.ferry-"),
        "{}",
        reqs[1]
    );
    assert!(reqs[1].ends_with("new text"), "{}", reqs[1]);
    assert!(
        first(&reqs[2]).starts_with("MOVE /remote.php/dav/files/me/.caf%C3%A9.epub.ferry-"),
        "{}",
        reqs[2]
    );
    assert!(has_header(&reqs[2], "Overwrite: T"), "{}", reqs[2]);
    assert!(
        has_header(
            &reqs[2],
            &format!("Destination: {at}/remote.php/dav/files/me/caf%C3%A9.epub")
        ),
        "{}",
        reqs[2]
    );
}

#[test]
fn copies_to_and_from_files_on_this_computer() {
    let (asked, at) = serve(vec![
        ("207 Multi-Status", "", NEXTCLOUD),
        ("207 Multi-Status", "", NEXTCLOUD),
        ("200 OK", "", "the file's bytes"),
        ("201 Created", "", ""),
        ("204 No Content", "", ""),
        ("207 Multi-Status", "", NEXTCLOUD),
    ]);
    let dav = Dav::open(&format!("{at}/remote.php/dav/files/me"), None).unwrap();
    let dir = std::env::temp_dir().join(format!("ferry-dav-files-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (to, from) = (dir.join("got"), dir.join("send"));
    let mut seen = 0;
    let v = dav
        .read_to(
            "/café.epub",
            &to,
            &mut |n| seen += n,
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(
        (std::fs::read(&to).unwrap(), seen, v.as_str()),
        (b"the file's bytes".to_vec(), 16, "\"abc123\"")
    );
    std::fs::write(&from, b"sent from a file").unwrap();
    assert_eq!(
        dav.replace_from("/café.epub", &from, true).unwrap(),
        "\"abc123\""
    );
    std::fs::remove_dir_all(&dir).unwrap();
    let reqs = asked.join().unwrap();
    assert!(
        first(&reqs[3]).starts_with("PUT /remote.php/dav/files/me/.caf%C3%A9.epub.ferry-"),
        "{}",
        reqs[3]
    );
    assert!(
        has_header(&reqs[3], "Content-Length: 16") && reqs[3].ends_with("sent from a file"),
        "{}",
        reqs[3]
    );
    assert!(first(&reqs[4]).starts_with("MOVE "), "{}", reqs[4]);
}

#[test]
fn deletes_makes_and_renames() {
    let (asked, at) = serve(vec![
        ("207 Multi-Status", "", NEXTCLOUD),
        ("204 No Content", "", ""),
        ("201 Created", "", ""),
        ("405 Method Not Allowed", "", ""),
        ("201 Created", "", ""),
        ("412 Precondition Failed", "", ""),
    ]);
    let dav = Dav::open(&format!("{at}/dav"), None).unwrap();
    dav.delete("/old", true).unwrap();
    dav.make_dir("/new").unwrap();
    assert!(matches!(dav.make_dir("/new"), Err(DevError::Conflict(_))));
    dav.rename("/a/b.txt", "c d.txt").unwrap();
    assert!(matches!(
        dav.rename("/a/b.txt", "x"),
        Err(DevError::Conflict(_))
    ));
    let reqs = asked.join().unwrap();
    assert!(
        first(&reqs[1]).starts_with("DELETE /dav/old/ "),
        "{}",
        reqs[1]
    );
    assert!(
        first(&reqs[2]).starts_with("MKCOL /dav/new/ "),
        "{}",
        reqs[2]
    );
    assert!(
        first(&reqs[4]).starts_with("MOVE /dav/a/b.txt "),
        "{}",
        reqs[4]
    );
    assert!(has_header(&reqs[4], "Overwrite: F"), "{}", reqs[4]);
    assert!(
        has_header(&reqs[4], &format!("Destination: {at}/dav/a/c%20d.txt")),
        "{}",
        reqs[4]
    );
}

const NONCE: &str = "7ypf/xlj9XXwfDPEoM4URrv/xwf94BcCAzFZH4GiTo0v";
const CNONCE: &str = "f2/wE4q74E6zIJEtWaHKaf5wv/H5QzzpXusqGemxURZJ";

fn challenge(algo: &str) -> Digest {
    let line = format!("Digest realm=\"http-auth@example.org\", qop=\"auth, auth-int\", algorithm={algo}, nonce=\"{NONCE}\", opaque=\"FQhe/qaU925kfnzjCev0ciny7QMkPqMAFRtzCUYo5tdS\"");
    digest_challenge(&[&line]).unwrap()
}

#[test]
fn digest_matches_the_rfc_examples() {
    // RFC 7616, section 3.9.
    let h = challenge("SHA-256").header_with(
        "Mufasa",
        "Circle of Life",
        "GET",
        "/dir/index.html",
        1,
        CNONCE,
    );
    assert!(
        h.contains("response=\"753927fa0e85d155564e2e272a28d1802ca10daf4496794697cf8db5856cb6c1\""),
        "{h}"
    );
    assert!(
        h.contains("algorithm=SHA-256, ")
            && h.contains("qop=auth, nc=00000001")
            && h.contains("opaque=\"FQhe/"),
        "{h}"
    );
    let h = challenge("MD5").header_with(
        "Mufasa",
        "Circle of Life",
        "GET",
        "/dir/index.html",
        1,
        CNONCE,
    );
    assert!(
        h.contains("response=\"8ca523f5e9506fed4657c9700eebdbec\""),
        "{h}"
    );
}

#[test]
fn reads_digest_challenges() {
    let both = [
        "Basic realm=\"x\"",
        "Digest realm=\"a, b\", nonce=\"n1\", algorithm=MD5",
        "Digest realm=\"a, b\", nonce=\"n2\", algorithm=SHA-256, stale=TRUE",
    ];
    let d = digest_challenge(&both).unwrap();
    assert_eq!(
        (d.realm.as_str(), d.nonce.as_str(), d.sha256, d.stale, d.qop),
        ("a, b", "n2", true, true, false)
    );
    assert!(digest_challenge(&["Basic realm=\"x\""]).is_none());
    assert!(
        digest_challenge(&["Digest realm=\"x\", nonce=\"n\", algorithm=SHA-512-256"]).is_none()
    );
}

/// The `key="value"` parts of an Authorization header the client sent.
fn sent_params(req: &str) -> Vec<(String, String)> {
    const AT: &str = "authorization: digest ";
    let line = req
        .lines()
        .find(|l| {
            l.get(..AT.len())
                .is_some_and(|h| h.eq_ignore_ascii_case(AT))
        })
        .unwrap_or_else(|| panic!("no Digest in {req}"));
    params(&line[AT.len()..])
}

#[test]
fn logs_in_with_digest_and_retries_when_stale() {
    let (asked, at) = serve(vec![
            ("401 Unauthorized", "WWW-Authenticate: Digest realm=\"nas\", nonce=\"n1\", qop=\"auth\", algorithm=SHA-256\r\n", ""),
            ("207 Multi-Status", "", NEXTCLOUD),
            ("401 Unauthorized", "WWW-Authenticate: Digest realm=\"nas\", nonce=\"n2\", qop=\"auth\", algorithm=SHA-256, stale=true\r\n", ""),
            ("207 Multi-Status", "", NEXTCLOUD),
        ]);
    let dav = Dav::open(&format!("{at}/remote.php/dav/files/me"), Some(("me", "pw"))).unwrap();
    dav.listdir("/").unwrap();
    let reqs = asked.join().unwrap();
    // Basic first, then Digest.
    assert!(
        has_header(&reqs[0], "Authorization: Basic bWU6cHc="),
        "{}",
        reqs[0]
    );
    for (i, nonce) in [(1, "n1"), (3, "n2")] {
        let p = sent_params(&reqs[i]);
        let get = |k: &str| {
            p.iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        assert_eq!(
            (get("username"), get("realm"), get("nonce"), get("qop")),
            ("me".into(), "nas".into(), nonce.into(), "auth".into())
        );
        assert_eq!(get("algorithm"), "SHA-256");
        // The server could work out the same answer from what was sent.
        let d = Digest {
            realm: "nas".into(),
            nonce: nonce.into(),
            opaque: None,
            sha256: true,
            sess: false,
            qop: true,
            stale: false,
            count: 0,
        };
        let want = d.header_with(
            "me",
            "pw",
            reqs[i].split(' ').next().unwrap(),
            &get("uri"),
            u32::from_str_radix(&get("nc"), 16).unwrap(),
            &get("cnonce"),
        );
        assert_eq!(
            sent_params(&reqs[i]),
            params(want.strip_prefix("Digest ").unwrap()),
            "{}",
            reqs[i]
        );
        assert!(get("uri").starts_with("/remote.php/dav/files/me/"));
    }
    // The first Digest request used nc 1, the next 2 (the retry after the stale nonce, 1 again).
    assert!(sent_params(&reqs[1]).contains(&("nc".to_string(), "00000001".to_string())));
    assert!(sent_params(&reqs[2]).contains(&("nc".to_string(), "00000002".to_string())));
    assert!(sent_params(&reqs[3]).contains(&("nc".to_string(), "00000001".to_string())));
}

#[test]
fn a_wrong_digest_password_fails_once() {
    let ch = "WWW-Authenticate: Digest realm=\"nas\", nonce=\"n1\", qop=\"auth\"\r\n";
    let (asked, at) = serve(vec![
        ("401 Unauthorized", ch, ""),
        ("401 Unauthorized", ch, ""),
    ]);
    let e = Dav::open(&at, Some(("me", "bad"))).err().unwrap();
    assert!(e.contains("user name and password"), "{e}");
    assert_eq!(asked.join().unwrap().len(), 2);
}

#[test]
fn follows_redirects_keeping_method_and_body() {
    let (asked, at) = serve(vec![
        ("207 Multi-Status", "", NEXTCLOUD),
        ("307 Temporary Redirect", "Location: /moved/a.txt\r\n", ""),
        ("308 Permanent Redirect", "Location: b.txt\r\n", ""),
        ("201 Created", "", ""),
        ("307 Temporary Redirect", "Location: /moved/new\r\n", ""),
        ("201 Created", "", ""),
    ]);
    let dav = Dav::open(&format!("{at}/dav"), Some(("me", "pw"))).unwrap();
    // Two hops for the PUT, and the MOVE's Destination stays on the same server.
    dav.send("PUT", &format!("{at}/dav/a.txt"), &[], b"body")
        .unwrap();
    dav.move_to("/x", "/y", true).unwrap();
    let reqs = asked.join().unwrap();
    assert!(
        first(&reqs[2]).starts_with("PUT /moved/a.txt "),
        "{}",
        reqs[2]
    );
    assert!(
        first(&reqs[3]).starts_with("PUT /moved/b.txt "),
        "{}",
        reqs[3]
    );
    assert!(
        reqs[3].ends_with("body") && has_header(&reqs[3], "Authorization: Basic bWU6cHc="),
        "{}",
        reqs[3]
    );
    assert!(
        first(&reqs[5]).starts_with("MOVE /moved/new "),
        "{}",
        reqs[5]
    );
}

#[test]
fn redirects_stay_on_the_server() {
    assert!(may_follow("http://h/a", "http://h/b"));
    assert!(may_follow("http://h/a", "https://H/b"));
    assert!(may_follow("https://h:8443/a", "https://h:8443/b"));
    assert!(!may_follow("https://h/a", "http://h/b"));
    assert!(!may_follow("http://h/a", "http://other/b"));
    assert!(!may_follow("http://h:1/a", "http://h:2/b"));
    assert_eq!(
        resolve("http://h:1/dav/x/y", "z").unwrap(),
        "http://h:1/dav/x/z"
    );
    assert_eq!(
        resolve("http://h:1/dav/x/y?q", "/root").unwrap(),
        "http://h:1/root"
    );
    assert_eq!(resolve("http://h/a", "https://o/b").unwrap(), "https://o/b");
}

#[test]
fn gives_up_on_loops_and_other_servers() {
    let (asked, at) = serve(vec![
        ("307 Temporary Redirect", "Location: /again\r\n", "");
        6
    ]);
    let e = Dav::open(&at, None).err().unwrap();
    assert!(e.contains("sent us to"), "{e}");
    assert_eq!(asked.join().unwrap().len(), 6);
}

#[test]
fn says_what_went_wrong() {
    let (asked, at) = serve(vec![("401 Unauthorized", "", "")]);
    let e = Dav::open(&at, Some(("me", "bad"))).err().unwrap();
    assert!(e.contains("user name and password"), "{e}");
    asked.join().unwrap();
    let (asked, at) = serve(vec![("401 Unauthorized", "", "")]);
    assert!(Dav::open(&at, None)
        .err()
        .unwrap()
        .contains("asks for a user name"));
    asked.join().unwrap();
    let (asked, at) = serve(vec![("404 Not Found", "", "")]);
    assert!(Dav::open(&at, None)
        .err()
        .unwrap()
        .contains("isn't a WebDAV folder"));
    asked.join().unwrap();
    let (asked, at) = serve(vec![(
        "301 Moved Permanently",
        "Location: https://x.example/dav/\r\n",
        "",
    )]);
    assert!(Dav::open(&at, None)
        .err()
        .unwrap()
        .contains("https://x.example/dav/"));
    asked.join().unwrap();
    let (asked, at) = serve(vec![("200 OK", "", "<html>hi</html>")]);
    assert!(Dav::open(&at, None)
        .err()
        .unwrap()
        .contains("isn't a WebDAV folder"));
    asked.join().unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    assert!(Dav::open(&format!("http://127.0.0.1:{port}"), None)
        .err()
        .unwrap()
        .starts_with("Can't reach"));
}
