//! One open libmtp device: listing, reading and writing through it.

use super::*;

#[derive(Clone)]
pub(super) struct Child {
    pub(super) id: u32,
    pub(super) name: String,
    pub(super) dir: bool,
    pub(super) size: u64,
    pub(super) mtime: i64,
}

/// One open libmtp device. Only used behind `Mtp::inner`'s mutex.
pub(super) struct Link {
    pub(super) lib: &'static Lib,
    pub(super) dev: Dev,
    /// False once the device has refused a partial read; ranges then come from a whole copy.
    pub(super) partial: bool,
    /// Set when the device is being let go of: its copies stop (see `progress`).
    pub(super) stop: Arc<AtomicBool>,
}

// The raw device pointer is only touched while holding the mutex in `Mtp`.
unsafe impl Send for Link {}

impl Link {
    /// What `progress` gets as its data: lives as long as the link.
    pub(super) fn stop_flag(&self) -> *const c_void {
        Arc::as_ptr(&self.stop).cast()
    }

    pub(super) fn error(&self, what: &str) -> DevError {
        let mut last = None;
        let mut e = unsafe { (self.lib.errorstack)(self.dev) };
        while !e.is_null() {
            let text = cstr(unsafe { (*e).error_text });
            if !text.is_empty() {
                last = Some(text);
            }
            e = unsafe { (*e).next };
        }
        unsafe { (self.lib.clear_errorstack)(self.dev) };
        DevError::Msg(match last {
            Some(t) => format!("{what} failed: {t}"),
            None => format!("{what} failed."),
        })
    }

    pub(super) fn children(&self, storage: u32, parent: u32) -> Vec<Child> {
        let mut out = Vec::new();
        let mut f = unsafe { (self.lib.files_and_folders)(self.dev, storage, parent) };
        while !f.is_null() {
            // Read everything (including `next`) before freeing the node.
            let (c, next) = unsafe {
                let r = &*f;
                (
                    Child {
                        id: r.item_id,
                        name: cstr(r.filename),
                        dir: r.filetype == FILETYPE_FOLDER,
                        size: r.filesize,
                        mtime: r.modificationdate,
                    },
                    r.next,
                )
            };
            unsafe { (self.lib.destroy_file)(f) };
            out.push(c);
            f = next;
        }
        unsafe { (self.lib.clear_errorstack)(self.dev) };
        out
    }

    pub(super) fn download(&self, id: u32) -> Result<Vec<u8>> {
        let tmp = temp_path("dl");
        let data = self
            .download_to(id, &tmp)
            .and_then(|_| std::fs::read(&tmp).map_err(|e| DevError::Msg(e.to_string())));
        let _ = std::fs::remove_file(&tmp);
        data
    }

    pub(super) fn download_to(&self, id: u32, to: &std::path::Path) -> Result<()> {
        let c = CString::new(to.to_string_lossy().as_bytes()).unwrap();
        let rc = unsafe {
            (self.lib.get_file_to_file)(self.dev, id, c.as_ptr(), Some(progress), self.stop_flag())
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(self.error("Reading the file"))
        }
    }

