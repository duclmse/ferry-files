//! iPhones and iPads over USB, through Apple's usbmuxd (built into macOS; Apple Devices or
//! iTunes on Windows; the usbmuxd daemon on Linux) and the AFC file service, with the `idevice`
//! crate. What AFC shows is the media area (DCIM, Downloads, Books, ...). Apps that share their
//! files (as in Finder's Files tab) show under `/Apps`, each its Documents folder.
//!
//! `idevice` is async; each connection runs it on its own small tokio runtime, a call at a time.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use idevice::afc::opcode::AfcFopenMode;
use idevice::afc::AfcClient;
use idevice::house_arrest::HouseArrestClient;
use idevice::installation_proxy::InstallationProxyClient;
use idevice::lockdown::LockdownClient;
use idevice::provider::UsbmuxdProvider;
use idevice::usbmuxd::{Connection, UsbmuxdAddr, UsbmuxdConnection};
use idevice::{IdeviceError, IdeviceService};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

use crate::device::{basename, dirname, err, join, DevError, Entry, Result, Space, Transport};

const LABEL: &str = "ferry-files";
/// The folder apps' shared files show in.
const APPS: &str = "/Apps";
/// Bytes per read or write when copying.
const PIECE: usize = 1 << 20;

/// What to tell someone whose iPhone hasn't trusted this computer yet. The frontend retries on
/// "tap Trust" (`needs_access`).
pub const TRUST_HINT: &str = "Unlock the iPhone and tap Trust when it asks, then it connects.";

fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .map_err(|e| DevError::Msg(format!("Can't start the iPhone connection: {e}")))
}

/// An error from the iPhone in plain words.
fn plain(e: IdeviceError) -> DevError {
    use idevice::afc::errors::AfcError;
    DevError::Msg(match e {
        // No pair record yet (usbmuxd has none), or the iPhone refusing a session: it hasn't
        // trusted this computer, or it's locked.
        IdeviceError::InvalidHostID
        | IdeviceError::DeviceLocked
        | IdeviceError::SessionInactive => TRUST_HINT.into(),
        IdeviceError::UnexpectedResponse(m) if m.contains("PairRecord") => TRUST_HINT.into(),
        IdeviceError::UnknownErrorType(m)
            if m.contains("Pairing")
                || m.contains("PasswordProtected")
                || m.contains("InvalidHostID") =>
        {
            if m.contains("UserDenied") {
                "The iPhone didn't trust this computer. Unplug it, plug it in again and tap Trust."
                    .into()
            } else {
                TRUST_HINT.into()
            }
        }
        IdeviceError::DeviceNotFound => "The iPhone isn't connected any more.".into(),
        IdeviceError::Afc(AfcError::ObjectNotFound) => "Not found on the iPhone.".into(),
        IdeviceError::Afc(AfcError::PermDenied) => "The iPhone doesn't allow that here.".into(),
        IdeviceError::Afc(AfcError::NoSpaceLeft) => "The iPhone is full.".into(),
        IdeviceError::Afc(AfcError::ObjectExists) => {
            "Something with that name is already there.".into()
        }
        IdeviceError::Afc(AfcError::DirNotEmpty) => "The folder isn't empty.".into(),
        e => format!("The iPhone said: {e}"),
    })
}

/// Whether the connection itself broke (so the next call connects again), rather than the
/// iPhone refusing one request.
fn broke(e: &IdeviceError) -> bool {
    !matches!(e, IdeviceError::Afc(_))
}

/// An iPhone or iPad on USB, not connected yet.
pub struct Detected {
    pub udid: String,
    pub name: String,
}

/// Names already asked for, by UDID: asking takes a lockdown connection, and devices are looked
/// for every few seconds.
static NAMES: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

