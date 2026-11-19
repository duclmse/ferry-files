//! SFTP v3 request handling for one session, confined to a [`Jail`].

use std::collections::HashMap;
use std::fs::{File as StdFile, Metadata, OpenOptions};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use russh_sftp::protocol::{
    Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode, Version,
};

use crate::jail::Jail;

/// Bigger reads are cut down; clients ask again for the rest.
const MAX_READ: u32 = 256 * 1024;
const DIR_BATCH: usize = 128;

enum Open {
    File(Arc<StdFile>),
    Dir(Vec<File>),
}

pub struct SftpSession {
    jail: Arc<Jail>,
    read_only: bool,
    user: String,
    handles: HashMap<String, Open>,
    next_handle: u64,
}

impl SftpSession {
    pub fn new(jail: Arc<Jail>, read_only: bool, user: String) -> Self {
        Self {
            jail,
            read_only,
            user,
            handles: HashMap::new(),
            next_handle: 0,
        }
    }

    fn add(&mut self, open: Open) -> String {
        self.next_handle += 1;
        let handle = self.next_handle.to_string();
        self.handles.insert(handle.clone(), open);
        handle
    }

    fn file(&self, handle: &str) -> Result<Arc<StdFile>, StatusCode> {
        match self.handles.get(handle) {
            Some(Open::File(f)) => Ok(Arc::clone(f)),
            _ => Err(StatusCode::Failure),
        }
    }

    fn writable(&self) -> Result<(), StatusCode> {
        if self.read_only {
            Err(StatusCode::PermissionDenied)
        } else {
            Ok(())
        }
    }

    fn resolve(&self, path: &str) -> Result<PathBuf, StatusCode> {
        self.jail.resolve(path)
    }

    /// Refuses operations that would remove or replace the served folder.
    fn resolve_below_root(&self, path: &str) -> Result<PathBuf, StatusCode> {
        let real = self.resolve(path)?;
        if real == self.jail.root() {
            return Err(StatusCode::PermissionDenied);
        }
        Ok(real)
    }

    fn log(&self, op: &str, path: &str) {
        tracing::debug!(user = %self.user, "{op} {path}");
    }
}

fn status(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: "Ok".into(),
        language_tag: "en-US".into(),
    }
}

fn code(e: io::Error) -> StatusCode {
    match e.kind() {
        io::ErrorKind::NotFound => StatusCode::NoSuchFile,
        io::ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
        _ => StatusCode::Failure,
    }
}

async fn blocking<T, F>(f: F) -> Result<T, StatusCode>
where
    T: Send + 'static,
    F: FnOnce() -> io::Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| StatusCode::Failure)?
        .map_err(code)
}

fn attrs_of(meta: &Metadata) -> FileAttributes {
    FileAttributes::from(meta)
}

#[cfg(unix)]
fn read_at(file: &StdFile, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buf, offset)
}

#[cfg(windows)]
fn read_at(file: &StdFile, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buf, offset)
}

#[cfg(unix)]
fn write_all_at(file: &StdFile, buf: &[u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, buf, offset)
}

#[cfg(windows)]
fn write_all_at(file: &StdFile, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        let n = std::os::windows::fs::FileExt::seek_write(file, buf, offset)?;
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        buf = &buf[n..];
        offset += n as u64;
    }
    Ok(())
}

fn apply_attrs(file: &StdFile, attrs: &FileAttributes) -> io::Result<()> {
    if let Some(size) = attrs.size {
        file.set_len(size)?;
    }
    #[cfg(unix)]
    if let Some(mode) = attrs.permissions {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(mode & 0o7777))?;
    }
    if let Some(mtime) = attrs.mtime {
        let at = |s: u32| std::time::UNIX_EPOCH + std::time::Duration::from_secs(u64::from(s));
        let times = std::fs::FileTimes::new()
            .set_modified(at(mtime))
            .set_accessed(at(attrs.atime.unwrap_or(mtime)));
        file.set_times(times)?;
    }
    Ok(())
}

