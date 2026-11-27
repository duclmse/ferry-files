//! TLS for FTPS: the certificates the user said to trust, kept by fingerprint, and the config that checks them.

use super::*;

/// SHA-256 of a certificate as "AB:CD:…".
pub(super) fn fingerprint(der: &[u8]) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(der)
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Where the fingerprints of trusted certificates are kept.
pub(super) fn pin_file() -> Option<std::path::PathBuf> {
    Some(
        dirs::config_dir()?
            .join("ferry-files")
            .join("ftp_certs.json"),
    )
}

/// The one writing the file at a time.
pub(super) static PINS: Mutex<()> = Mutex::new(());

/// The fingerprint trusted for "host:port", if any.
pub(super) fn pin_get(file: &std::path::Path, key: &str) -> Option<String> {
    let _g = PINS.lock().unwrap_or_else(|e| e.into_inner());
    let map: std::collections::BTreeMap<String, String> =
        serde_json::from_slice(&std::fs::read(file).ok()?).ok()?;
    map.get(key).cloned()
}

/// Trusts the fingerprint for "host:port".
pub(super) fn pin_set(file: &std::path::Path, key: &str, fp: &str) -> std::io::Result<()> {
    let _g = PINS.lock().unwrap_or_else(|e| e.into_inner());
    let mut map: std::collections::BTreeMap<String, String> = std::fs::read(file)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    map.insert(key.to_string(), fp.to_string());
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = file.with_extension("json.tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec_pretty(&map).map_err(std::io::Error::other)?,
    )?;
    std::fs::rename(&tmp, file)
}

/// Checks a server's certificate as usual. One that fails, and that the user chose to trust, is
/// accepted the first time and its fingerprint kept; from then on only that one is accepted.
#[derive(Debug)]
pub(super) struct Pinning {
    pub(super) normal: Arc<WebPkiServerVerifier>,
    pub(super) trust: bool,
    /// "host:port".
    pub(super) key: String,
    pub(super) file: Option<std::path::PathBuf>,
    /// Why the last certificate was refused, in words for the user (rustls's error is lost on the
    /// way out of the TLS stream).
    pub(super) why: Arc<Mutex<Option<String>>>,
}

impl Pinning {
    fn refuse<T>(&self, message: String) -> std::result::Result<T, rustls::Error> {
        *self.why.lock().unwrap_or_else(|e| e.into_inner()) = Some(message.clone());
        Err(rustls::Error::General(message))
    }
}

impl ServerCertVerifier for Pinning {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        let problem = match self.normal.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        ) {
            Ok(ok) => return Ok(ok),
            Err(e) => e,
        };
        let fp = fingerprint(end_entity.as_ref());
        if !self.trust {
            return self.refuse(format!(
                "The server's certificate isn't trusted ({problem}). If you know this server, tick \"Trust this server's certificate\" in Settings > Connections. Its fingerprint is {fp}."
            ));
        }
        let Some(file) = &self.file else {
            return self.refuse(
                "There's no place to keep the server's certificate, so it can't be trusted.".into(),
            );
        };
        match pin_get(file, &self.key) {
            Some(p) if p == fp => Ok(ServerCertVerified::assertion()),
            Some(p) => self.refuse(format!(
                "The certificate of {} changed since you trusted it. Trusted: {p}. Now: {fp}. If the server was changed on purpose, remove {} from {} to trust the new one.",
                self.key,
                self.key,
                file.display()
            )),
            None => match pin_set(file, &self.key, &fp) {
                Ok(()) => Ok(ServerCertVerified::assertion()),
                Err(e) => self.refuse(format!("Couldn't keep the server's certificate in {}: {e}", file.display())),
            },
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.normal.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.normal.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.normal.supported_verify_schemes()
    }
}

/// Where a refused certificate's explanation is left.
pub(super) type Why = Arc<Mutex<Option<String>>>;

pub(super) fn tls_config(
    trust: bool,
    host: &str,
    port: u16,
    file: Option<std::path::PathBuf>,
) -> std::result::Result<(Arc<ClientConfig>, Why), String> {
    let roots = Arc::new(RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    });
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let normal = WebPkiServerVerifier::builder_with_provider(roots, provider.clone())
        .build()
        .map_err(|e| e.to_string())?;
    let why = Why::default();
    let verifier = Pinning {
        normal,
        trust,
        key: format!("{host}:{port}"),
        file,
        why: why.clone(),
    };
    let cfg = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    Ok((Arc::new(cfg), why))
}
