//! HTTP Digest login (RFC 7616): reading a server's challenge, and answering it.

/// What a server's `WWW-Authenticate: Digest` challenge asks for (RFC 7616).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Digest {
    pub(super) realm: String,
    pub(super) nonce: String,
    pub(super) opaque: Option<String>,
    /// SHA-256 (else MD5).
    pub(super) sha256: bool,
    /// The "-sess" variant of the algorithm.
    pub(super) sess: bool,
    /// The server wants a counter and client nonce (qop=auth). Old servers don't.
    pub(super) qop: bool,
    /// The challenge said the nonce is out of date, not that the password is wrong.
    pub(super) stale: bool,
    /// Requests sent with this nonce.
    pub(super) count: u32,
}

/// Splits `key=value, key="quoted, value"` pairs.
pub(super) fn params(s: &str) -> Vec<(String, String)> {
    let mut out = vec![];
    let mut rest = s.trim_start();
    while let Some(eq) = rest.find('=') {
        let key = rest[..eq]
            .trim()
            .trim_start_matches(',')
            .trim()
            .to_lowercase();
        let after = rest[eq + 1..].trim_start();
        let (value, tail) = if let Some(q) = after.strip_prefix('"') {
            let mut v = String::new();
            let mut chars = q.char_indices();
            let mut end = q.len();
            while let Some((i, c)) = chars.next() {
                match c {
                    '\\' => {
                        if let Some((_, n)) = chars.next() {
                            v.push(n);
                        }
                    }
                    '"' => {
                        end = i + 1;
                        break;
                    }
                    c => v.push(c),
                }
            }
            (v, &q[end.min(q.len())..])
        } else {
            let e = after.find(',').unwrap_or(after.len());
            (after[..e].trim().to_string(), &after[e..])
        };
        out.push((key, value));
        rest = tail.trim_start().trim_start_matches(',').trim_start();
    }
    out
}

/// The Digest challenge among the `WWW-Authenticate` values, the SHA-256 one when there are two.
pub(super) fn digest_challenge(values: &[&str]) -> Option<Digest> {
    let mut best: Option<Digest> = None;
    for v in values {
        let v = v.trim();
        let Some(args) = v
            .get(..7)
            .filter(|p| p.eq_ignore_ascii_case("digest "))
            .map(|_| &v[7..])
        else {
            continue;
        };
        let p = params(args);
        let get = |k: &str| p.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        let algo = get("algorithm")
            .unwrap_or_else(|| "MD5".into())
            .to_uppercase();
        let (sha256, sess) = match algo.as_str() {
            "MD5" => (false, false),
            "MD5-SESS" => (false, true),
            "SHA-256" => (true, false),
            "SHA-256-SESS" => (true, true),
            _ => continue,
        };
        let qop =
            get("qop").is_some_and(|q| q.split(',').any(|x| x.trim().eq_ignore_ascii_case("auth")));
        if get("qop").is_some() && !qop {
            continue;
        }
        let Some(nonce) = get("nonce") else { continue };
        let d = Digest {
            realm: get("realm").unwrap_or_default(),
            nonce,
            opaque: get("opaque"),
            sha256,
            sess,
            qop,
            stale: get("stale").is_some_and(|s| s.eq_ignore_ascii_case("true")),
            count: 0,
        };
        if best.as_ref().is_none_or(|b| !b.sha256 && d.sha256) {
            best = Some(d);
        }
    }
    best
}

impl Digest {
    pub(super) fn hash(&self, s: &str) -> String {
        if self.sha256 {
            use sha2::Digest as _;
            hex::encode(sha2::Sha256::digest(s.as_bytes()))
        } else {
            format!("{:x}", md5::compute(s.as_bytes()))
        }
    }

    /// The Authorization header for a request, with `nc` and `cnonce` given (for tests).
    pub(super) fn header_with(
        &self,
        user: &str,
        pass: &str,
        method: &str,
        uri: &str,
        nc: u32,
        cnonce: &str,
    ) -> String {
        let mut a1 = self.hash(&format!("{user}:{}:{pass}", self.realm));
        if self.sess {
            a1 = self.hash(&format!("{a1}:{}:{cnonce}", self.nonce));
        }
        let ha2 = self.hash(&format!("{method}:{uri}"));
        let nc_hex = format!("{nc:08x}");
        let response = if self.qop {
            self.hash(&format!("{a1}:{}:{nc_hex}:{cnonce}:auth:{ha2}", self.nonce))
        } else {
            self.hash(&format!("{a1}:{}:{ha2}", self.nonce))
        };
        let q = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        let mut h = format!(
            "Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{}\", algorithm={}{}, response=\"{response}\"",
            q(user),
            q(&self.realm),
            q(&self.nonce),
            q(uri),
            if self.sha256 { "SHA-256" } else { "MD5" },
            if self.sess { "-sess" } else { "" },
        );
        if self.qop {
            h.push_str(&format!(", qop=auth, nc={nc_hex}, cnonce=\"{cnonce}\""));
        }
        if let Some(o) = &self.opaque {
            h.push_str(&format!(", opaque=\"{}\"", q(o)));
        }
        h
    }
}
