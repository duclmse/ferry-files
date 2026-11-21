//! S3 buckets (AWS, Cloudflare R2, Backblaze B2, MinIO…) as drives. Requests are signed with
//! AWS Signature Version 4 and use path-style addresses (`endpoint/bucket/key`).
//!
//! Paths are the app's: "/" is the bucket's top, "/a/b.txt" is the key "a/b.txt". S3 has no
//! folders: a folder is a prefix, and one made empty is kept as a zero-byte key "a/b/".

use super::*;
use crate::device::{basename, dirname, err, Result, Transport};
use hmac::{Hmac, Mac};
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
const UNRESERVED: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.').remove(b'~');
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

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    let mut m = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes keys of any size");
    m.update(data.as_bytes());
    m.finalize().into_bytes().to_vec()
}

/// The sorted, encoded query string ("a=1&b=2"), the same for the address and the signature.
fn canonical_query(params: &[(&str, String)]) -> String {
    let mut pairs: Vec<(String, String)> = params.iter().map(|(k, v)| (encode(k), encode(v))).collect();
    pairs.sort();
    pairs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")
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
fn authorization(method: &str, uri: &str, query: &str, headers: &[(String, String)], payload_hash: &str, keys: &Keys, amz_date: &str) -> String {
    let mut sorted: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (k.as_str(), v.trim())).collect();
    sorted.sort();
    let canon_headers: String = sorted.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed = sorted.iter().map(|(k, _)| *k).collect::<Vec<_>>().join(";");
    let request = format!("{method}\n{uri}\n{query}\n{canon_headers}\n{signed}\n{payload_hash}");
    let day = &amz_date[..8];
    let scope = format!("{day}/{}/s3/aws4_request", keys.region);
    let to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", sha256_hex(request.as_bytes()));
    let k = hmac(format!("AWS4{}", keys.secret).as_bytes(), day);
    let k = hmac(&k, keys.region);
    let k = hmac(&k, "s3");
    let k = hmac(&k, "aws4_request");
    let signature = hex::encode(hmac(&k, &to_sign));
    format!("AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}", keys.access)
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
    use crate::xml::{tokenize, Kind};
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
                    let text = if t.kind == Kind::CData { raw[9..raw.len() - 3].to_string() } else { crate::xml::unescape(raw) };
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
            "ListBucketResult/Contents/Key" => page.files.push(Obj { key: text, ..Obj::default() }),
            "ListBucketResult/Contents/Size" => {
                if let Some(o) = page.files.last_mut() {
                    o.size = text.trim().parse().unwrap_or(0);
                }
            }
            "ListBucketResult/Contents/LastModified" => {
                if let Some(o) = page.files.last_mut() {
                    o.mtime = chrono::DateTime::parse_from_rfc3339(text.trim()).map_or(0, |d| d.timestamp());
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
    let field = |name: &str| fields.iter().find(|(p, _)| p == name).map(|(_, v)| v.as_str()).unwrap_or("");
    let (code, message) = (field("Error/Code"), field("Error/Message"));
    match code {
        "InvalidAccessKeyId" | "SignatureDoesNotMatch" => "The access key or the secret key is wrong. Check them in Settings > Connections.".into(),
        "NoSuchBucket" => format!("The bucket {bucket} doesn't exist at this address."),
        "AccessDenied" => "Access denied. This key may not be allowed to do that in the bucket.".into(),
        "NoSuchKey" => "That file doesn't exist.".into(),
        "RequestTimeTooSkewed" => "This computer's clock is too far off for S3. Set the time and try again.".into(),
        "AuthorizationHeaderMalformed" | "PermanentRedirect" | "IllegalLocationConstraintException" => {
            let hint = message.split_once("expecting '").and_then(|(_, r)| r.split_once('\'')).map(|(r, _)| r);
            match hint {
                Some(r) => format!("The region is wrong. This bucket is in {r}."),
                None => "The region is wrong for this bucket. Check it in Settings > Connections.".into(),
            }
        }
        "" => match status {
            301 | 307 => "The region or the address is wrong for this bucket.".into(),
            403 => "Access denied. The keys may be wrong, or not allowed to do that in the bucket.".into(),
            404 => "Not found.".into(),
            _ => format!("S3 answered with error {status}."),
        },
        _ if !message.is_empty() => format!("S3 says: {message}"),
        _ => format!("S3 answered with error {status} ({code})."),
    }
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(60))
        .timeout_write(Duration::from_secs(60))
        .redirects(0)
        .build()
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
    if base.starts_with("https://") { host.strip_suffix(":443").unwrap_or(host) } else { host.strip_suffix(":80").unwrap_or(host) }.to_string()
}

/// An answer's ETag without its quotes.
fn etag_of(res: &ureq::Response) -> String {
    res.header("ETag").unwrap_or("").trim_matches('"').to_string()
}

impl S3 {
    /// Connects to `bucket`: the keys must be allowed to list it.
    pub fn open(endpoint: &str, region: &str, bucket: &str, access: &str, secret: &str) -> std::result::Result<Self, String> {
        let region = if region.trim().is_empty() { "us-east-1" } else { region.trim() };
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
            return Err("Use an address like https://s3.us-east-1.amazonaws.com, without a folder.".into());
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
    fn call(&self, method: &str, key: &str, query: &[(&str, String)], headers: &[(&str, String)], body: &[u8]) -> Res<ureq::Response> {
        let uri = if key.is_empty() { format!("/{}", encode(&self.bucket)) } else { format!("/{}/{}", encode(&self.bucket), encode_path(key)) };
        let query = canonical_query(query);
        let hash = sha256_hex(body);
        let amz_date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let mut signed: Vec<(String, String)> = vec![
            ("host".into(), self.host.clone()),
            ("x-amz-content-sha256".into(), hash.clone()),
            ("x-amz-date".into(), amz_date.clone()),
        ];
        signed.extend(headers.iter().map(|(k, v)| (k.to_string(), v.clone())));
        let keys = Keys { access: &self.access, secret: &self.secret, region: &self.region };
        let auth = authorization(method, &uri, &query, &signed, &hash, &keys, &amz_date);
        let url = if query.is_empty() { format!("{}{uri}", self.base) } else { format!("{}{uri}?{query}", self.base) };
        let mut req = agent().request(method, &url);
        // The Host header is set here, exactly as signed: ureq would write the address's host in
        // lowercase, which a signature made with the host as typed wouldn't match.
        for (k, v) in &signed {
            req = req.set(k, v);
        }
        req = req.set("Authorization", &auth);
        let sent = if matches!(method, "PUT" | "POST") { req.send_bytes(body) } else { req.call() };
        sent.map_err(|e| match e {
            ureq::Error::Status(status, res) => {
                let text = res.into_string().unwrap_or_default();
                Fail { status, msg: explain(status, &text, &self.bucket) }
            }
            ureq::Error::Transport(_) => Fail { status: 0, msg: format!("Can't reach {}. Check the address and the connection.", self.base) },
        })
    }

    fn text(&self, res: ureq::Response) -> Res<String> {
        res.into_string().map_err(|e| Fail { status: 0, msg: format!("Couldn't read the answer from S3: {e}") })
    }

    /// One page of keys under `prefix`.
    fn list_page(&self, prefix: &str, delimiter: Option<&str>, max: usize, token: Option<&str>) -> Res<Page> {
        let mut q = vec![("list-type", "2".to_string()), ("max-keys", max.to_string()), ("prefix", prefix.to_string())];
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
                return Err(Fail { status: 0, msg: format!("This folder has more than {limit} files. Rename it in smaller parts.") });
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
                let mtime = res.header("Last-Modified").and_then(|d| chrono::DateTime::parse_from_rfc2822(d).ok()).map_or(0, |d| d.timestamp());
                let size = res.header("Content-Length").and_then(|n| n.parse().ok()).unwrap_or(0);
                Ok(Some(Entry { name: basename(key).to_string(), path: format!("/{key}"), dir: false, size, mtime, hidden: false }))
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
        match fields.into_iter().find(|(p, _)| p == "InitiateMultipartUploadResult/UploadId") {
            Some((_, id)) if !id.is_empty() => Ok(id),
            _ => Err(Fail { status: 200, msg: explain(200, &body, &self.bucket) }),
        }
    }

    /// Puts the parts together (each is its number's ETag, in order). The version is the new ETag.
    fn finish_parts(&self, key: &str, id: &str, etags: &[String]) -> Res<String> {
        let mut xml = String::from("<CompleteMultipartUpload>");
        for (i, e) in etags.iter().enumerate() {
            xml.push_str(&format!("<Part><PartNumber>{}</PartNumber><ETag>\"{e}\"</ETag></Part>", i + 1));
        }
        xml.push_str("</CompleteMultipartUpload>");
        let res = self.call("POST", key, &[("uploadId", id.to_string())], &[], xml.as_bytes())?;
        // Like a copy, this can answer 200 and say in the body that it failed.
        let body = self.text(res)?;
        if body.contains("<Error>") {
            return Err(Fail { status: 200, msg: explain(200, &body, &self.bucket) });
        }
        let fields = leaves(&body).unwrap_or_default();
        Ok(fields.into_iter().find(|(p, _)| p == "CompleteMultipartUploadResult/ETag").map(|(_, e)| e.trim_matches('"').to_string()).unwrap_or_default())
    }

    /// Gives up a multipart upload, so the parts sent so far aren't kept (and paid for).
    fn abort_parts(&self, key: &str, id: &str) {
        let _ = self.call("DELETE", key, &[("uploadId", id.to_string())], &[], &[]);
    }

    /// Runs `send` for each part of `size` bytes in a multipart upload of `key`; it gets the
    /// part's query, number and byte range and gives its ETag. Aborts the upload on an error.
    fn in_parts(&self, key: &str, size: u64, mut send: impl FnMut(&[(&str, String)], u64, std::ops::Range<u64>) -> Res<String>) -> Res<String> {
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
        self.in_parts(key, data.len() as u64, |q, _, r| Ok(etag_of(&self.call("PUT", key, q, &[], &data[r.start as usize..r.end as usize])?)))
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
            return Err(Fail { status: 200, msg: explain(200, &body, &self.bucket) });
        }
        Ok(())
    }

    fn copy_in_parts(&self, source: &str, to: &str, size: u64) -> Res<()> {
        self.in_parts(to, size, |q, _, r| {
            let headers = [("x-amz-copy-source", source.to_string()), ("x-amz-copy-source-range", format!("bytes={}-{}", r.start, r.end - 1))];
            let body = self.text(self.call("PUT", to, q, &headers, &[])?)?;
            let fields = leaves(&body).unwrap_or_default();
            match fields.into_iter().find(|(p, _)| p == "CopyPartResult/ETag") {
                Some((_, e)) => Ok(e.trim_matches('"').to_string()),
                None => Err(Fail { status: 200, msg: explain(200, &body, &self.bucket) }),
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
        let prefix = if key.is_empty() { String::new() } else { format!("{key}/") };
        let all = self.list_all(&prefix, Some("/"), usize::MAX)?;
        let mut out = vec![];
        for p in all.prefixes {
            let name = p[prefix.len()..].trim_end_matches('/').to_string();
            out.push(Entry { path: format!("/{prefix}{name}"), name, dir: true, size: 0, mtime: 0, hidden: false });
        }
        for o in all.files {
            // The folder's own marker.
            if o.key == prefix {
                continue;
            }
            let name = o.key[prefix.len()..].to_string();
            out.push(Entry { path: format!("/{}", o.key), name, dir: false, size: o.size, mtime: o.mtime, hidden: false });
        }
        Ok(out)
    }

    fn stat(&self, path: &str) -> Result<Option<Entry>> {
        let key = key_of(path);
        if key.is_empty() {
            return Ok(Some(Entry { name: String::new(), path: "/".into(), dir: true, size: 0, mtime: 0, hidden: false }));
        }
        if let Some(e) = self.head(key)? {
            return Ok(Some(e));
        }
        Ok(self.is_dir(key)?.then(|| Entry { name: basename(key).to_string(), path: format!("/{key}"), dir: true, size: 0, mtime: 0, hidden: false }))
    }

    fn read(&self, path: &str) -> Result<(Vec<u8>, String)> {
        let res = self.call("GET", key_of(path), &[], &[], &[])?;
        let version = etag_of(&res);
        let mut data = vec![];
        res.into_reader().read_to_end(&mut data).map_err(|e| DevError::Msg(format!("Couldn't read {path} from S3: {e}")))?;
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
        let mut reader = res.into_reader();
        std::io::copy(&mut (&mut reader).take(skip), &mut std::io::sink()).map_err(|e| DevError::Msg(e.to_string()))?;
        let mut data = vec![];
        reader.take(len).read_to_end(&mut data).map_err(|e| DevError::Msg(format!("Couldn't read {path} from S3: {e}")))?;
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
        let to = if parent.is_empty() { new_name.to_string() } else { format!("{parent}/{new_name}") };
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
            self.copy(&o.key, &format!("{to_prefix}{}", &o.key[from_prefix.len()..]), o.size)?;
        }
        for o in &objects {
            self.delete_key(&o.key)?;
        }
        Ok(())
    }
}

/// Connects the S3 bucket as a drive.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn s3_connect(
    state: State<'_, AppState>,
    endpoint: String,
    region: String,
    bucket: String,
    access_key: String,
    secret: Option<String>,
) -> std::result::Result<connect::Info, String> {
    let s3 = blocking(move || {
        let secret = secret.as_deref().and_then(crate::secrets::get).unwrap_or_default();
        S3::open(&endpoint, &region, &bucket, &access_key, &secret)
    })
    .await?;
    let usb = Some(format!("s3:{}/{}", s3.base, s3.bucket));
    if state.devs().values().any(|c| c.usb == usb) {
        return Err(format!("The bucket {} at {} is already added.", s3.bucket, s3.base));
    }
    let id = format!("d{}", state.next_dev.fetch_add(1, Ordering::Relaxed));
    let conn = Conn { dev: Arc::new(Device::new(Box::new(s3))), usb, serial: None };
    let info = connect::info_of(&id, Some(&conn), &conn.dev);
    state.devs().insert(id, conn);
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// The GET Object example from AWS's Signature Version 4 documentation.
    #[test]
    fn signs_like_the_aws_example() {
        let hash = sha256_hex(b"");
        assert_eq!(hash, "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        let headers: Vec<(String, String)> = vec![
            ("host".into(), "examplebucket.s3.amazonaws.com".into()),
            ("range".into(), "bytes=0-9".into()),
            ("x-amz-content-sha256".into(), hash.clone()),
            ("x-amz-date".into(), "20130524T000000Z".into()),
        ];
        let keys = Keys { access: "AKIAIOSFODNN7EXAMPLE", secret: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY", region: "us-east-1" };
        let auth = authorization("GET", "/test.txt", "", &headers, &hash, &keys, "20130524T000000Z");
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
        let q = canonical_query(&[("prefix", "a b/".into()), ("list-type", "2".into()), ("delimiter", "/".into())]);
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
        assert_eq!(p.files[1], Obj { key: "a/x & y.txt".into(), size: 12, mtime: 1_704_164_645, etag: "abc".into() });
        let last = parse_list("<ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>").unwrap();
        assert_eq!(last, Page::default());
        assert!(parse_list("<html/>").is_err());
    }

    #[test]
    fn explains_errors() {
        let e = |code: &str, msg: &str| format!("<Error><Code>{code}</Code><Message>{msg}</Message></Error>");
        assert!(explain(403, &e("InvalidAccessKeyId", "x"), "b").contains("access key"));
        assert!(explain(403, &e("SignatureDoesNotMatch", "x"), "b").contains("secret key"));
        assert!(explain(404, &e("NoSuchBucket", "x"), "photos").contains("photos doesn't exist"));
        assert!(explain(403, &e("AccessDenied", "x"), "b").starts_with("Access denied"));
        let region = explain(400, &e("AuthorizationHeaderMalformed", "the region 'us-east-1' is wrong; expecting 'eu-west-1'"), "b");
        assert_eq!(region, "The region is wrong. This bucket is in eu-west-1.");
        assert!(explain(403, "", "b").starts_with("Access denied"));
        assert_eq!(explain(500, &e("InternalError", "Try again"), "b"), "S3 says: Try again");
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
    fn serve(answers: Vec<(&'static str, &'static str, &'static str)>) -> (std::thread::JoinHandle<Vec<Req>>, String) {
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
                    .find_map(|l| l.to_lowercase().strip_prefix("content-length:").map(|n| n.trim().parse::<usize>().unwrap_or(0)))
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
                reqs.push(Req { head, body: buf[end..].to_vec() });
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
        let shown: Vec<(&str, &str, bool, u64)> = items.iter().map(|e| (e.name.as_str(), e.path.as_str(), e.dir, e.size)).collect();
        assert_eq!(shown, [("sub", "/a/sub", true, 0), ("one.txt", "/a/one.txt", false, 3)]);
        assert_eq!(items[1].mtime, 1_704_164_645);
        let f = s3.stat("/a/one.txt").unwrap().unwrap();
        assert_eq!((f.dir, f.size, f.mtime), (false, 7, 1_704_164_645));
        assert!(s3.stat("/a/sub").unwrap().unwrap().dir);
        assert!(s3.stat("/nope").unwrap().is_none());

        let reqs = asked.join().unwrap();
        assert!(reqs[0].first().starts_with("GET /bkt?list-type=2&max-keys=1&prefix= "), "{}", reqs[0].first());
        assert!(reqs[1].first().contains("delimiter=%2F&list-type=2&max-keys=1000&prefix=a%2F"), "{}", reqs[1].first());
        assert!(reqs[2].first().contains("continuation-token=t1"), "{}", reqs[2].first());
        assert!(reqs[3].first().starts_with("HEAD /bkt/a/one.txt "), "{}", reqs[3].first());
        assert!(reqs[5].first().contains("max-keys=1&prefix=a%2Fsub%2F"), "{}", reqs[5].first());
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
            ("416 Range Not Satisfiable", "", "<Error><Code>InvalidRange</Code></Error>"),
        ]);
        let s3 = S3::open(&at, "auto", "bkt", "AK", "SK").unwrap();
        assert_eq!(s3.read_range("/dir/a b.txt", 1, 3).unwrap(), b"ell");
        // The server ignored the range.
        assert_eq!(s3.read_range("/x", 1, 3).unwrap(), b"ell");
        assert_eq!(s3.read("/x").unwrap(), (b"hello".to_vec(), "v1".to_string()));
        assert_eq!(s3.replace("/x", b"new text", true).unwrap(), "v2");
        assert!(s3.read_range("/x", 99, 3).unwrap().is_empty());

        let reqs = asked.join().unwrap();
        assert!(reqs[1].first().starts_with("GET /bkt/dir/a%20b.txt "), "{}", reqs[1].first());
        assert!(reqs[1].has("range: bytes=1-3"));
        assert!(reqs[1].has("SignedHeaders=host;range;x-amz-content-sha256;x-amz-date,"));
        assert!(reqs[1].has("/auto/s3/aws4_request"));
        assert!(reqs[4].first().starts_with("PUT /bkt/x "), "{}", reqs[4].first());
        assert_eq!(reqs[4].body, b"new text");
        assert!(reqs[4].has(&format!("x-amz-content-sha256: {}", sha256_hex(b"new text"))));
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
            ("200 OK", "", "<Error><Code>InternalError</Code><Message>Copy broke</Message></Error>"),
        ]);
        let s3 = S3::open(&at, "", "bkt", "AK", "SK").unwrap();
        s3.rename("/a/old name.txt", "new.txt").unwrap();
        let e = s3.rename("/a/new.txt", "b.txt").unwrap_err();
        assert!(e.to_string().contains("Copy broke"), "{e}");
        let reqs = asked.join().unwrap();
        assert!(reqs[2].first().starts_with("PUT /bkt/a/new.txt "), "{}", reqs[2].first());
        assert!(reqs[2].has("x-amz-copy-source: /bkt/a/old%20name.txt"));
        assert!(reqs[2].has("SignedHeaders=host;x-amz-content-sha256;x-amz-copy-source;x-amz-date,"));
        assert!(reqs[3].first().starts_with("DELETE /bkt/a/old%20name.txt "), "{}", reqs[3].first());
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
        assert!(reqs[4].first().starts_with("PUT /bkt/e/ "), "{}", reqs[4].first());
        assert!(reqs[5].first().starts_with("PUT /bkt/e/f "), "{}", reqs[5].first());
        assert!(reqs[6].first().starts_with("DELETE /bkt/d/ "), "{}", reqs[6].first());
        assert!(reqs[11].first().starts_with("DELETE /bkt/d "), "{}", reqs[11].first());
    }

    #[test]
    fn makes_a_folder_marker() {
        let (asked, at) = serve(vec![("200 OK", "", EMPTY_LIST), ("200 OK", "ETag: \"d41d8cd9\"\r\n", "")]);
        let s3 = S3::open(&at, "", "bkt", "AK", "SK").unwrap();
        s3.make_dir("/a/b").unwrap();
        let reqs = asked.join().unwrap();
        assert!(reqs[1].first().starts_with("PUT /bkt/a/b/ "), "{}", reqs[1].first());
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
        assert_eq!(s3.replace("/d/big.bin", b"0123456789", true).unwrap(), "whole-3");
        assert_eq!(s3.replace("/small", b"abc", false).unwrap(), "one");
        let reqs = asked.join().unwrap();
        assert!(reqs[1].first().starts_with("POST /bkt/d/big.bin?uploads= "), "{}", reqs[1].first());
        let (id, lens) = ("uploadId=up%2B1%2Fx", [4, 4, 2]);
        for n in 0..3 {
            assert!(reqs[2 + n].first().starts_with(&format!("PUT /bkt/d/big.bin?partNumber={}&{id} ", n + 1)), "{}", reqs[2 + n].first());
            assert_eq!(reqs[2 + n].body, b"0123456789"[n * 4..n * 4 + lens[n]]);
        }
        assert!(reqs[5].first().starts_with(&format!("POST /bkt/d/big.bin?{id} ")), "{}", reqs[5].first());
        let xml = String::from_utf8_lossy(&reqs[5].body).to_string();
        assert_eq!(
            xml,
            "<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>\"p1\"</ETag></Part><Part><PartNumber>2</PartNumber><ETag>\"p2\"</ETag></Part>\
             <Part><PartNumber>3</PartNumber><ETag>\"p3\"</ETag></Part></CompleteMultipartUpload>"
        );
        assert!(reqs[6].first().starts_with("PUT /bkt/small "), "{}", reqs[6].first());
    }

    #[test]
    fn gives_up_a_failed_multipart_upload() {
        let (asked, at) = serve(vec![
            ("200 OK", "", EMPTY_LIST),
            ("200 OK", "", STARTED),
            ("200 OK", "ETag: \"p1\"\r\n", ""),
            ("500 Internal Server Error", "", "<Error><Code>InternalError</Code><Message>Part broke</Message></Error>"),
            ("204 No Content", "", ""),
        ]);
        let s3 = small_parts(&at);
        let e = s3.replace("/big.bin", b"0123456789", true).unwrap_err();
        assert!(e.to_string().contains("Part broke"), "{e}");
        let reqs = asked.join().unwrap();
        assert!(reqs[4].first().starts_with("DELETE /bkt/big.bin?uploadId=up%2B1%2Fx "), "{}", reqs[4].first());
    }

    #[test]
    fn copies_big_objects_in_parts() {
        let part = |e: &'static str| -> (&'static str, &'static str, &'static str) {
            ("200 OK", "", Box::leak(format!("<CopyPartResult><ETag>&quot;{e}&quot;</ETag></CopyPartResult>").into_boxed_str()))
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
        assert!(reqs[2].first().starts_with("POST /bkt/a/new.bin?uploads= "), "{}", reqs[2].first());
        for (n, range) in ["bytes=0-3", "bytes=4-7", "bytes=8-9"].iter().enumerate() {
            let r = &reqs[3 + n];
            assert!(r.first().starts_with(&format!("PUT /bkt/a/new.bin?partNumber={}&uploadId=up%2B1%2Fx ", n + 1)), "{}", r.first());
            assert!(r.has(&format!("x-amz-copy-source-range: {range}")) && r.has("x-amz-copy-source: /bkt/a/old.bin"), "{}", r.head);
            assert!(r.has("SignedHeaders=host;x-amz-content-sha256;x-amz-copy-source;x-amz-copy-source-range;x-amz-date,"), "{}", r.head);
        }
        assert!(String::from_utf8_lossy(&reqs[6].body).contains("<ETag>\"c3\"</ETag>"));
        // The old one goes only after the copy is complete.
        assert!(reqs[7].first().starts_with("DELETE /bkt/a/old.bin "), "{}", reqs[7].first());
    }

    #[test]
    fn host_header_has_the_port_only_when_its_not_the_default() {
        assert_eq!(host_header("https://s3.amazonaws.com"), "s3.amazonaws.com");
        assert_eq!(host_header("https://s3.amazonaws.com:443"), "s3.amazonaws.com");
        assert_eq!(host_header("https://minio.lan:9000"), "minio.lan:9000");
        assert_eq!(host_header("https://minio.lan:1443"), "minio.lan:1443");
        assert_eq!(host_header("http://nas:80"), "nas");
        assert_eq!(host_header("http://nas:9000"), "nas:9000");
        assert_eq!(host_header("http://nas:443"), "nas:443");
    }

    /// Signs the request again from what the server received, to see that the host it was signed
    /// with is the Host header that arrived.
    fn signed_host_matches(r: &Req, want_host: &str) {
        let header = |name: &str| r.head.lines().find_map(|l| l.split_once(": ").filter(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v)).unwrap().to_string();
        let host = header("Host");
        assert_eq!(host, want_host);
        let auth = header("Authorization");
        assert!(auth.contains("SignedHeaders=host;"), "{auth}");
        let (target, _) = r.first().strip_prefix("GET ").unwrap().rsplit_once(" HTTP").unwrap();
        let (uri, query) = target.split_once('?').unwrap_or((target, ""));
        let (date, hash) = (header("x-amz-date"), header("x-amz-content-sha256"));
        let headers: Vec<(String, String)> = vec![("host".into(), host), ("x-amz-content-sha256".into(), hash.clone()), ("x-amz-date".into(), date.clone())];
        let keys = Keys { access: "AK", secret: "SK", region: "us-east-1" };
        assert_eq!(authorization("GET", uri, query, &headers, &hash, &keys, &date), auth);
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
        assert!(S3::open(&at, "", "bkt", "AK", "SK").err().unwrap().contains("access key"));
        asked.join().unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        assert!(S3::open(&format!("127.0.0.1:{port}"), "", "bkt", "AK", "SK").err().unwrap().starts_with("Can't reach"));
        assert!(S3::open("", "", "", "AK", "SK").err().unwrap().contains("bucket"));
        assert!(S3::open("", "", "b", "AK", "").err().unwrap().contains("secret key"));
        assert!(S3::open("https://host/folder", "", "b", "AK", "SK").is_err());
    }
}
