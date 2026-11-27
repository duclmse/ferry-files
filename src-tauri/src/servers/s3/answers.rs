//! Reading S3's XML answers: a page of a listing, and what an error answer means.

/// The text of each element that has some, with its path ("ListBucketResult/Contents/Key").
pub(super) fn leaves(xml: &str) -> std::result::Result<Vec<(String, String)>, String> {
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
pub(super) struct Obj {
    pub(super) key: String,
    pub(super) size: u64,
    pub(super) mtime: i64,
    pub(super) etag: String,
}

/// One page of a ListObjectsV2 answer.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Page {
    pub(super) files: Vec<Obj>,
    /// Folders ("a/b/").
    pub(super) prefixes: Vec<String>,
    pub(super) next: Option<String>,
}

pub(super) fn parse_list(xml: &str) -> std::result::Result<Page, String> {
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
pub(super) fn explain(status: u16, body: &str, bucket: &str) -> String {
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
