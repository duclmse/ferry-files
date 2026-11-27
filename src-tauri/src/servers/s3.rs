//! S3 buckets (AWS, Cloudflare R2, Backblaze B2, MinIO…) as drives. Requests are signed with
//! AWS Signature Version 4 and use path-style addresses (`endpoint/bucket/key`).
//!
//! Paths are the app's: "/" is the bucket's top, "/a/b.txt" is the key "a/b.txt". S3 has no
//! folders: a folder is a prefix, and one made empty is kept as a zero-byte key "a/b/".

use crate::device::{basename, dirname, err, Result, Transport};
use crate::devices::connect;
use crate::servers::web;
use crate::*;
use hmac::{Hmac, KeyInit, Mac};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::time::Duration;

/// Most keys a folder rename copies.
const RENAME_MAX: usize = 1000;

/// The biggest object one PUT or one CopyObject takes: 5 GiB. Bigger goes in parts.
const SINGLE_MAX: u64 = 5 * 1024 * 1024 * 1024;

/// How big the parts are: 512 MiB (S3 allows 5 MiB to 5 GiB, and 10,000 parts).
const PART_SIZE: u64 = 512 * 1024 * 1024;

/// What stays as it is in an encoded name: letters, digits and - _ . ~
const UNRESERVED: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');
/// The same, and "/", for a path.
const PATH: &AsciiSet = &UNRESERVED.remove(b'/');

fn encode(s: &str) -> String {
    utf8_percent_encode(s, UNRESERVED).to_string()
}

fn encode_path(s: &str) -> String {
    utf8_percent_encode(s, PATH).to_string()
}

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// What a request sends: bytes, or a byte range of a file on this computer, read as it's sent.
enum Body<'a> {
    Bytes(&'a [u8]),
    File(&'a std::path::Path, std::ops::Range<u64>),
}

/// Bytes `r` of file `from`, to read.
fn part_of(from: &std::path::Path, r: std::ops::Range<u64>) -> Res<std::io::Take<std::fs::File>> {
    use std::io::{Seek, SeekFrom};
    let fail = |e: std::io::Error| Fail {
        status: 0,
        msg: format!("Can't read {}: {e}", from.display()),
    };
    let mut f = std::fs::File::open(from).map_err(fail)?;
    f.seek(SeekFrom::Start(r.start)).map_err(fail)?;
    Ok(f.take(r.end - r.start))
}

/// The SHA-256 of bytes `r` of file `from`, in hex: the request is signed with it, so the file is
/// read once for that and again as it's sent.
fn file_sha256(from: &std::path::Path, r: std::ops::Range<u64>) -> Res<String> {
    let mut part = part_of(from, r.clone())?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut got = 0u64;
    loop {
        let n = part.read(&mut buf).map_err(|e| Fail {
            status: 0,
            msg: format!("Can't read {}: {e}", from.display()),
        })?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        got += n as u64;
    }
    if got != r.end - r.start {
        return Err(Fail {
            status: 0,
            msg: format!("{} changed while it was being sent.", from.display()),
        });
    }
    Ok(hex::encode(h.finalize()))
}

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    let mut m = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes keys of any size");
    m.update(data.as_bytes());
    m.finalize().into_bytes().to_vec()
}

/// The sorted, encoded query string ("a=1&b=2"), the same for the address and the signature.
fn canonical_query(params: &[(&str, String)]) -> String {
    let mut pairs: Vec<(String, String)> =
        params.iter().map(|(k, v)| (encode(k), encode(v))).collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// What a request is signed with.
struct Keys<'a> {
    access: &'a str,
    secret: &'a str,
    region: &'a str,
}