/// The iPhones and iPads on USB. An error when usbmuxd can't be reached (on Windows, Apple's
/// device service isn't installed; on Linux, usbmuxd isn't running).
pub fn detect() -> Result<Vec<Detected>> {
    let rt = runtime()?;
    rt.block_on(async {
        let mut mux = UsbmuxdConnection::default()
            .await
            .map_err(|_| DevError::Msg(crate::platform::IPHONE_HINT.into()))?;
        let devs = mux.get_devices().await.map_err(plain)?;
        let mut out = vec![];
        for d in devs
            .into_iter()
            .filter(|d| matches!(d.connection_type, Connection::Usb))
        {
            let known = NAMES
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get_or_insert_with(HashMap::new)
                .get(&d.udid)
                .cloned();
            let name = match known {
                Some(n) => n,
                None => {
                    let provider = d.to_provider(UsbmuxdAddr::default(), LABEL);
                    match device_name(&provider).await {
                        Some(n) => {
                            NAMES
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .get_or_insert_with(HashMap::new)
                                .insert(d.udid.clone(), n.clone());
                            n
                        }
                        // Asked again next time: it may tell once it trusts this computer.
                        None => "iPhone".into(),
                    }
                }
            };
            out.push(Detected { udid: d.udid, name });
        }
        Ok(out)
    })
}

/// The UDIDs of the iPhones on USB, None when usbmuxd can't be asked: for telling when a
/// connected one is gone, without asking each its name.
pub fn listed() -> Option<Vec<String>> {
    let rt = runtime().ok()?;
    rt.block_on(async {
        let mut mux = UsbmuxdConnection::default().await.ok()?;
        let devs = mux.get_devices().await.ok()?;
        Some(
            devs.into_iter()
                .filter(|d| matches!(d.connection_type, Connection::Usb))
                .map(|d| d.udid)
                .collect(),
        )
    })
}

/// The name set on the iPhone ("Ann's iPhone").
async fn device_name(provider: &UsbmuxdProvider) -> Option<String> {
    let mut lockdown = LockdownClient::connect(provider).await.ok()?;
    let v = lockdown.get_value(Some("DeviceName"), None).await.ok()?;
    v.as_string().map(str::to_string).filter(|s| !s.is_empty())
}

/// An app that shares its files: shown as `/Apps/<name>`.
#[derive(Clone, Debug, PartialEq)]
struct App {
    name: String,
    bundle: String,
}

/// Where a path is: in the media area, the `/Apps` folder itself, or in an app's Documents
/// (`inner` being the path in that app's AFC, "/Documents/...").
#[derive(Debug, PartialEq)]
enum Place {
    Media(String),
    Apps,
    App { bundle: String, inner: String },
}

struct Conn {
    rt: tokio::runtime::Runtime,
    provider: UsbmuxdProvider,
    afc: Option<AfcClient>,
    apps: Option<Vec<App>>,
    vended: HashMap<String, AfcClient>,
}

pub struct IPhone {
    name: String,
    conn: Mutex<Conn>,
    stop: AtomicBool,
}

impl IPhone {
    pub fn open(udid: &str) -> Result<IPhone> {
        let rt = runtime()?;
        let (provider, name, afc) = rt.block_on(async {
            let mut mux = UsbmuxdConnection::default()
                .await
                .map_err(|_| DevError::Msg(crate::platform::IPHONE_HINT.into()))?;
            let dev = mux
                .get_device(udid)
                .await
                .map_err(|_| DevError::Msg("The iPhone isn't connected any more.".into()))?;
            let provider = dev.to_provider(UsbmuxdAddr::default(), LABEL);
            let afc = AfcClient::connect(&provider).await.map_err(plain)?;
            let name = device_name(&provider)
                .await
                .unwrap_or_else(|| "iPhone".into());
            Ok::<_, DevError>((provider, name, afc))
        })?;
        Ok(IPhone {
            name,
            conn: Mutex::new(Conn {
                rt,
                provider,
                afc: Some(afc),
                apps: None,
                vended: HashMap::new(),
            }),
            stop: AtomicBool::new(false),
        })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Conn> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The AFC connection for `bundle`'s Documents (None: the media area), connected first when
    /// needed. Held until dropped, so one call goes at a time.
    fn session(&self, bundle: Option<&str>) -> Result<Session<'_>> {
        let mut guard = self.conn();
        let c = &mut *guard;
        match bundle {
            None if c.afc.is_none() => {
                c.afc = Some(
                    c.rt.block_on(AfcClient::connect(&c.provider))
                        .map_err(plain)?,
                )
            }
            Some(b) if !c.vended.contains_key(b) => {
                let afc =
                    c.rt.block_on(async {
                        let ha = HouseArrestClient::connect(&c.provider).await?;
                        ha.vend_documents(b).await
                    })
                    .map_err(plain)?;
                c.vended.insert(b.to_string(), afc);
            }
            _ => {}
        }
        Ok(Session {
            guard,
            bundle: bundle.map(str::to_string),
        })
    }

