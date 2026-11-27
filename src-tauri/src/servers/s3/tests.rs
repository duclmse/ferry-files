use super::*;
use std::io::Write;

/// The GET Object example from AWS's Signature Version 4 documentation.
#[test]
fn signs_like_the_aws_example() {
    let hash = sha256_hex(b"");
    assert_eq!(
        hash,
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    let headers: Vec<(String, String)> = vec![
        ("host".into(), "examplebucket.s3.amazonaws.com".into()),
        ("range".into(), "bytes=0-9".into()),
        ("x-amz-content-sha256".into(), hash.clone()),
        ("x-amz-date".into(), "20130524T000000Z".into()),
    ];
    let keys = Keys {
        access: "AKIAIOSFODNN7EXAMPLE",
        secret: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        region: "us-east-1",
    };
    let auth = authorization(
        "GET",
        "/test.txt",
        "",
        &headers,
        &hash,
        &keys,
        "20130524T000000Z",
    );
    assert_eq!(
        auth,
        "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
    );
}

#[test]
fn encodes_names() {
    assert_eq!(encode_path("a b/ä+c~.txt"), "a%20b/%C3%A4%2Bc~.txt");
    assert_eq!(encode("a/b"), "a%2Fb");
    let q = canonical_query(&[
        ("prefix", "a b/".into()),
        ("list-type", "2".into()),
        ("delimiter", "/".into()),
    ]);
    assert_eq!(q, "delimiter=%2F&list-type=2&prefix=a%20b%2F");
}

#[test]
fn reads_a_list() {
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Name>b</Name><Prefix>a/</Prefix>
<NextContinuationToken>tok+1</NextContinuationToken><KeyCount>3</KeyCount><IsTruncated>true</IsTruncated>
<Contents><Key>a/</Key><LastModified>2024-01-02T03:04:05.000Z</LastModified><ETag>&quot;d41d8cd9&quot;</ETag><Size>0</Size></Contents>
<Contents><Key>a/x &amp; y.txt</Key><LastModified>2024-01-02T03:04:05.000Z</LastModified><ETag>&quot;abc&quot;</ETag><Size>12</Size></Contents>
<CommonPrefixes><Prefix>a/sub/</Prefix></CommonPrefixes></ListBucketResult>"#;
    let p = parse_list(xml).unwrap();
    assert_eq!(p.next.as_deref(), Some("tok+1"));
    assert_eq!(p.prefixes, ["a/sub/"]);
    assert_eq!(p.files.len(), 2);
    assert_eq!(
        p.files[1],
        Obj {
            key: "a/x & y.txt".into(),
            size: 12,
            mtime: 1_704_164_645,
            etag: "abc".into()
        }
    );
    let last = parse_list("<ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>")
        .unwrap();
    assert_eq!(last, Page::default());
    assert!(parse_list("<html/>").is_err());
}

#[test]
fn explains_errors() {
    let e = |code: &str, msg: &str| {
        format!("<Error><Code>{code}</Code><Message>{msg}</Message></Error>")
    };
    assert!(explain(403, &e("InvalidAccessKeyId", "x"), "b").contains("access key"));
    assert!(explain(403, &e("SignatureDoesNotMatch", "x"), "b").contains("secret key"));
    assert!(explain(404, &e("NoSuchBucket", "x"), "photos").contains("photos doesn't exist"));
    assert!(explain(403, &e("AccessDenied", "x"), "b").starts_with("Access denied"));
    let region = explain(
        400,
        &e(
            "AuthorizationHeaderMalformed",
            "the region 'us-east-1' is wrong; expecting 'eu-west-1'",
        ),
        "b",
    );
    assert_eq!(region, "The region is wrong. This bucket is in eu-west-1.");
    assert!(explain(403, "", "b").starts_with("Access denied"));
    assert_eq!(
        explain(500, &e("InternalError", "Try again"), "b"),
        "S3 says: Try again"
    );
}

struct Req {
    head: String,
    body: Vec<u8>,
}

impl Req {
    fn first(&self) -> &str {
        self.head.lines().next().unwrap_or_default()
    }

    fn has(&self, header: &str) -> bool {
        self.head.to_lowercase().contains(&header.to_lowercase())
    }
}

/// A server on a free port giving `answers` (status, extra header lines, body) in turn: what it
/// was asked, and the address to ask it at. A HEAD answer sends no body, only its headers.
fn serve(
    answers: Vec<(&'static str, &'static str, &'static str)>,
) -> (std::thread::JoinHandle<Vec<Req>>, String) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let at = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let asked = std::thread::spawn(move || {
        let mut reqs = vec![];
        for (status, headers, body) in answers {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = vec![];
            let mut chunk = [0u8; 4096];
            let end = loop {
                let n = s.read(&mut chunk).unwrap();
                buf.extend_from_slice(&chunk[..n]);
                if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break i + 4;
                }
                assert!(n > 0, "request ended early");
            };
            let head = String::from_utf8_lossy(&buf[..end]).to_string();
            let want = head
                .lines()
                .find_map(|l| {
                    l.to_lowercase()
                        .strip_prefix("content-length:")
                        .map(|n| n.trim().parse::<usize>().unwrap_or(0))
                })
                .unwrap_or(0);
            while buf.len() < end + want {
                let n = s.read(&mut chunk).unwrap();
                buf.extend_from_slice(&chunk[..n]);
            }
            let reply = if head.starts_with("HEAD ") {
                format!("HTTP/1.1 {status}\r\n{headers}Connection: close\r\n\r\n")
            } else {
                format!("HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
            };
            s.write_all(reply.as_bytes()).unwrap();
            reqs.push(Req {
                head,
                body: buf[end..].to_vec(),
            });
        }
        reqs
    });
    (asked, at)
}

const EMPTY_LIST: &str = "<ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>";

#[test]
fn lists_and_stats() {
    let page1 = "<ListBucketResult><IsTruncated>true</IsTruncated><NextContinuationToken>t1</NextContinuationToken>\
            <Contents><Key>a/</Key><Size>0</Size></Contents>\
            <Contents><Key>a/one.txt</Key><LastModified>2024-01-02T03:04:05.000Z</LastModified><ETag>&quot;e1&quot;</ETag><Size>3</Size></Contents></ListBucketResult>";
    let page2 = "<ListBucketResult><IsTruncated>false</IsTruncated>\
            <CommonPrefixes><Prefix>a/sub/</Prefix></CommonPrefixes></ListBucketResult>";
    let one_key = "<ListBucketResult><IsTruncated>false</IsTruncated><Contents><Key>a/sub/x</Key><Size>1</Size></Contents></ListBucketResult>";
    let (asked, at) = serve(vec![
            ("200 OK", "", EMPTY_LIST),
            ("200 OK", "", page1),
            ("200 OK", "", page2),
            ("200 OK", "Content-Length: 7\r\nETag: \"abc\"\r\nLast-Modified: Tue, 02 Jan 2024 03:04:05 GMT\r\n", ""),
            ("404 Not Found", "Content-Length: 0\r\n", ""),
            ("200 OK", "", one_key),
            ("404 Not Found", "Content-Length: 0\r\n", ""),
            ("200 OK", "", EMPTY_LIST),
        ]);
    let s3 = S3::open(&at, "", "bkt", "AK", "SK").unwrap();
    assert_eq!(s3.label(), "bkt");
    let items = s3.listdir("/a").unwrap();
    let shown: Vec<(&str, &str, bool, u64)> = items
        .iter()
        .map(|e| (e.name.as_str(), e.path.as_str(), e.dir, e.size))
        .collect();
    assert_eq!(
        shown,
        [
            ("sub", "/a/sub", true, 0),
            ("one.txt", "/a/one.txt", false, 3)
        ]
    );
    assert_eq!(items[1].mtime, 1_704_164_645);
    let f = s3.stat("/a/one.txt").unwrap().unwrap();
    assert_eq!((f.dir, f.size, f.mtime), (false, 7, 1_704_164_645));
    assert!(s3.stat("/a/sub").unwrap().unwrap().dir);
    assert!(s3.stat("/nope").unwrap().is_none());

    let reqs = asked.join().unwrap();
    assert!(
        reqs[0]
            .first()
            .starts_with("GET /bkt?list-type=2&max-keys=1&prefix= "),
        "{}",
        reqs[0].first()
    );
    assert!(
        reqs[1]
            .first()
            .contains("delimiter=%2F&list-type=2&max-keys=1000&prefix=a%2F"),
        "{}",
        reqs[1].first()
    );
    assert!(
        reqs[2].first().contains("continuation-token=t1"),
        "{}",
        reqs[2].first()
    );
    assert!(
        reqs[3].first().starts_with("HEAD /bkt/a/one.txt "),
        "{}",
        reqs[3].first()
    );
    assert!(
        reqs[5].first().contains("max-keys=1&prefix=a%2Fsub%2F"),
        "{}",
        reqs[5].first()
    );
    assert!(reqs[0].has("authorization: AWS4-HMAC-SHA256 Credential=AK/"));
    assert!(reqs[0].has("SignedHeaders=host;x-amz-content-sha256;x-amz-date,"));
    assert!(reqs[0].has("host: 127.0.0.1:"));
}

#[test]
fn reads_ranges_and_writes() {
    let (asked, at) = serve(vec![
        ("200 OK", "", EMPTY_LIST),
        ("206 Partial Content", "", "ell"),
        ("200 OK", "", "hello"),
        ("200 OK", "ETag: \"v1\"\r\n", "hello"),
        ("200 OK", "ETag: \"v2\"\r\n", ""),
        (
            "416 Range Not Satisfiable",
            "",
            "<Error><Code>InvalidRange</Code></Error>",
        ),
    ]);
    let s3 = S3::open(&at, "auto", "bkt", "AK", "SK").unwrap();
    assert_eq!(s3.read_range("/dir/a b.txt", 1, 3).unwrap(), b"ell");
    // The server ignored the range.
    assert_eq!(s3.read_range("/x", 1, 3).unwrap(), b"ell");
    assert_eq!(
        s3.read("/x").unwrap(),
        (b"hello".to_vec(), "v1".to_string())
    );
    assert_eq!(s3.replace("/x", b"new text", true).unwrap(), "v2");
    assert!(s3.read_range("/x", 99, 3).unwrap().is_empty());

    let reqs = asked.join().unwrap();
    assert!(
        reqs[1].first().starts_with("GET /bkt/dir/a%20b.txt "),
        "{}",
        reqs[1].first()
    );
    assert!(reqs[1].has("range: bytes=1-3"));
    assert!(reqs[1].has("SignedHeaders=host;range;x-amz-content-sha256;x-amz-date,"));
    assert!(reqs[1].has("/auto/s3/aws4_request"));
    assert!(
        reqs[4].first().starts_with("PUT /bkt/x "),
        "{}",
        reqs[4].first()
    );
    assert_eq!(reqs[4].body, b"new text");
    assert!(reqs[4].has(&format!(
        "x-amz-content-sha256: {}",
        sha256_hex(b"new text")
    )));
}

#[test]
fn renames_by_copy_and_delete() {
    let copied = "<CopyObjectResult><ETag>&quot;x&quot;</ETag></CopyObjectResult>";
    let (asked, at) = serve(vec![
        ("200 OK", "", EMPTY_LIST),
        ("200 OK", "Content-Length: 3\r\n", ""),
        ("200 OK", "", copied),
        ("204 No Content", "", ""),
        ("200 OK", "Content-Length: 3\r\n", ""),
        (
            "200 OK",
            "",
            "<Error><Code>InternalError</Code><Message>Copy broke</Message></Error>",
        ),
    ]);
    let s3 = S3::open(&at, "", "bkt", "AK", "SK").unwrap();
    s3.rename("/a/old name.txt", "new.txt").unwrap();
    let e = s3.rename("/a/new.txt", "b.txt").unwrap_err();
    assert!(e.to_string().contains("Copy broke"), "{e}");
    let reqs = asked.join().unwrap();
    assert!(
        reqs[2].first().starts_with("PUT /bkt/a/new.txt "),
        "{}",
        reqs[2].first()
    );
    assert!(reqs[2].has("x-amz-copy-source: /bkt/a/old%20name.txt"));
    assert!(reqs[2].has("SignedHeaders=host;x-amz-content-sha256;x-amz-copy-source;x-amz-date,"));
    assert!(
        reqs[3].first().starts_with("DELETE /bkt/a/old%20name.txt "),
        "{}",
        reqs[3].first()
    );
}

#[test]
fn renames_and_deletes_a_folder() {
    let keys = "<ListBucketResult><IsTruncated>false</IsTruncated>\
            <Contents><Key>d/</Key><Size>0</Size></Contents><Contents><Key>d/f</Key><Size>1</Size></Contents></ListBucketResult>";
    let copied = "<CopyObjectResult/>";
    let (asked, at) = serve(vec![
        ("200 OK", "", EMPTY_LIST),
        // rename: HEAD misses, a key is under the prefix, then the list, copies and deletes.
        ("404 Not Found", "Content-Length: 0\r\n", ""),
        ("200 OK", "", keys),
        ("200 OK", "", keys),
        ("200 OK", "", copied),
        ("200 OK", "", copied),
        ("204 No Content", "", ""),
        ("204 No Content", "", ""),
        // delete: list, then each key and the folder's own name.
        ("200 OK", "", keys),
        ("204 No Content", "", ""),
        ("204 No Content", "", ""),
        ("204 No Content", "", ""),
    ]);
    let s3 = S3::open(&at, "", "bkt", "AK", "SK").unwrap();
    s3.rename("/d", "e").unwrap();
    s3.delete("/d", true).unwrap();
    let reqs = asked.join().unwrap();
    assert!(
        reqs[4].first().starts_with("PUT /bkt/e/ "),
        "{}",
        reqs[4].first()
    );
    assert!(
        reqs[5].first().starts_with("PUT /bkt/e/f "),
        "{}",
        reqs[5].first()
    );
    assert!(
        reqs[6].first().starts_with("DELETE /bkt/d/ "),
        "{}",
        reqs[6].first()
    );
    assert!(
        reqs[11].first().starts_with("DELETE /bkt/d "),
        "{}",
        reqs[11].first()
    );
}

#[test]
fn makes_a_folder_marker() {
    let (asked, at) = serve(vec![
        ("200 OK", "", EMPTY_LIST),
        ("200 OK", "ETag: \"d41d8cd9\"\r\n", ""),
    ]);
    let s3 = S3::open(&at, "", "bkt", "AK", "SK").unwrap();
    s3.make_dir("/a/b").unwrap();
    let reqs = asked.join().unwrap();
    assert!(
        reqs[1].first().starts_with("PUT /bkt/a/b/ "),
        "{}",
        reqs[1].first()
    );
    assert!(reqs[1].body.is_empty());
}

const STARTED: &str = "<InitiateMultipartUploadResult><Bucket>bkt</Bucket><Key>k</Key><UploadId>up+1/x</UploadId></InitiateMultipartUploadResult>";
const COMPLETED: &str = "<CompleteMultipartUploadResult><Location>x</Location><ETag>&quot;whole-3&quot;</ETag></CompleteMultipartUploadResult>";

/// A bucket whose objects over 5 bytes go in parts of 4.
fn small_parts(at: &str) -> S3 {
    let mut s3 = S3::open(at, "", "bkt", "AK", "SK").unwrap();
    (s3.single_max, s3.part_size) = (5, 4);
    s3
}

#[test]
fn uploads_big_objects_in_parts() {
    let (asked, at) = serve(vec![
        ("200 OK", "", EMPTY_LIST),
        ("200 OK", "", STARTED),
        ("200 OK", "ETag: \"p1\"\r\n", ""),
        ("200 OK", "ETag: \"p2\"\r\n", ""),
        ("200 OK", "ETag: \"p3\"\r\n", ""),
        ("200 OK", "", COMPLETED),
        // Small ones stay one PUT.
        ("200 OK", "ETag: \"one\"\r\n", ""),
    ]);
    let s3 = small_parts(&at);
    assert_eq!(
        s3.replace("/d/big.bin", b"0123456789", true).unwrap(),
        "whole-3"
    );
    assert_eq!(s3.replace("/small", b"abc", false).unwrap(), "one");
    let reqs = asked.join().unwrap();
    assert!(
        reqs[1].first().starts_with("POST /bkt/d/big.bin?uploads= "),
        "{}",
        reqs[1].first()
    );
    let (id, lens) = ("uploadId=up%2B1%2Fx", [4, 4, 2]);
    for n in 0..3 {
        assert!(
            reqs[2 + n]
                .first()
                .starts_with(&format!("PUT /bkt/d/big.bin?partNumber={}&{id} ", n + 1)),
            "{}",
            reqs[2 + n].first()
        );
        assert_eq!(reqs[2 + n].body, b"0123456789"[n * 4..n * 4 + lens[n]]);
    }
    assert!(
        reqs[5]
            .first()
            .starts_with(&format!("POST /bkt/d/big.bin?{id} ")),
        "{}",
        reqs[5].first()
    );
    let xml = String::from_utf8_lossy(&reqs[5].body).to_string();
    assert_eq!(
            xml,
            "<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>\"p1\"</ETag></Part><Part><PartNumber>2</PartNumber><ETag>\"p2\"</ETag></Part>\
             <Part><PartNumber>3</PartNumber><ETag>\"p3\"</ETag></Part></CompleteMultipartUpload>"
        );
    assert!(
        reqs[6].first().starts_with("PUT /bkt/small "),
        "{}",
        reqs[6].first()
    );
}

#[test]
fn uploads_and_downloads_through_files() {
    let (asked, at) = serve(vec![
        ("200 OK", "", EMPTY_LIST),
        ("200 OK", "", STARTED),
        ("200 OK", "ETag: \"p1\"\r\n", ""),
        ("200 OK", "ETag: \"p2\"\r\n", ""),
        ("200 OK", "ETag: \"p3\"\r\n", ""),
        ("200 OK", "", COMPLETED),
        ("200 OK", "ETag: \"one\"\r\n", ""),
        ("200 OK", "ETag: \"got\"\r\n", "0123456789"),
    ]);
    let s3 = small_parts(&at);
    let dir = std::env::temp_dir().join(format!("ferry-s3-files-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (big, small, to) = (dir.join("big"), dir.join("small"), dir.join("got"));
    std::fs::write(&big, b"0123456789").unwrap();
    std::fs::write(&small, b"abc").unwrap();
    assert_eq!(
        s3.replace_from("/d/big.bin", &big, true).unwrap(),
        "whole-3"
    );
    assert_eq!(s3.replace_from("/small", &small, false).unwrap(), "one");
    let mut seen = 0;
    assert_eq!(
        s3.read_to(
            "/d/big.bin",
            &to,
            &mut |n| seen += n,
            &std::sync::atomic::AtomicBool::new(false)
        )
        .unwrap(),
        "got"
    );
    assert_eq!(
        (std::fs::read(&to).unwrap(), seen),
        (b"0123456789".to_vec(), 10)
    );
    std::fs::remove_dir_all(&dir).unwrap();
    let reqs = asked.join().unwrap();
    let lens = [4, 4, 2];
    for n in 0..3 {
        assert!(
            reqs[2 + n]
                .first()
                .starts_with(&format!("PUT /bkt/d/big.bin?partNumber={}&", n + 1)),
            "{}",
            reqs[2 + n].first()
        );
        assert_eq!(reqs[2 + n].body, b"0123456789"[n * 4..n * 4 + lens[n]]);
        // Signed with the hash of the part that was sent.
        let hash = sha256_hex(&reqs[2 + n].body);
        assert!(
            reqs[2 + n]
                .head
                .to_lowercase()
                .contains(&format!("x-amz-content-sha256: {hash}")),
            "{}",
            reqs[2 + n].head
        );
    }
    assert!(
        reqs[6].first().starts_with("PUT /bkt/small ") && reqs[6].body == b"abc",
        "{}",
        reqs[6].first()
    );
}

#[test]
fn gives_up_a_failed_multipart_upload() {
    let (asked, at) = serve(vec![
        ("200 OK", "", EMPTY_LIST),
        ("200 OK", "", STARTED),
        ("200 OK", "ETag: \"p1\"\r\n", ""),
        (
            "500 Internal Server Error",
            "",
            "<Error><Code>InternalError</Code><Message>Part broke</Message></Error>",
        ),
        ("204 No Content", "", ""),
    ]);
    let s3 = small_parts(&at);
    let e = s3.replace("/big.bin", b"0123456789", true).unwrap_err();
    assert!(e.to_string().contains("Part broke"), "{e}");
    let reqs = asked.join().unwrap();
    assert!(
        reqs[4]
            .first()
            .starts_with("DELETE /bkt/big.bin?uploadId=up%2B1%2Fx "),
        "{}",
        reqs[4].first()
    );
}

#[test]
fn copies_big_objects_in_parts() {
    let part = |e: &'static str| -> (&'static str, &'static str, &'static str) {
        (
            "200 OK",
            "",
            Box::leak(
                format!("<CopyPartResult><ETag>&quot;{e}&quot;</ETag></CopyPartResult>")
                    .into_boxed_str(),
            ),
        )
    };
    let (asked, at) = serve(vec![
        ("200 OK", "", EMPTY_LIST),
        // rename: HEAD says 10 bytes.
        ("200 OK", "Content-Length: 10\r\n", ""),
        ("200 OK", "", STARTED),
        part("c1"),
        part("c2"),
        part("c3"),
        ("200 OK", "", COMPLETED),
        ("204 No Content", "", ""),
    ]);
    let s3 = small_parts(&at);
    s3.rename("/a/old.bin", "new.bin").unwrap();
    let reqs = asked.join().unwrap();
    assert!(
        reqs[2].first().starts_with("POST /bkt/a/new.bin?uploads= "),
        "{}",
        reqs[2].first()
    );
    for (n, range) in ["bytes=0-3", "bytes=4-7", "bytes=8-9"].iter().enumerate() {
        let r = &reqs[3 + n];
        assert!(
            r.first().starts_with(&format!(
                "PUT /bkt/a/new.bin?partNumber={}&uploadId=up%2B1%2Fx ",
                n + 1
            )),
            "{}",
            r.first()
        );
        assert!(
            r.has(&format!("x-amz-copy-source-range: {range}"))
                && r.has("x-amz-copy-source: /bkt/a/old.bin"),
            "{}",
            r.head
        );
        assert!(r.has("SignedHeaders=host;x-amz-content-sha256;x-amz-copy-source;x-amz-copy-source-range;x-amz-date,"), "{}", r.head);
    }
    assert!(String::from_utf8_lossy(&reqs[6].body).contains("<ETag>\"c3\"</ETag>"));
    // The old one goes only after the copy is complete.
    assert!(
        reqs[7].first().starts_with("DELETE /bkt/a/old.bin "),
        "{}",
        reqs[7].first()
    );
}

#[test]
fn host_header_has_the_port_only_when_its_not_the_default() {
    assert_eq!(host_header("https://s3.amazonaws.com"), "s3.amazonaws.com");
    assert_eq!(
        host_header("https://s3.amazonaws.com:443"),
        "s3.amazonaws.com"
    );
    assert_eq!(host_header("https://minio.lan:9000"), "minio.lan:9000");
    assert_eq!(host_header("https://minio.lan:1443"), "minio.lan:1443");
    assert_eq!(host_header("http://nas:80"), "nas");
    assert_eq!(host_header("http://nas:9000"), "nas:9000");
    assert_eq!(host_header("http://nas:443"), "nas:443");
}

/// Signs the request again from what the server received, to see that the host it was signed
/// with is the Host header that arrived.
fn signed_host_matches(r: &Req, want_host: &str) {
    let header = |name: &str| {
        r.head
            .lines()
            .find_map(|l| {
                l.split_once(": ")
                    .filter(|(k, _)| k.eq_ignore_ascii_case(name))
                    .map(|(_, v)| v)
            })
            .unwrap()
            .to_string()
    };
    let host = header("Host");
    assert_eq!(host, want_host);
    let auth = header("Authorization");
    assert!(auth.contains("SignedHeaders=host;"), "{auth}");
    let (target, _) = r
        .first()
        .strip_prefix("GET ")
        .unwrap()
        .rsplit_once(" HTTP")
        .unwrap();
    let (uri, query) = target.split_once('?').unwrap_or((target, ""));
    let (date, hash) = (header("x-amz-date"), header("x-amz-content-sha256"));
    let headers: Vec<(String, String)> = vec![
        ("host".into(), host),
        ("x-amz-content-sha256".into(), hash.clone()),
        ("x-amz-date".into(), date.clone()),
    ];
    let keys = Keys {
        access: "AK",
        secret: "SK",
        region: "us-east-1",
    };
    assert_eq!(
        authorization("GET", uri, query, &headers, &hash, &keys, &date),
        auth
    );
}

#[test]
fn signs_the_host_it_sends_with_a_port() {
    // A port that isn't the default, like MinIO on :9000, and a host as typed (capitals).
    let (asked, at) = serve(vec![("200 OK", "", EMPTY_LIST)]);
    let port = at.rsplit(':').next().unwrap().to_string();
    let s3 = S3::open(&format!("http://LOCALHOST:{port}"), "", "bkt", "AK", "SK").unwrap();
    assert_eq!(s3.host, format!("LOCALHOST:{port}"));
    let reqs = asked.join().unwrap();
    signed_host_matches(&reqs[0], &format!("LOCALHOST:{port}"));
    let (asked, at) = serve(vec![("200 OK", "", EMPTY_LIST)]);
    let _ = S3::open(&at, "", "bkt", "AK", "SK").unwrap();
    let reqs = asked.join().unwrap();
    signed_host_matches(&reqs[0], at.strip_prefix("http://").unwrap());
}

#[test]
fn says_what_went_wrong() {
    let (asked, at) = serve(vec![(
        "403 Forbidden",
        "",
        "<Error><Code>InvalidAccessKeyId</Code><Message>The key does not exist</Message></Error>",
    )]);
    assert!(S3::open(&at, "", "bkt", "AK", "SK")
        .err()
        .unwrap()
        .contains("access key"));
    asked.join().unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    assert!(
        S3::open(&format!("127.0.0.1:{port}"), "", "bkt", "AK", "SK")
            .err()
            .unwrap()
            .starts_with("Can't reach")
    );
    assert!(S3::open("", "", "", "AK", "SK")
        .err()
        .unwrap()
        .contains("bucket"));
    assert!(S3::open("", "", "b", "AK", "")
        .err()
        .unwrap()
        .contains("secret key"));
    assert!(S3::open("https://host/folder", "", "b", "AK", "SK").is_err());
}