impl russh_sftp::server::Handler for SftpSession {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    async fn init(
        &mut self,
        _version: u32,
        _extensions: HashMap<String, String>,
    ) -> Result<Version, Self::Error> {
        Ok(Version::new())
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: OpenFlags,
        attrs: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        self.log("open", &filename);
        let writes = pflags.intersects(
            OpenFlags::WRITE | OpenFlags::APPEND | OpenFlags::CREATE | OpenFlags::TRUNCATE,
        );
        if writes {
            self.writable()?;
        }
        let path = self.resolve(&filename)?;
        let mut options = OpenOptions::new();
        options
            .read(pflags.contains(OpenFlags::READ) || !writes)
            .write(writes)
            .append(pflags.contains(OpenFlags::APPEND))
            .truncate(pflags.contains(OpenFlags::TRUNCATE));
        if pflags.contains(OpenFlags::CREATE) {
            if pflags.contains(OpenFlags::EXCLUDE) {
                options.create_new(true);
            } else {
                options.create(true);
            }
        }
        #[cfg(unix)]
        if let Some(mode) = attrs.permissions {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(mode & 0o777);
        }
        #[cfg(not(unix))]
        let _ = attrs;
        let file = blocking(move || {
            let file = options.open(&path)?;
            if file.metadata()?.is_dir() {
                return Err(io::Error::other("is a folder"));
            }
            Ok(file)
        })
        .await?;
        let handle = self.add(Open::File(Arc::new(file)));
        Ok(Handle { id, handle })
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        match self.handles.remove(&handle) {
            Some(Open::File(file)) => {
                blocking(move || file.sync_data()).await?;
                Ok(status(id))
            }
            Some(Open::Dir(_)) => Ok(status(id)),
            None => Err(StatusCode::Failure),
        }
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        let file = self.file(&handle)?;
        let data = blocking(move || {
            let mut buf = vec![0u8; len.min(MAX_READ) as usize];
            let n = read_at(&file, &mut buf, offset)?;
            buf.truncate(n);
            Ok(buf)
        })
        .await?;
        if data.is_empty() {
            return Err(StatusCode::Eof);
        }
        Ok(Data { id, data })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        self.writable()?;
        let file = self.file(&handle)?;
        blocking(move || write_all_at(&file, &data, offset)).await?;
        Ok(status(id))
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let real = self.resolve(&path)?;
        let meta = blocking(move || std::fs::symlink_metadata(real)).await?;
        Ok(Attrs {
            id,
            attrs: attrs_of(&meta),
        })
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let real = self.resolve(&path)?;
        let meta = blocking(move || std::fs::metadata(real)).await?;
        Ok(Attrs {
            id,
            attrs: attrs_of(&meta),
        })
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        let file = self.file(&handle)?;
        let meta = blocking(move || file.metadata()).await?;
        Ok(Attrs {
            id,
            attrs: attrs_of(&meta),
        })
    }

    async fn setstat(
        &mut self,
        id: u32,
        path: String,
        attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        self.writable()?;
        let real = self.resolve(&path)?;
        blocking(move || {
            let file = if attrs.size.is_some() {
                OpenOptions::new().write(true).open(&real)?
            } else {
                StdFile::open(&real)?
            };
            apply_attrs(&file, &attrs)
        })
        .await?;
        Ok(status(id))
    }

    async fn fsetstat(
        &mut self,
        id: u32,
        handle: String,
        attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        self.writable()?;
        let file = self.file(&handle)?;
        blocking(move || apply_attrs(&file, &attrs)).await?;
        Ok(status(id))
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        self.log("opendir", &path);
        let real = self.resolve(&path)?;
        let entries = blocking(move || {
            let mut files = Vec::new();
            for entry in std::fs::read_dir(&real)? {
                let entry = entry?;
                // An entry removed while listing is skipped, not an error.
                let Ok(meta) = entry.metadata() else {
                    continue;
                };
                let name = entry.file_name().to_string_lossy().into_owned();
                files.push(File::new(name, attrs_of(&meta)));
            }
            Ok(files)
        })
        .await?;
        let handle = self.add(Open::Dir(entries));
        Ok(Handle { id, handle })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        match self.handles.get_mut(&handle) {
            Some(Open::Dir(entries)) if entries.is_empty() => Err(StatusCode::Eof),
            Some(Open::Dir(entries)) => {
                let take = entries.len().min(DIR_BATCH);
                let files = entries.drain(..take).collect();
                Ok(Name { id, files })
            }
            _ => Err(StatusCode::Failure),
        }
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        self.log("remove", &filename);
        self.writable()?;
        let real = self.resolve_below_root(&filename)?;
        blocking(move || std::fs::remove_file(real)).await?;
        Ok(status(id))
    }

    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        self.log("mkdir", &path);
        self.writable()?;
        let real = self.resolve_below_root(&path)?;
        blocking(move || std::fs::create_dir(real)).await?;
        Ok(status(id))
    }

    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        self.log("rmdir", &path);
        self.writable()?;
        let real = self.resolve_below_root(&path)?;
        blocking(move || std::fs::remove_dir(real)).await?;
        Ok(status(id))
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        Ok(Name {
            id,
            files: vec![File::dummy(Jail::normalize(&path))],
        })
    }

    async fn rename(
        &mut self,
        id: u32,
        oldpath: String,
        newpath: String,
    ) -> Result<Status, Self::Error> {
        self.log("rename", &format!("{oldpath} -> {newpath}"));
        self.writable()?;
        let from = self.resolve_below_root(&oldpath)?;
        let to = self.resolve_below_root(&newpath)?;
        // SFTP v3 rename must not replace an existing file.
        blocking(move || {
            if to.symlink_metadata().is_ok() {
                return Err(io::ErrorKind::AlreadyExists.into());
            }
            std::fs::rename(from, to)
        })
        .await?;
        Ok(status(id))
    }
}