    /// The apps that share their files, by name, asked for once per connection.
    fn apps(&self) -> Result<Vec<App>> {
        let mut guard = self.conn();
        let c = &mut *guard;
        if let Some(a) = &c.apps {
            return Ok(a.clone());
        }
        let found =
            c.rt.block_on(async {
                let mut ip = InstallationProxyClient::connect(&c.provider).await?;
                ip.get_apps(Some("User"), None).await
            })
            .map_err(plain)?;
        let apps = sharing_apps(found);
        c.apps = Some(apps.clone());
        Ok(apps)
    }

    fn place(&self, path: &str) -> Result<Place> {
        let Some(rest) = path
            .strip_prefix(APPS)
            .filter(|r| r.is_empty() || r.starts_with('/'))
        else {
            return Ok(Place::Media(path.to_string()));
        };
        let rest = rest.trim_start_matches('/');
        if rest.is_empty() {
            return Ok(Place::Apps);
        }
        let (name, tail) = rest.split_once('/').unwrap_or((rest, ""));
        let apps = self.apps()?;
        let Some(app) = apps.iter().find(|a| a.name == name) else {
            return err(format!("No app named {name} shares its files."));
        };
        Ok(Place::App {
            bundle: app.bundle.clone(),
            inner: if tail.is_empty() {
                "/Documents".into()
            } else {
                format!("/Documents/{tail}")
            },
        })
    }

    /// The app (None: the media area) and AFC path for a file: refuses `/Apps` and an app's own
    /// folder.
    fn file_place(&self, path: &str) -> Result<(Option<String>, String)> {
        match self.place(path)? {
            Place::Media(p) if p == "/" => err("That's the top of the iPhone."),
            Place::Media(p) => Ok((None, p)),
            Place::Apps => err("Apps is where apps' shared files are; it can't be changed."),
            Place::App { inner, .. } if inner == "/Documents" => {
                err("That's an app's own folder; it can't be changed.")
            }
            Place::App { bundle, inner } => Ok((Some(bundle), inner)),
        }
    }

    /// What AFC tells about `path`, None when it isn't there.
    fn info(&self, bundle: Option<&str>, path: &str) -> Result<Option<Info>> {
        let mut s = self.session(bundle)?;
        let (rt, afc) = s.parts();
        let r = rt.block_on(async {
            match afc.get_file_info_raw(path).await {
                Ok(kv) => Ok(Some(Info::of(&kv))),
                // Asking about a missing file may fail other ways than "not found", so it's
                // looked for in its folder.
                Err(e) if !broke(&e) => {
                    if path == "/" {
                        return Err(e);
                    }
                    let names = afc.list_dir(dirname(path)).await.unwrap_or_default();
                    if names.iter().any(|n| n == basename(path)) {
                        Err(e)
                    } else {
                        Ok(None)
                    }
                }
                Err(e) => Err(e),
            }
        });
        s.done(r)
    }

