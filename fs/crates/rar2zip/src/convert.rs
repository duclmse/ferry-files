use std::fs::File;
use std::io::{BufWriter, Cursor, Read, Write};
use std::path::{Path, PathBuf};

use zip::write::FileOptions;
use zip::CompressionMethod;

use crate::config::ConversionConfig;
use crate::error::{AppError, Result};
use crate::queue::Job;

const CHUNK: usize = 65536;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Convert `job.file_path` to a `.zip` in the same directory and return the
/// zip path. Runs in `spawn_blocking`; does NOT delete any source files.
pub async fn convert(job: &Job, config: &ConversionConfig) -> Result<PathBuf> {
    let src = job.file_path.clone();
    // Replace only the final extension: foo.tar.gz → foo.tar.zip is wrong,
    // so we replace the entire file_name rather than using with_extension().
    let zip_name = zip_filename(&src);
    let dst = src
        .parent()
        .unwrap_or(Path::new("."))
        .join(zip_name);
    let cfg     = config.clone();
    let dst_ret = dst.clone();

    tokio::task::spawn_blocking(move || convert_to_file(&src, &dst, &cfg))
        .await
        .map_err(|e| AppError::Conversion(e.to_string()))??;

    Ok(dst_ret)
}

/// Convert `src` archive into zip bytes held in memory. Used by the Stream
/// strategy (no zip file written to disk). Runs synchronously in
/// `spawn_blocking` context.
pub fn convert_to_bytes(src: &Path, cfg: &ConversionConfig) -> Result<Vec<u8>> {
    let cursor  = Cursor::new(Vec::new());
    let mut zip = zip::ZipWriter::new(cursor);
    let result  = fill_zip(src, &mut zip, cfg);
    if result.is_err() {
        return result.map(|_| Vec::new());
    }
    let cursor = zip
        .finish()
        .map_err(|e| AppError::Conversion(e.to_string()))?;
    Ok(cursor.into_inner())
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn zip_filename(src: &Path) -> String {
    let name = src
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("archive");
    // Strip known multi-part suffixes before adding .zip
    let lower = name.to_ascii_lowercase();
    let stem = [".tar.gz", ".tar.bz2", ".part1.rar"]
        .iter()
        .find(|&&sfx| lower.ends_with(sfx))
        .map(|sfx| &name[..name.len() - sfx.len()])
        .or_else(|| {
            Path::new(name)
                .file_stem()
                .and_then(|s| s.to_str())
        })
        .unwrap_or(name);
    format!("{stem}.zip")
}

fn convert_to_file(src: &Path, dst: &Path, cfg: &ConversionConfig) -> Result<()> {
    let file    = File::create(dst).map_err(AppError::Io)?;
    let buf     = BufWriter::with_capacity(256 * 1024, file);
    let mut zip = zip::ZipWriter::new(buf);
    let result  = fill_zip(src, &mut zip, cfg);
    if result.is_err() {
        drop(zip);
        let _ = std::fs::remove_file(dst);
        return result;
    }
    zip.finish().map_err(|e| AppError::Conversion(e.to_string()))?;
    Ok(())
}

fn fill_zip<W: Write + std::io::Seek>(
    src: &Path,
    zip: &mut zip::ZipWriter<W>,
    cfg: &ConversionConfig,
) -> Result<()> {
    let name  = src.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let lower = name.to_ascii_lowercase();

    if lower.ends_with(".rar") || lower.ends_with(".part1.rar") {
        fill_from_rar(src, zip, cfg)
    } else if lower.ends_with(".7z") {
        fill_from_7z(src, zip, cfg)
    } else if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        let f = File::open(src).map_err(AppError::Io)?;
        fill_from_tar(flate2::read::GzDecoder::new(f), zip, cfg)
    } else if lower.ends_with(".tar.bz2") {
        let f = File::open(src).map_err(AppError::Io)?;
        fill_from_tar(bzip2::read::BzDecoder::new(f), zip, cfg)
    } else if lower.ends_with(".tar") {
        let f = File::open(src).map_err(AppError::Io)?;
        fill_from_tar(f, zip, cfg)
    } else {
        Err(AppError::Conversion(format!("unsupported archive format: {name}")))
    }
}