/// The Authorization header's value. `headers` are (lowercase name, value) and must hold every
/// header to sign (host, x-amz-content-sha256, x-amz-date, and any others); `uri` and `query`
/// are already encoded.
fn authorization(
    method: &str,
    uri: &str,
    query: &str,
    headers: &[(String, String)],
    payload_hash: &str,
    keys: &Keys,
    amz_date: &str,
) -> String {
    let mut sorted: Vec<(&str, &str)> = headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.trim()))
        .collect();
    sorted.sort();
    let canon_headers: String = sorted.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed = sorted.iter().map(|(k, _)| *k).collect::<Vec<_>>().join(";");
    let request = format!("{method}\n{uri}\n{query}\n{canon_headers}\n{signed}\n{payload_hash}");
    let day = &amz_date[..8];
    let scope = format!("{day}/{}/s3/aws4_request", keys.region);
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(request.as_bytes())
    );
    let k = hmac(format!("AWS4{}", keys.secret).as_bytes(), day);
    let k = hmac(&k, keys.region);
    let k = hmac(&k, "s3");
    let k = hmac(&k, "aws4_request");
    let signature = hex::encode(hmac(&k, &to_sign));
    format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}",
        keys.access
    )
}

/// A failed request: the HTTP status (0 when there was no answer) and what to tell the user.
#[derive(Debug)]
struct Fail {
    status: u16,
    msg: String,
}

impl From<Fail> for DevError {
    fn from(f: Fail) -> DevError {
        DevError::Msg(f.msg)
    }
}

type Res<T> = std::result::Result<T, Fail>;