    /// Writes a new file through a temporary name in the same folder, swapped in when complete,
    /// with `size` bytes from `src`.
    fn put(
        &self,
        path: &str,
        exists: bool,
        size: u64,
        src: &mut dyn Read,
        what: &str,
    ) -> Result<String> {
        let (bundle, inner) = self.file_place(path)?;
        let b = bundle.as_deref();
        let tmp = join(
            &dirname(&inner),
            &format!(".{}.ferry-tmp", basename(&inner)),
        );
        let stop = &self.stop;
        let mut s = self.session(b)?;
        let (rt, afc) = s.parts();
        let mut failed: Option<DevError> = None;
        let r = rt.block_on(async {
            let mut f = afc.open(tmp.as_str(), AfcFopenMode::WrOnly).await?;
            let mut buf = vec![0u8; PIECE];
            let sent = loop {
                if stop.load(Ordering::Relaxed) {
                    failed = Some(DevError::Msg("Cancelled.".into()));
                    break Ok(());
                }
                let n = match src.read(&mut buf) {
                    Ok(0) => break f.flush().await.map_err(IdeviceError::Socket),
                    Ok(n) => n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        failed = Some(DevError::Msg(format!("Can't read {what}: {e}")));
                        break Ok(());
                    }
                };
                if let Err(e) = f.write_all(&buf[..n]).await {
                    break Err(IdeviceError::Socket(e));
                }
            };
            let closed = f.close().await;
            sent.and(closed)
        });
        let r = s.done(r);
        drop(s);
        let checked = match (r, failed) {
            (_, Some(e)) | (Err(e), None) => Err(e),
            (Ok(()), None) => match self.info(b, &tmp)? {
                Some(i) if i.size == size => Ok(()),
                Some(i) => err(format!(
                    "The iPhone has {} of {size} bytes; nothing was changed.",
                    i.size
                )),
                None => err("The new file didn't arrive; nothing was changed."),
            },
        };
        let mut s = self.session(b)?;
        let (rt, afc) = s.parts();
        if let Err(e) = checked {
            let _ = rt.block_on(afc.remove(tmp.as_str()));
            return Err(e);
        }
        let r = rt.block_on(async {
            match afc.rename(tmp.as_str(), inner.as_str()).await {
                // In case AFC won't rename over a file: the old one goes first (it's backed up).
                Err(e) if exists && !broke(&e) => {
                    afc.remove(inner.as_str()).await?;
                    afc.rename(tmp.as_str(), inner.as_str()).await
                }
                r => r,
            }
        });
        s.done(r)?;
        drop(s);
        match self.info(b, &inner)? {
            Some(i) => Ok(i.version()),
            None => err("The file isn't there after writing it."),
        }
    }

    /// Runs one AFC request on `bundle`'s connection.
    fn ask<T>(
        &self,
        bundle: Option<&str>,
        f: impl FnOnce(
            &mut AfcClient,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = std::result::Result<T, IdeviceError>> + '_>,
        >,
    ) -> Result<T> {
        let mut s = self.session(bundle)?;
        let (rt, afc) = s.parts();
        let r = rt.block_on(f(afc));
        s.done(r)
    }
}

/// The connection held for a call.
struct Session<'g> {
    guard: std::sync::MutexGuard<'g, Conn>,
    bundle: Option<String>,
}

impl Session<'_> {
    fn parts(&mut self) -> (&tokio::runtime::Runtime, &mut AfcClient) {
        let c = &mut *self.guard;
        let afc = match &self.bundle {
            None => c.afc.as_mut(),
            Some(b) => c.vended.get_mut(b),
        };
        (&c.rt, afc.expect("connected by session()"))
    }

    /// The answer in plain words. A broken connection is let go of, so the next call connects
    /// again.
    fn done<T>(&mut self, r: std::result::Result<T, IdeviceError>) -> Result<T> {
        if r.as_ref().is_err_and(broke) {
            let c = &mut *self.guard;
            match &self.bundle {
                None => c.afc = None,
                Some(b) => {
                    c.vended.remove(b);
                }
            }
        }
        r.map_err(plain)
    }
}

/// From installation_proxy's apps: those that share files (UIFileSharingEnabled), named by their
/// display name, with the bundle id added to tell two of the same name apart. Sorted by name.
fn sharing_apps(found: HashMap<String, plist::Value>) -> Vec<App> {
    let mut apps: Vec<App> = found
        .into_iter()
        .filter_map(|(bundle, v)| {
            let d = v.as_dictionary()?;
            let shares = d.get("UIFileSharingEnabled").and_then(|v| {
                v.as_boolean()
                    .or_else(|| v.as_string().map(|s| s == "YES" || s == "true"))
            });
            if shares != Some(true) {
                return None;
            }
            let name = ["CFBundleDisplayName", "CFBundleName"]
                .iter()
                .find_map(|k| {
                    d.get(*k)
                        .and_then(|v| v.as_string())
                        .filter(|s| !s.trim().is_empty())
                })
                .unwrap_or(&bundle);
            Some(App {
                name: name.trim().replace('/', "-"),
                bundle,
            })
        })
        .collect();
    apps.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then(a.bundle.cmp(&b.bundle))
    });
    let names: Vec<String> = apps.iter().map(|a| a.name.clone()).collect();
    for a in apps.iter_mut() {
        if names.iter().filter(|n| **n == a.name).count() > 1 {
            a.name = format!("{} ({})", a.name, a.bundle);
        }
    }
    apps
}

