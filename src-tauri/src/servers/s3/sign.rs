//! Signing S3 requests with AWS Signature Version 4, and the encoding it signs.

use super::*;
use hmac::{Hmac, KeyInit, Mac};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};

/// What stays as it is in an encoded name: letters, digits and - _ . ~
pub(super) const UNRESERVED: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');
/// The same, and "/", for a path.
pub(super) const PATH: &AsciiSet = &UNRESERVED.remove(b'/');

pub(super) fn encode(s: &str) -> String {
    utf8_percent_encode(s, UNRESERVED).to_string()
}

pub(super) fn encode_path(s: &str) -> String {
    utf8_percent_encode(s, PATH).to_string()
}

pub(super) fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

pub(super) fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    let mut m = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes keys of any size");
    m.update(data.as_bytes());
    m.finalize().into_bytes().to_vec()
}

/// The sorted, encoded query string ("a=1&b=2"), the same for the address and the signature.
pub(super) fn canonical_query(params: &[(&str, String)]) -> String {
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
pub(super) struct Keys<'a> {
    pub(super) access: &'a str,
    pub(super) secret: &'a str,
    pub(super) region: &'a str,
}

/// The Authorization header's value. `headers` are (lowercase name, value) and must hold every
/// header to sign (host, x-amz-content-sha256, x-amz-date, and any others); `uri` and `query`
/// are already encoded.
pub(super) fn authorization(
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