// ---------------------------------------------------------------------------
// RAR → ZIP
// ---------------------------------------------------------------------------

fn fill_from_rar<W: Write + std::io::Seek>(
    src: &Path,
    zip: &mut zip::ZipWriter<W>,
    cfg: &ConversionConfig,
) -> Result<()> {
    // Integrity check: iterate all headers without extracting
    verify_rar(src)?;

    let stored   = zip_opts(CompressionMethod::Stored, 0);
    let deflated = zip_opts(CompressionMethod::Deflated, cfg.compression_level as i64);

    // Inline open to avoid naming the complex OpenArchive<Process, CursorBeforeHeader> type
    let mut arch = if let Some(pass) = cfg.archive_password.as_deref() {
        unrar::Archive::with_password(src, pass)
            .open_for_processing()
            .map_err(|e: unrar::error::UnrarError| AppError::Conversion(e.to_string()))?
    } else {
        unrar::Archive::new(src)
            .open_for_processing()
            .map_err(|e: unrar::error::UnrarError| AppError::Conversion(e.to_string()))?
    };

    loop {
        match arch.read_header() {
            Ok(None) => break,
            Ok(Some(header)) => {
                // Extract metadata before consuming `header` with skip()/read()
                let entry  = header.entry();
                let name   = entry.filename.to_string_lossy().replace('\\', "/");
                let is_dir = entry.is_directory();
                // method 0x30 == "stored" in the RAR header encoding
                let method = entry.method;

                if is_dir {
                    zip.add_directory(&name, stored)
                        .map_err(|e| AppError::Conversion(e.to_string()))?;
                    arch = header
                        .skip()
                        .map_err(|e: unrar::error::UnrarError| AppError::Conversion(e.to_string()))?;
                } else {
                    let entry_opts = if method == 0x30 { stored } else { deflated };
                    zip.start_file(&name, entry_opts)
                        .map_err(|e| AppError::Conversion(e.to_string()))?;
                    // read() returns (Vec<u8>, next_archive), not Cursor
                    let (bytes, next) = header
                        .read()
                        .map_err(|e: unrar::error::UnrarError| AppError::Conversion(e.to_string()))?;
                    let mut cursor = Cursor::new(bytes);
                    copy_chunked(&mut cursor, zip)?;
                    arch = next;
                }
            }
            Err(e) => return Err(AppError::Conversion(e.to_string())),
        }
    }
    Ok(())
}