    /// As `download_to`, also stopping when `cancel` is set.
    pub(super) fn download_until(
        &self,
        id: u32,
        to: &std::path::Path,
        cancel: &AtomicBool,
    ) -> Result<()> {
        let c = CString::new(to.to_string_lossy().as_bytes()).unwrap();
        let flags: [&AtomicBool; 2] = [&self.stop, cancel];
        let data: *const [&AtomicBool; 2] = &flags;
        let rc = unsafe {
            (self.lib.get_file_to_file)(
                self.dev,
                id,
                c.as_ptr(),
                Some(progress_either),
                data.cast(),
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(self.error("Reading the file"))
        }
    }

    /// Part of a file straight from the device, or None when it (or libmtp) can't do that.
    pub(super) fn read_partial(&mut self, id: u32, start: u64, len: u32) -> Option<Vec<u8>> {
        let f = self.lib.get_partial.filter(|_| self.partial)?;
        let (mut data, mut size) = (std::ptr::null_mut::<u8>(), 0u32);
        let rc = unsafe { f(self.dev, id, start, len, &mut data, &mut size) };
        let out = (rc == 0).then(|| {
            if data.is_null() {
                vec![]
            } else {
                unsafe { std::slice::from_raw_parts(data, size as usize) }.to_vec()
            }
        });
        if !data.is_null() {
            unsafe { free(data.cast()) };
        }
        if out.is_none() {
            unsafe { (self.lib.clear_errorstack)(self.dev) };
            self.partial = false;
        }
        out
    }

    pub(super) fn upload(&self, data: &[u8], name: &str, storage: u32, parent: u32) -> Result<u32> {
        let tmp = temp_path("ul");
        std::fs::write(&tmp, data)
            .map_err(|e| DevError::Msg(format!("Can't write temp file: {e}")))?;
        let done = self.upload_file(&tmp, data.len() as u64, name, storage, parent);
        let _ = std::fs::remove_file(&tmp);
        done
    }

    /// Sends file `from` on this computer (`size` bytes) as `name`. Returns its id.
    pub(super) fn upload_file(
        &self,
        from: &std::path::Path,
        size: u64,
        name: &str,
        storage: u32,
        parent: u32,
    ) -> Result<u32> {
        let path = CString::new(from.to_string_lossy().as_bytes())
            .map_err(|_| DevError::Msg("Bad file name.".into()))?;
        let cname = CString::new(name).map_err(|_| DevError::Msg("Bad file name.".into()))?;
        let mut meta = MtpFile {
            item_id: 0,
            parent_id: parent,
            storage_id: storage,
            filename: cname.as_ptr() as *mut c_char,
            filesize: size,
            modificationdate: 0,
            filetype: FILETYPE_UNKNOWN,
            next: std::ptr::null_mut(),
        };
        let rc = unsafe {
            (self.lib.send_file_from_file)(
                self.dev,
                path.as_ptr(),
                &mut meta,
                Some(progress),
                self.stop_flag(),
            )
        };
        if rc != 0 {
            return Err(self.error(&format!("Uploading {name}")));
        }
        Ok(meta.item_id)
    }

    pub(super) fn delete(&self, id: u32) -> Result<()> {
        if unsafe { (self.lib.delete_object)(self.dev, id) } != 0 {
            return Err(self.error("Deleting"));
        }
        Ok(())
    }

    pub(super) fn rename(&self, id: u32, name: &str) -> Result<()> {
        let meta = unsafe { (self.lib.filemetadata)(self.dev, id) };
        if meta.is_null() {
            return Err(self.error("Reading file details"));
        }
        let cname = CString::new(name).map_err(|_| DevError::Msg("Bad file name.".into()))?;
        let rc = unsafe { (self.lib.set_file_name)(self.dev, meta, cname.as_ptr()) };
        unsafe { (self.lib.destroy_file)(meta) };
        if rc != 0 {
            return Err(self.error(&format!("Renaming to {name}")));
        }
        Ok(())
    }

    pub(super) fn mkdir(&self, name: &str, storage: u32, parent: u32) -> Result<u32> {
        // libmtp may rewrite the name in place, so hand it a buffer it owns for the call.
        let raw = CString::new(name)
            .map_err(|_| DevError::Msg("Bad folder name.".into()))?
            .into_raw();
        let id = unsafe { (self.lib.create_folder)(self.dev, raw, parent, storage) };
        drop(unsafe { CString::from_raw(raw) });
        if id == 0 {
            return Err(self.error(&format!("Creating folder {name}")));
        }
        Ok(id)
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        if !self.dev.is_null() {
            let _lib = lib_lock();
            unsafe { (self.lib.release)(self.dev) };
        }
    }
}