/// What AFC tells about a file.
#[derive(Debug, PartialEq)]
struct Info {
    dir: bool,
    size: u64,
    /// Nanoseconds since 1970.
    mtime_ns: i64,
}

impl Info {
    fn of(kv: &HashMap<String, String>) -> Info {
        let num = |k: &str| kv.get(k).and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
        Info {
            dir: kv.get("st_ifmt").is_some_and(|t| t == "S_IFDIR"),
            size: num("st_size").max(0) as u64,
            mtime_ns: num("st_mtime"),
        }
    }

    fn version(&self) -> String {
        format!("{}:{}", self.size, self.mtime_ns)
    }

    fn entry(&self, path: &str) -> Entry {
        Entry {
            name: basename(path).to_string(),
            path: path.to_string(),
            dir: self.dir,
            size: if self.dir { 0 } else { self.size },
            mtime: self.mtime_ns.div_euclid(1_000_000_000),
            hidden: false,
        }
    }
}

fn folder(path: &str) -> Entry {
    Entry {
        name: basename(path).to_string(),
        path: path.to_string(),
        dir: true,
        size: 0,
        mtime: 0,
        hidden: false,
    }
}

impl Transport for IPhone {
    fn kind(&self) -> &'static str {
        "iphone"
    }

    fn label(&self) -> String {
        self.name.clone()
    }

    fn default_dir(&self) -> String {
        "/".into()
    }

    fn quick_dirs(&self) -> Vec<(String, String)> {
        vec![
            ("Photos".into(), "/DCIM".into()),
            ("Downloads".into(), "/Downloads".into()),
            ("Books".into(), "/Books".into()),
            ("Apps".into(), APPS.into()),
        ]
    }

    fn refresh(&self) {
        self.conn().apps = None;
    }

    fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    fn listdir(&self, path: &str) -> Result<Vec<Entry>> {
        let (bundle, dir) = match self.place(path)? {
            Place::Apps => {
                return Ok(self
                    .apps()?
                    .iter()
                    .map(|a| folder(&join(APPS, &a.name)))
                    .collect())
            }
            Place::Media(p) => (None, p),
            Place::App { bundle, inner } => (Some(bundle), inner),
        };
        let mut s = self.session(bundle.as_deref())?;
        let (rt, afc) = s.parts();
        let r = rt.block_on(async {
            let mut out = vec![];
            for name in afc.list_dir(dir.as_str()).await? {
                if name == "." || name == ".." {
                    continue;
                }
                let info = afc.get_file_info_raw(join(&dir, &name)).await;
                if info.as_ref().is_err_and(broke) {
                    return Err(info.err().expect("is an error"));
                }
                out.push((name, info.ok()));
            }
            Ok(out)
        });
        let named = s.done(r)?;
        let mut out: Vec<Entry> = named
            .into_iter()
            .map(|(name, info)| {
                let p = join(path, &name);
                match info {
                    Some(kv) => Info::of(&kv).entry(&p),
                    // Listed but can't be looked at: shown as a file of unknown size.
                    None => Entry {
                        name,
                        path: p,
                        dir: false,
                        size: 0,
                        mtime: 0,
                        hidden: false,
                    },
                }
            })
            .collect();
        if path == "/" && !out.iter().any(|e| e.name == "Apps") {
            out.push(folder(APPS));
        }
        Ok(out)
    }

    fn stat(&self, path: &str) -> Result<Option<Entry>> {
        match self.place(path) {
            Ok(Place::Apps) => Ok(Some(folder(APPS))),
            Ok(Place::App { inner, .. }) if inner == "/Documents" => Ok(Some(folder(path))),
            Ok(Place::App { bundle, inner }) => {
                Ok(self.info(Some(&bundle), &inner)?.map(|i| i.entry(path)))
            }
            Ok(Place::Media(p)) => Ok(self.info(None, &p)?.map(|i| i.entry(path))),
            // An app that isn't there (any more).
            Err(_) => Ok(None),
        }
    }

    fn read(&self, path: &str) -> Result<(Vec<u8>, String)> {
        let (bundle, inner) = self.file_place(path)?;
        let b = bundle.as_deref();
        let before = self
            .info(b, &inner)?
            .ok_or_else(|| DevError::Msg(format!("Not found: {path}")))?;
        if before.dir {
            return err(format!("{path} is a folder."));
        }
        let data = self.ask(b, |afc| {
            Box::pin(async move {
                let mut f = afc.open(inner, AfcFopenMode::RdOnly).await?;
                let data = f.read_entire().await;
                let closed = f.close().await;
                let data = data?;
                closed.map(|_| data)
            })
        })?;
        if data.len() as u64 != before.size {
            return err(format!(
                "Read {} of {} bytes of {path}; try again.",
                data.len(),
                before.size
            ));
        }
        Ok((data, before.version()))
    }

    fn read_range(&self, path: &str, start: u64, len: u64) -> Result<Vec<u8>> {
        let (bundle, inner) = self.file_place(path)?;
        self.ask(bundle.as_deref(), |afc| {
            Box::pin(async move {
                let mut f = afc.open(inner, AfcFopenMode::RdOnly).await?;
                let data = match f.seek(std::io::SeekFrom::Start(start)).await {
                    Ok(_) => f.read_n(len as usize).await,
                    Err(e) => Err(IdeviceError::Socket(e)),
                };
                let closed = f.close().await;
                let data = data?;
                closed.map(|_| data)
            })
        })
    }

    fn version(&self, path: &str) -> Result<Option<String>> {
        match self.file_place(path) {
            Ok((bundle, inner)) => Ok(self.info(bundle.as_deref(), &inner)?.map(|i| i.version())),
            Err(_) => Ok(None),
        }
    }

    fn replace(&self, path: &str, data: &[u8], exists: bool) -> Result<String> {
        self.put(
            path,
            exists,
            data.len() as u64,
            &mut &data[..],
            "the new contents",
        )
    }

    fn replace_from(&self, path: &str, from: &Path, exists: bool) -> Result<String> {
        let what = from.display().to_string();
        let mut src = std::fs::File::open(from)
            .map_err(|e| DevError::Msg(format!("Can't read {what}: {e}")))?;
        let size = src
            .metadata()
            .map_err(|e| DevError::Msg(format!("Can't read {what}: {e}")))?
            .len();
        self.put(path, exists, size, &mut src, &what)
    }

    fn read_to(
        &self,
        path: &str,
        to: &Path,
        read: &mut dyn FnMut(u64),
        stop: &AtomicBool,
    ) -> Result<String> {
        let (bundle, inner) = self.file_place(path)?;
        let b = bundle.as_deref();
        let before = self
            .info(b, &inner)?
            .ok_or_else(|| DevError::Msg(format!("Not found: {path}")))?;
        if before.dir {
            return err(format!("{path} is a folder."));
        }
        let mut out = std::fs::File::create(to)
            .map_err(|e| DevError::Msg(format!("Can't write {}: {e}", to.display())))?;
        let mine = &self.stop;
        let mut failed: Option<DevError> = None;
        let mut s = self.session(b)?;
        let (rt, afc) = s.parts();
        let r = rt.block_on(async {
            let mut f = afc.open(inner.as_str(), AfcFopenMode::RdOnly).await?;
            let mut total = 0u64;
            let got = loop {
                if stop.load(Ordering::Relaxed) || mine.load(Ordering::Relaxed) {
                    failed = Some(DevError::Msg("Cancelled.".into()));
                    break Ok(total);
                }
                let piece = match f.read_n(PIECE).await {
                    Ok(p) => p,
                    Err(e) => break Err(e),
                };
                if piece.is_empty() {
                    break Ok(total);
                }
                if let Err(e) = out.write_all(&piece) {
                    failed = Some(DevError::Msg(format!("Can't write {}: {e}", to.display())));
                    break Ok(total);
                }
                total += piece.len() as u64;
                read(piece.len() as u64);
            };
            let closed = f.close().await;
            let total = got?;
            closed.map(|_| total)
        });
        let total = s.done(r)?;
        if let Some(e) = failed {
            return Err(e);
        }
        if total != before.size {
            return err(format!(
                "Read {total} of {} bytes of {path}; try again.",
                before.size
            ));
        }
        Ok(before.version())
    }

    fn delete(&self, path: &str, is_dir: bool) -> Result<()> {
        let (bundle, inner) = self.file_place(path)?;
        self.ask(bundle.as_deref(), |afc| {
            Box::pin(async move {
                if is_dir {
                    afc.remove_all(inner).await
                } else {
                    afc.remove(inner).await
                }
            })
        })
    }

    fn make_dir(&self, path: &str) -> Result<()> {
        let (bundle, inner) = self.file_place(path)?;
        self.ask(bundle.as_deref(), |afc| {
            Box::pin(async move { afc.mk_dir(inner).await })
        })
    }

    fn rename(&self, path: &str, new_name: &str) -> Result<()> {
        let (bundle, inner) = self.file_place(path)?;
        let to = join(&dirname(&inner), new_name);
        self.ask(bundle.as_deref(), |afc| {
            Box::pin(async move { afc.rename(inner, to).await })
        })
    }

    fn space(&self) -> Vec<Space> {
        match self.ask(None, |afc| {
            Box::pin(async move { afc.get_device_info().await })
        }) {
            Ok(d) => vec![Space {
                name: self.name.clone(),
                free: d.free_bytes as u64,
                total: d.total_bytes as u64,
                path: "/".into(),
            }],
            Err(_) => vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kv(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// The app runs device work in tokio's blocking threads (`blocking()`); each connection's
    /// own runtime must work there. Asks usbmuxd for its devices, which is harmless (and gives
    /// None where there's no usbmuxd).
    #[test]
    fn runs_in_the_apps_blocking_threads() {
        let outer = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let listed = outer.block_on(async { tokio::task::spawn_blocking(listed).await.unwrap() });
        if cfg!(target_os = "macos") {
            assert!(listed.is_some(), "macOS has usbmuxd");
        }
        assert_eq!(detect().is_ok(), listed.is_some());
    }

    #[test]
    fn reads_what_afc_tells_about_a_file() {
        let f = Info::of(&kv(&[
            ("st_ifmt", "S_IFREG"),
            ("st_size", "1234"),
            ("st_mtime", "1700000000123456789"),
        ]));
        assert_eq!(
            f,
            Info {
                dir: false,
                size: 1234,
                mtime_ns: 1_700_000_000_123_456_789
            }
        );
        assert_eq!(f.version(), "1234:1700000000123456789");
        let e = f.entry("/DCIM/100APPLE/IMG_0001.HEIC");
        assert_eq!(
            (e.name.as_str(), e.dir, e.size, e.mtime),
            ("IMG_0001.HEIC", false, 1234, 1_700_000_000)
        );
        let d = Info::of(&kv(&[("st_ifmt", "S_IFDIR"), ("st_size", "96")]));
        assert!(d.dir);
        assert_eq!(d.entry("/DCIM").size, 0);
        // Missing or odd values don't fail.
        assert_eq!(
            Info::of(&kv(&[("st_size", "x")])),
            Info {
                dir: false,
                size: 0,
                mtime_ns: 0
            }
        );
    }

    #[test]
    fn lists_only_apps_that_share_files() {
        let app = |name: Option<&str>, display: Option<&str>, shares: Option<plist::Value>| {
            let mut d = plist::Dictionary::new();
            if let Some(n) = name {
                d.insert("CFBundleName".into(), n.into());
            }
            if let Some(n) = display {
                d.insert("CFBundleDisplayName".into(), n.into());
            }
            if let Some(s) = shares {
                d.insert("UIFileSharingEnabled".into(), s);
            }
            plist::Value::Dictionary(d)
        };
        let found: HashMap<String, plist::Value> = [
            (
                "org.videolan.vlc-ios",
                app(Some("VLC"), Some("VLC"), Some(true.into())),
            ),
            (
                "com.example.notes",
                app(Some("Notes"), None, Some(false.into())),
            ),
            ("com.example.plain", app(Some("Plain"), None, None)),
            (
                "com.example.reader",
                app(Some("Reader"), Some("Books/PDF"), Some("YES".into())),
            ),
            ("com.a.twin", app(Some("Twin"), None, Some(true.into()))),
            ("com.b.twin", app(Some("Twin"), None, Some(true.into()))),
            ("com.example.nameless", app(None, None, Some(true.into()))),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let names: Vec<String> = sharing_apps(found).into_iter().map(|a| a.name).collect();
        assert_eq!(
            names,
            [
                "Books-PDF",
                "com.example.nameless",
                "Twin (com.a.twin)",
                "Twin (com.b.twin)",
                "VLC"
            ]
        );
    }
}