fn verify_rar(src: &Path) -> Result<()> {
    let mut a = unrar::Archive::new(src)
        .open_for_processing()
        .map_err(|e: unrar::error::UnrarError| {
            AppError::Conversion(format!("integrity check failed: {e}"))
        })?;
    loop {
        match a.read_header() {
            Ok(None) => break,
            Ok(Some(h)) => {
                a = h.skip().map_err(|e: unrar::error::UnrarError| {
                    AppError::Conversion(format!("integrity check failed: {e}"))
                })?;
            }
            Err(e) => return Err(AppError::Conversion(format!("integrity check failed: {e}"))),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 7Z → ZIP
// ---------------------------------------------------------------------------

fn fill_from_7z<W: Write + std::io::Seek>(
    src: &Path,
    zip: &mut zip::ZipWriter<W>,
    cfg: &ConversionConfig,
) -> Result<()> {
    let stored   = zip_opts(CompressionMethod::Stored, 0);
    let deflated = zip_opts(CompressionMethod::Deflated, cfg.compression_level as i64);
    let mut zip_err: Option<AppError> = None;

    let f   = File::open(src).map_err(AppError::Io)?;
    let len = f.metadata().map_err(AppError::Io)?.len();
    let mut reader = sevenz_rust::SevenZReader::new(f, len, sevenz_rust::Password::empty())
        .map_err(|e| AppError::Conversion(format!("integrity check failed: {e}")))?;

    reader
        .for_each_entries(&mut |entry: &sevenz_rust::SevenZArchiveEntry, reader: &mut dyn std::io::Read| {
            if zip_err.is_some() {
                return Ok(false);
            }
            let name = entry.name().replace('\\', "/");
            if entry.is_directory() {
                if let Err(e) = zip.add_directory(&name, stored) {
                    zip_err = Some(AppError::Conversion(e.to_string()));
                    return Ok(false);
                }
            } else {
                if let Err(e) = zip.start_file(&name, deflated) {
                    zip_err = Some(AppError::Conversion(e.to_string()));
                    return Ok(false);
                }
                let mut buf = [0u8; CHUNK];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if let Err(e) = zip.write_all(&buf[..n]) {
                                zip_err = Some(AppError::Io(e));
                                return Ok(false);
                            }
                        }
                        Err(e) => {
                            zip_err = Some(AppError::Io(e));
                            return Ok(false);
                        }
                    }
                }
            }
            Ok(true)
        })
        .map_err(|e| AppError::Conversion(e.to_string()))?;

    if let Some(e) = zip_err {
        return Err(e);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// TAR (plain / gz / bz2) → ZIP
// ---------------------------------------------------------------------------

fn fill_from_tar<R: Read, W: Write + std::io::Seek>(
    reader: R,
    zip:    &mut zip::ZipWriter<W>,
    cfg:    &ConversionConfig,
) -> Result<()> {
    let stored   = zip_opts(CompressionMethod::Stored, 0);
    let deflated = zip_opts(CompressionMethod::Deflated, cfg.compression_level as i64);

    let mut archive = tar::Archive::new(reader);
    let entries = archive
        .entries()
        .map_err(|e| AppError::Conversion(format!("integrity check failed: {e}")))?;

    for entry_result in entries {
        let mut entry     = entry_result.map_err(AppError::Io)?;
        let path          = entry.path().map_err(AppError::Io)?.into_owned();
        let name          = path.to_string_lossy().replace('\\', "/");
        let file_type     = entry.header().entry_type();

        if file_type.is_dir() {
            zip.add_directory(&name, stored)
                .map_err(|e| AppError::Conversion(e.to_string()))?;
        } else if file_type.is_file() {
            zip.start_file(&name, deflated)
                .map_err(|e| AppError::Conversion(e.to_string()))?;
            copy_chunked(&mut entry, zip)?;
        }
        // Skip symlinks, hard links, and other special types.
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn copy_chunked<R: Read, W: Write>(src: &mut R, dst: &mut W) -> Result<()> {
    let mut buf = [0u8; CHUNK];
    loop {
        let n = src.read(&mut buf).map_err(AppError::Io)?;
        if n == 0 {
            break;
        }
        dst.write_all(&buf[..n]).map_err(AppError::Io)?;
    }
    Ok(())
}

fn zip_opts(method: CompressionMethod, level: i64) -> FileOptions<'static, ()> {
    let base = FileOptions::default().compression_method(method);
    if method == CompressionMethod::Stored {
        base
    } else {
        base.compression_level(Some(level))
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zip_filename_strips_tar_gz() {
        assert_eq!(zip_filename(Path::new("/w/archive.tar.gz")), "archive.zip");
    }

    #[test]
    fn zip_filename_strips_tar_bz2() {
        assert_eq!(zip_filename(Path::new("/w/archive.tar.bz2")), "archive.zip");
    }

    #[test]
    fn zip_filename_strips_part1_rar() {
        assert_eq!(zip_filename(Path::new("/w/movie.part1.rar")), "movie.zip");
    }

    #[test]
    fn zip_filename_plain_rar() {
        assert_eq!(zip_filename(Path::new("/w/file.rar")), "file.zip");
    }

    #[test]
    fn zip_filename_7z() {
        assert_eq!(zip_filename(Path::new("/w/data.7z")), "data.zip");
    }

    #[test]
    fn unsupported_format_returns_error() {
        let cfg = ConversionConfig::default();
        let src = Path::new("/tmp/file.zip");
        let result = convert_to_bytes(src, &cfg);
        assert!(result.is_err());
    }
}