/// The text of each element that has some, with its path ("ListBucketResult/Contents/Key").
fn leaves(xml: &str) -> std::result::Result<Vec<(String, String)>, String> {
    use crate::books::xml::{tokenize, Kind};
    let mut stack: Vec<String> = vec![];
    let mut out = vec![];
    for t in tokenize(xml)? {
        match t.kind {
            Kind::Open => stack.push(t.local().to_string()),
            Kind::Close => {
                stack.pop();
            }
            Kind::Text | Kind::CData => {
                let raw = &xml[t.start..t.end];
                if !stack.is_empty() && !raw.trim().is_empty() {
                    let text = if t.kind == Kind::CData {
                        raw[9..raw.len() - 3].to_string()
                    } else {
                        crate::books::xml::unescape(raw)
                    };
                    out.push((stack.join("/"), text));
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

/// One key in a listing.
#[derive(Debug, Default, PartialEq)]
struct Obj {
    key: String,
    size: u64,
    mtime: i64,
    etag: String,
}

/// One page of a ListObjectsV2 answer.
#[derive(Debug, Default, PartialEq)]
struct Page {
    files: Vec<Obj>,
    /// Folders ("a/b/").
    prefixes: Vec<String>,
    next: Option<String>,
}

fn parse_list(xml: &str) -> std::result::Result<Page, String> {
    let mut page = Page::default();
    let mut truncated = false;
    let mut token = None;
    for (path, text) in leaves(xml)? {
        match path.as_str() {
            "ListBucketResult/Contents/Key" => page.files.push(Obj {
                key: text,
                ..Obj::default()
            }),
            "ListBucketResult/Contents/Size" => {
                if let Some(o) = page.files.last_mut() {
                    o.size = text.trim().parse().unwrap_or(0);
                }
            }
            "ListBucketResult/Contents/LastModified" => {
                if let Some(o) = page.files.last_mut() {
                    o.mtime = chrono::DateTime::parse_from_rfc3339(text.trim())
                        .map_or(0, |d| d.timestamp());
                }
            }
            "ListBucketResult/Contents/ETag" => {
                if let Some(o) = page.files.last_mut() {
                    o.etag = text.trim_matches('"').to_string();
                }
            }
            "ListBucketResult/CommonPrefixes/Prefix" => page.prefixes.push(text),
            "ListBucketResult/IsTruncated" => truncated = text.trim() == "true",
            "ListBucketResult/NextContinuationToken" => token = Some(text),
            _ => {}
        }
    }
    if !xml.contains("ListBucketResult") {
        return Err("The answer wasn't a list of files. Is the address an S3 server?".into());
    }
    page.next = if truncated { token } else { None };
    Ok(page)
}

/// What to tell the user about an S3 error answer.
fn explain(status: u16, body: &str, bucket: &str) -> String {
    let fields = leaves(body).unwrap_or_default();
    let field = |name: &str| {
        fields
            .iter()
            .find(|(p, _)| p == name)
            .map(|(_, v)| v.as_str())
            .unwrap_or("")
    };
    let (code, message) = (field("Error/Code"), field("Error/Message"));
    match code {
        "InvalidAccessKeyId" | "SignatureDoesNotMatch" => {
            "The access key or the secret key is wrong. Check them in Settings > Connections."
                .into()
        }
        "NoSuchBucket" => format!("The bucket {bucket} doesn't exist at this address."),
        "AccessDenied" => {
            "Access denied. This key may not be allowed to do that in the bucket.".into()
        }
        "NoSuchKey" => "That file doesn't exist.".into(),
        "RequestTimeTooSkewed" => {
            "This computer's clock is too far off for S3. Set the time and try again.".into()
        }
        "AuthorizationHeaderMalformed"
        | "PermanentRedirect"
        | "IllegalLocationConstraintException" => {
            let hint = message
                .split_once("expecting '")
                .and_then(|(_, r)| r.split_once('\''))
                .map(|(r, _)| r);
            match hint {
                Some(r) => format!("The region is wrong. This bucket is in {r}."),
                None => "The region is wrong for this bucket. Check it in Settings > Connections."
                    .into(),
            }
        }
        "" => match status {
            301 | 307 => "The region or the address is wrong for this bucket.".into(),
            403 => "Access denied. The keys may be wrong, or not allowed to do that in the bucket."
                .into(),
            404 => "Not found.".into(),
            _ => format!("S3 answered with error {status}."),
        },
        _ if !message.is_empty() => format!("S3 says: {message}"),
        _ => format!("S3 answered with error {status} ({code})."),
    }
}

fn agent() -> ureq::Agent {
    web::config()
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_per_read(Some(Duration::from_secs(60)))
        .timeout_per_write(Some(Duration::from_secs(60)))
        .max_redirects(0)
        .build()
        .into()
}

pub struct S3 {
    /// "https://host[:port]", no trailing slash.
    base: String,
    /// What the Host header holds: the address without the scheme.
    host: String,
    region: String,
    bucket: String,
    access: String,
    secret: String,
    /// Objects over this many bytes are written and copied in parts.
    single_max: u64,
    /// The size of those parts.
    part_size: u64,
}

/// What the Host header holds for an address ("https://host:9000" gives "host:9000"): the port
/// only when it isn't the scheme's own. It's what's signed, and what's sent.
fn host_header(base: &str) -> String {
    let host = base.split_once("://").map_or("", |(_, h)| h);
    if base.starts_with("https://") {
        host.strip_suffix(":443").unwrap_or(host)
    } else {
        host.strip_suffix(":80").unwrap_or(host)
    }
    .to_string()
}

/// An answer's ETag without its quotes.
fn etag_of(res: &web::Response) -> String {
    web::header(res, "ETag")
        .unwrap_or("")
        .trim_matches('"')
        .to_string()
}

impl S3 {
    /// Connects to `bucket`: the keys must be allowed to list it.
    pub fn open(
        endpoint: &str,
        region: &str,
        bucket: &str,
        access: &str,
        secret: &str,
    ) -> std::result::Result<Self, String> {
        let region = if region.trim().is_empty() {
            "us-east-1"
        } else {
            region.trim()
        };
        let bucket = bucket.trim();
        if bucket.is_empty() {
            return Err("Type the name of the bucket.".into());
        }
        if access.trim().is_empty() || secret.is_empty() {
            return Err("Type the access key and the secret key.".into());
        }
        let ep = endpoint.trim().trim_end_matches('/');
        let base = match ep {
            "" => format!("https://s3.{region}.amazonaws.com"),
            e if e.contains("://") => e.to_string(),
            e => format!("https://{e}"),
        };
        let host = base.split_once("://").map_or("", |(_, h)| h).to_string();
        if host.is_empty() || host.contains('/') {
            return Err(
                "Use an address like https://s3.us-east-1.amazonaws.com, without a folder.".into(),
            );
        }
        let host = host_header(&base);
        let s3 = S3 {
            base,
            host,
            region: region.into(),
            bucket: bucket.into(),
            access: access.trim().into(),
            secret: secret.into(),
            single_max: SINGLE_MAX,
            part_size: PART_SIZE,
        };
        s3.list_page("", None, 1, None).map_err(|f| f.msg)?;
        Ok(s3)
    }

    /// Sends a request for `key` ("" for the bucket) and gives the answer, or what went wrong.
    /// `headers` are extra ones to sign, with lowercase names.
    fn call(
        &self,
        method: &str,
        key: &str,
        query: &[(&str, String)],
        headers: &[(&str, String)],
        body: &[u8],
    ) -> Res<web::Response> {
        self.call_body(method, key, query, headers, Body::Bytes(body))
    }

    /// As `call`, with the body in bytes or in part of a file on this computer.
    fn call_body(
        &self,
        method: &str,
        key: &str,
        query: &[(&str, String)],
        headers: &[(&str, String)],
        body: Body,
    ) -> Res<web::Response> {
        let uri = if key.is_empty() {
            format!("/{}", encode(&self.bucket))
        } else {
            format!("/{}/{}", encode(&self.bucket), encode_path(key))
        };
        let query = canonical_query(query);
        let hash = match body {
            Body::Bytes(b) => sha256_hex(b),
            Body::File(from, ref r) => file_sha256(from, r.clone())?,
        };
        let amz_date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let mut signed: Vec<(String, String)> = vec![
            ("host".into(), self.host.clone()),
            ("x-amz-content-sha256".into(), hash.clone()),
            ("x-amz-date".into(), amz_date.clone()),
        ];
        signed.extend(headers.iter().map(|(k, v)| (k.to_string(), v.clone())));
        let keys = Keys {
            access: &self.access,
            secret: &self.secret,
            region: &self.region,
        };
        let auth = authorization(method, &uri, &query, &signed, &hash, &keys, &amz_date);
        let url = if query.is_empty() {
            format!("{}{uri}", self.base)
        } else {
            format!("{}{uri}?{query}", self.base)
        };
        // The Host header is set here, exactly as signed: ureq would write the address's host in
        // lowercase, which a signature made with the host as typed wouldn't match.
        let mut headers: Vec<(&str, &str)> = signed
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        headers.push(("Authorization", &auth));
        let (mut bytes, mut file);
        let send: Option<(&mut dyn Read, u64)> = match body {
            _ if !matches!(method, "PUT" | "POST") => None,
            Body::Bytes(b) => {
                bytes = b;
                Some((&mut bytes, b.len() as u64))
            }
            Body::File(from, r) => {
                file = part_of(from, r.clone())?;
                Some((&mut file, r.end - r.start))
            }
        };
        web::run(&agent(), method, &url, &headers, send).map_err(|e| match e {
            web::Error::Status(status, res) => {
                let text = web::text(res).unwrap_or_default();
                Fail {
                    status,
                    msg: explain(status, &text, &self.bucket),
                }
            }
            web::Error::Transport(_) => Fail {
                status: 0,
                msg: format!(
                    "Can't reach {}. Check the address and the connection.",
                    self.base
                ),
            },
        })
    }

    fn text(&self, res: web::Response) -> Res<String> {
        web::text(res).map_err(|e| Fail {
            status: 0,
            msg: format!("Couldn't read the answer from S3: {e}"),
        })
    }

    /// One page of keys under `prefix`.
    fn list_page(
        &self,
        prefix: &str,
        delimiter: Option<&str>,
        max: usize,
        token: Option<&str>,
    ) -> Res<Page> {
        let mut q = vec![
            ("list-type", "2".to_string()),
            ("max-keys", max.to_string()),
            ("prefix", prefix.to_string()),
        ];
        if let Some(d) = delimiter {
            q.push(("delimiter", d.to_string()));
        }
        if let Some(t) = token {
            q.push(("continuation-token", t.to_string()));
        }
        let res = self.call("GET", "", &q, &[], &[])?;
        let xml = self.text(res)?;
        parse_list(&xml).map_err(|msg| Fail { status: 0, msg })
    }

    /// Every key and folder under `prefix`, following the continuation tokens; stops with an
    /// error past `limit` keys.
    fn list_all(&self, prefix: &str, delimiter: Option<&str>, limit: usize) -> Res<Page> {
        let mut all = Page::default();
        let mut token: Option<String> = None;
        loop {
            let page = self.list_page(prefix, delimiter, 1000, token.as_deref())?;
            all.files.extend(page.files);
            all.prefixes.extend(page.prefixes);
            if all.files.len() > limit {
                return Err(Fail {
                    status: 0,
                    msg: format!(
                        "This folder has more than {limit} files. Rename it in smaller parts."
                    ),
                });
            }
            match page.next {
                Some(t) => token = Some(t),
                None => return Ok(all),
            }
        }
    }

    fn head(&self, key: &str) -> Res<Option<Entry>> {
        match self.call("HEAD", key, &[], &[], &[]) {
            Ok(res) => {
                let mtime = web::header(&res, "Last-Modified")
                    .and_then(|d| chrono::DateTime::parse_from_rfc2822(d).ok())
                    .map_or(0, |d| d.timestamp());
                let size = web::header(&res, "Content-Length")
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
                Ok(Some(Entry {
                    name: basename(key).to_string(),
                    path: format!("/{key}"),
                    dir: false,
                    size,
                    mtime,
                    hidden: false,
                }))
            }
            Err(f) if f.status == 404 => Ok(None),
            Err(f) => Err(f),
        }
    }

    fn put(&self, key: &str, data: &[u8]) -> Res<String> {
        if data.len() as u64 > self.single_max {
            return self.put_in_parts(key, data);
        }
        // One PUT is atomic in S3: readers get the old object or the new one, never half of one.
        // So there's no temporary name to swap in.
        Ok(etag_of(&self.call("PUT", key, &[], &[], data)?))
    }

    /// Starts a multipart upload and gives its id.
    fn start_parts(&self, key: &str) -> Res<String> {
        let body = self.text(self.call("POST", key, &[("uploads", String::new())], &[], &[])?)?;
        let fields = leaves(&body).unwrap_or_default();
        match fields
            .into_iter()
            .find(|(p, _)| p == "InitiateMultipartUploadResult/UploadId")
        {
            Some((_, id)) if !id.is_empty() => Ok(id),
            _ => Err(Fail {
                status: 200,
                msg: explain(200, &body, &self.bucket),
            }),
        }
    }

    /// Puts the parts together (each is its number's ETag, in order). The version is the new ETag.
    fn finish_parts(&self, key: &str, id: &str, etags: &[String]) -> Res<String> {
        let mut xml = String::from("<CompleteMultipartUpload>");
        for (i, e) in etags.iter().enumerate() {
            xml.push_str(&format!(
                "<Part><PartNumber>{}</PartNumber><ETag>\"{e}\"</ETag></Part>",
                i + 1
            ));
        }
        xml.push_str("</CompleteMultipartUpload>");
        let res = self.call(
            "POST",
            key,
            &[("uploadId", id.to_string())],
            &[],
            xml.as_bytes(),
        )?;
        // Like a copy, this can answer 200 and say in the body that it failed.
        let body = self.text(res)?;
        if body.contains("<Error>") {
            return Err(Fail {
                status: 200,
                msg: explain(200, &body, &self.bucket),
            });
        }
        let fields = leaves(&body).unwrap_or_default();
        Ok(fields
            .into_iter()
            .find(|(p, _)| p == "CompleteMultipartUploadResult/ETag")
            .map(|(_, e)| e.trim_matches('"').to_string())
            .unwrap_or_default())
    }

    /// Gives up a multipart upload, so the parts sent so far aren't kept (and paid for).
    fn abort_parts(&self, key: &str, id: &str) {
        let _ = self.call("DELETE", key, &[("uploadId", id.to_string())], &[], &[]);
    }

    /// Runs `send` for each part of `size` bytes in a multipart upload of `key`; it gets the
    /// part's query, number and byte range and gives its ETag. Aborts the upload on an error.
    fn in_parts(
        &self,
        key: &str,
        size: u64,
        mut send: impl FnMut(&[(&str, String)], u64, std::ops::Range<u64>) -> Res<String>,
    ) -> Res<String> {
        let id = self.start_parts(key)?;
        let done = (|| {
            let mut etags = vec![];
            let mut start = 0;
            while start < size {
                let end = (start + self.part_size).min(size);
                let n = etags.len() as u64 + 1;
                let q = [("partNumber", n.to_string()), ("uploadId", id.clone())];
                etags.push(send(&q, n, start..end)?);
                start = end;
            }
            self.finish_parts(key, &id, &etags)
        })();
        if done.is_err() {
            self.abort_parts(key, &id);
        }
        done
    }

    /// Uploads `data` as a multipart upload.
    fn put_in_parts(&self, key: &str, data: &[u8]) -> Res<String> {
        self.in_parts(key, data.len() as u64, |q, _, r| {
            Ok(etag_of(&self.call(
                "PUT",
                key,
                q,
                &[],
                &data[r.start as usize..r.end as usize],
            )?))
        })
    }

    /// Uploads file `from`, in parts when it's big.
    fn put_from(&self, key: &str, from: &std::path::Path) -> Res<String> {
        let size = std::fs::metadata(from)
            .map_err(|e| Fail {
                status: 0,
                msg: format!("Can't read {}: {e}", from.display()),
            })?
            .len();
        if size > self.single_max {
            return self.in_parts(key, size, |q, _, r| {
                Ok(etag_of(&self.call_body(
                    "PUT",
                    key,
                    q,
                    &[],
                    Body::File(from, r),
                )?))
            });
        }
        Ok(etag_of(&self.call_body(
            "PUT",
            key,
            &[],
            &[],
            Body::File(from, 0..size),
        )?))
    }

    fn delete_key(&self, key: &str) -> Res<()> {
        self.call("DELETE", key, &[], &[], &[]).map(|_| ())
    }

    /// Copies `from` (`size` bytes) to `to`, in parts when it's too big for one CopyObject.
    fn copy(&self, from: &str, to: &str, size: u64) -> Res<()> {
        let source = format!("/{}/{}", encode(&self.bucket), encode_path(from));
        if size > self.single_max {
            return self.copy_in_parts(&source, to, size);
        }
        let res = self.call("PUT", to, &[], &[("x-amz-copy-source", source)], &[])?;
        // S3 can answer 200 and still say in the body that the copy failed.
        let body = self.text(res)?;
        if body.contains("<Error>") {
            return Err(Fail {
                status: 200,
                msg: explain(200, &body, &self.bucket),
            });
        }
        Ok(())
    }

    fn copy_in_parts(&self, source: &str, to: &str, size: u64) -> Res<()> {
        self.in_parts(to, size, |q, _, r| {
            let headers = [
                ("x-amz-copy-source", source.to_string()),
                (
                    "x-amz-copy-source-range",
                    format!("bytes={}-{}", r.start, r.end - 1),
                ),
            ];
            let body = self.text(self.call("PUT", to, q, &headers, &[])?)?;
            let fields = leaves(&body).unwrap_or_default();
            match fields.into_iter().find(|(p, _)| p == "CopyPartResult/ETag") {
                Some((_, e)) => Ok(e.trim_matches('"').to_string()),
                None => Err(Fail {
                    status: 200,
                    msg: explain(200, &body, &self.bucket),
                }),
            }
        })
        .map(|_| ())
    }

    fn is_dir(&self, key: &str) -> Res<bool> {
        let page = self.list_page(&format!("{key}/"), None, 1, None)?;
        Ok(!page.files.is_empty() || !page.prefixes.is_empty())
    }
}

/// The key for an app path ("/a/b.txt" is "a/b.txt"; "/" is "").
fn key_of(path: &str) -> &str {
    path.trim_matches('/')
}

impl Transport for S3 {
    fn kind(&self) -> &'static str {
        "s3"
    }

    fn label(&self) -> String {
        self.bucket.clone()
    }

    fn default_dir(&self) -> String {
        "/".into()
    }

    fn quick_dirs(&self) -> Vec<(String, String)> {
        vec![]
    }

    fn listdir(&self, path: &str) -> Result<Vec<Entry>> {
        let key = key_of(path);
        let prefix = if key.is_empty() {
            String::new()
        } else {
            format!("{key}/")
        };
        let all = self.list_all(&prefix, Some("/"), usize::MAX)?;
        let mut out = vec![];
        for p in all.prefixes {
            let name = p[prefix.len()..].trim_end_matches('/').to_string();
            out.push(Entry {
                path: format!("/{prefix}{name}"),
                name,
                dir: true,
                size: 0,
                mtime: 0,
                hidden: false,
            });
        }
        for o in all.files {
            // The folder's own marker.
            if o.key == prefix {
                continue;
            }
            let name = o.key[prefix.len()..].to_string();
            out.push(Entry {
                path: format!("/{}", o.key),
                name,
                dir: false,
                size: o.size,
                mtime: o.mtime,
                hidden: false,
            });
        }
        Ok(out)
    }

    fn stat(&self, path: &str) -> Result<Option<Entry>> {
        let key = key_of(path);
        if key.is_empty() {
            return Ok(Some(Entry {
                name: String::new(),
                path: "/".into(),
                dir: true,
                size: 0,
                mtime: 0,
                hidden: false,
            }));
        }
        if let Some(e) = self.head(key)? {
            return Ok(Some(e));
        }
        Ok(self.is_dir(key)?.then(|| Entry {
            name: basename(key).to_string(),
            path: format!("/{key}"),
            dir: true,
            size: 0,
            mtime: 0,
            hidden: false,
        }))
    }

    fn read(&self, path: &str) -> Result<(Vec<u8>, String)> {
        let res = self.call("GET", key_of(path), &[], &[], &[])?;
        let version = etag_of(&res);
        let mut data = vec![];
        web::reader(res)
            .read_to_end(&mut data)
            .map_err(|e| DevError::Msg(format!("Couldn't read {path} from S3: {e}")))?;
        Ok((data, version))
    }

    fn read_range(&self, path: &str, start: u64, len: u64) -> Result<Vec<u8>> {
        if len == 0 {
            return Ok(vec![]);
        }
        let range = format!("bytes={start}-{}", start + len - 1);
        let res = match self.call("GET", key_of(path), &[], &[("range", range)], &[]) {
            Ok(r) => r,
            // Starting past the end of the file.
            Err(f) if f.status == 416 => return Ok(vec![]),
            Err(f) => return Err(f.into()),
        };
        // A server that ignores the range sends the whole file: skip to `start`.
        let skip = if res.status() == 206 { 0 } else { start };
        let mut reader = web::reader(res);
        std::io::copy(&mut (&mut reader).take(skip), &mut std::io::sink())
            .map_err(|e| DevError::Msg(e.to_string()))?;
        let mut data = vec![];
        reader
            .take(len)
            .read_to_end(&mut data)
            .map_err(|e| DevError::Msg(format!("Couldn't read {path} from S3: {e}")))?;
        Ok(data)
    }

    fn version(&self, path: &str) -> Result<Option<String>> {
        match self.call("HEAD", key_of(path), &[], &[], &[]) {
            Ok(res) => Ok(Some(etag_of(&res))),
            Err(f) if f.status == 404 => Ok(None),
            Err(f) => Err(f.into()),
        }
    }

    fn replace(&self, path: &str, data: &[u8], _exists: bool) -> Result<String> {
        let key = key_of(path);
        if key.is_empty() {
            return err("Can't write a file in place of the bucket.");
        }
        Ok(self.put(key, data)?)
    }

    fn replace_from(&self, path: &str, from: &std::path::Path, _exists: bool) -> Result<String> {
        let key = key_of(path);
        if key.is_empty() {
            return err("Can't write a file in place of the bucket.");
        }
        Ok(self.put_from(key, from)?)
    }

    fn read_to(
        &self,
        path: &str,
        to: &std::path::Path,
        read: &mut dyn FnMut(u64),
        stop: &std::sync::atomic::AtomicBool,
    ) -> Result<String> {
        let res = self.call("GET", key_of(path), &[], &[], &[])?;
        let version = etag_of(&res);
        let size = web::header(&res, "Content-Length").and_then(|n| n.parse::<u64>().ok());
        let got = crate::device::pour(
            &mut web::reader(res),
            to,
            &format!("{path} from S3"),
            read,
            stop,
        )?;
        if size.is_some_and(|n| n != got) {
            return err(format!(
                "{path}: read {got} bytes of {}.",
                size.unwrap_or(0)
            ));
        }
        Ok(version)
    }

    fn delete(&self, path: &str, is_dir: bool) -> Result<()> {
        let key = key_of(path);
        if key.is_empty() {
            return err("Can't delete the whole bucket.");
        }
        if is_dir {
            // One by one: DeleteObjects needs a Content-MD5 and this is simpler. The folder's
            // marker is one of the keys under the prefix.
            let all = self.list_all(&format!("{key}/"), None, usize::MAX)?;
            for o in all.files {
                self.delete_key(&o.key)?;
            }
        }
        Ok(self.delete_key(key)?)
    }

    fn make_dir(&self, path: &str) -> Result<()> {
        let key = key_of(path);
        if key.is_empty() {
            return err("The bucket's top is already a folder.");
        }
        self.put(&format!("{key}/"), &[])?;
        Ok(())
    }

    fn rename(&self, path: &str, new_name: &str) -> Result<()> {
        let from = key_of(path);
        let parent = key_of(&dirname(path)).to_string();
        let to = if parent.is_empty() {
            new_name.to_string()
        } else {
            format!("{parent}/{new_name}")
        };
        let Some(entry) = self.stat(path)? else {
            return err(format!("{path} doesn't exist."));
        };
        if !entry.dir {
            self.copy(from, &to, entry.size)?;
            return Ok(self.delete_key(from)?);
        }
        let (from_prefix, to_prefix) = (format!("{from}/"), format!("{to}/"));
        let objects = self.list_all(&from_prefix, None, RENAME_MAX)?.files;
        for o in &objects {
            self.copy(
                &o.key,
                &format!("{to_prefix}{}", &o.key[from_prefix.len()..]),
                o.size,
            )?;
        }
        for o in &objects {
            self.delete_key(&o.key)?;
        }
        Ok(())
    }
}

/// Connects the S3 bucket as a drive.
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn s3_connect(
    state: State<'_, AppState>,
    endpoint: String,
    region: String,
    bucket: String,
    access_key: String,
    secret: Option<String>,
) -> std::result::Result<connect::Info, String> {
    let s3 = blocking(move || {
        let secret = secret
            .as_deref()
            .and_then(crate::servers::secrets::get)
            .unwrap_or_default();
        S3::open(&endpoint, &region, &bucket, &access_key, &secret)
    })
    .await?;
    let usb = Some(format!("s3:{}/{}", s3.base, s3.bucket));
    if state.devs().values().any(|c| c.usb == usb) {
        return Err(format!(
            "The bucket {} at {} is already added.",
            s3.bucket, s3.base
        ));
    }
    let id = format!("d{}", state.next_dev.fetch_add(1, Ordering::Relaxed));
    let conn = Conn {
        dev: Arc::new(Device::new(Box::new(s3))),
        usb,
        serial: None,
    };
    let info = connect::info_of(&id, Some(&conn), &conn.dev);
    state.devs().insert(id, conn);
    Ok(info)
}

#[cfg(test)]
mod tests;
