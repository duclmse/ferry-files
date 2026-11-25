//! What WebDAV, S3 and Everything share over HTTP (ureq).
//!
//! An answer with an error status is kept apart from no answer at all, with the answer itself:
//! S3 explains its errors in the body, and WebDAV needs a 401's challenge and a 3xx's address.

use std::io::Read;

pub type Response = ureq::http::Response<ureq::Body>;

/// Why a request didn't succeed.
#[derive(Debug)]
pub enum Error {
    /// The server answered with this status: 400 or above, or a redirect not followed.
    Status(u16, Response),
    /// No answer: the address, the connection or TLS.
    Transport(ureq::Error),
}

impl From<ureq::Error> for Error {
    fn from(e: ureq::Error) -> Error {
        Error::Transport(e)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error {
        Error::Transport(ureq::Error::Io(e))
    }
}

/// An agent's settings before the timeouts and redirects each server picks.
pub fn config() -> ureq::config::ConfigBuilder<ureq::typestate::AgentScope> {
    ureq::Agent::config_builder()
        // Error statuses come back as answers, which `checked` sorts out, so their bodies and
        // headers can still be read.
        .http_status_as_error(false)
        // WebDAV's PROPFIND, MKCOL and MOVE.
        .allow_non_standard_methods(true)
}

/// The answer, or its status as an error when it's 400 or above.
pub fn checked(sent: Result<Response, ureq::Error>) -> Result<Response, Error> {
    let res = sent?;
    match res.status().as_u16() {
        code if code >= 400 => Err(Error::Status(code, res)),
        _ => Ok(res),
    }
}

/// Sends `method` to `url` with `headers`, and the body if there is one: its bytes, and how many.
pub fn run(
    agent: &ureq::Agent,
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<(&mut dyn Read, u64)>,
) -> Result<Response, Error> {
    let mut req = ureq::http::Request::builder().method(method).uri(url);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    // Each arm runs its own request: `SendBody::none()` is only ever `'static`, and the body's
    // lifetime can't be shortened to the reader's.
    let sent = match body {
        // Sent with its length: a body read as it goes would go chunked, which S3 refuses.
        Some((from, n)) => {
            let req = req.header("Content-Length", n);
            agent.run(
                req.body(ureq::SendBody::from_reader(from))
                    .map_err(ureq::Error::Http)?,
            )
        }
        None => agent.run(
            req.body(ureq::SendBody::none())
                .map_err(ureq::Error::Http)?,
        ),
    };
    checked(sent)
}

/// The value of the answer's header `name`, when it's text.
pub fn header<'a>(res: &'a Response, name: &str) -> Option<&'a str> {
    res.headers().get(name)?.to_str().ok()
}

/// Every value of the answer's header `name` that's text.
pub fn headers<'a>(res: &'a Response, name: &str) -> Vec<&'a str> {
    res.headers()
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect()
}

/// The answer's body as it comes, with no limit on its size.
pub fn reader(res: Response) -> impl Read + Send + 'static {
    res.into_body().into_reader()
}

/// The answer's body as text, up to ureq's limit (10 MB).
pub fn text(res: Response) -> Result<String, ureq::Error> {
    res.into_body().read_to_string()
}
