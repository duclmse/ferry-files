use std::path::Path;

use crate::config::ContentConfig;
use crate::error::{AppError, Result};

/// Returns the maximum bytes to index for this file's extension.
/// `None` means skip content indexing entirely.
pub fn content_limit(path: &Path, cfg: &ContentConfig) -> Option<u64> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase());

    if let Some(ref ext) = ext {
        if let Some(&limit) = cfg.extension_limits.get(ext.as_str()) {
            return if limit == 0 { None } else { Some(limit) };
        }
    }
    if let Some(&limit) = cfg.extension_limits.get("*") {
        return if limit == 0 { None } else { Some(limit) };
    }
    None
}

/// Binary detection: scan first 8 KB. If > 0.1% of bytes are NUL → binary.
pub fn is_binary(buf: &[u8]) -> bool {
    let scan = if buf.len() > 8192 { &buf[..8192] } else { buf };
    if scan.is_empty() {
        return false;
    }
    let null_count = scan.iter().filter(|&&b| b == 0).count();
    null_count * 1000 > scan.len()
}

/// Extract indexable text from raw bytes (used by archive crawler).
/// Returns `None` if binary detected.
pub fn extract_content_from_bytes(buf: &[u8]) -> Option<String> {
    if is_binary(buf) {
        return None;
    }
    if let Some((enc, bom_len)) = encoding_rs::Encoding::for_bom(buf) {
        let (cow, _, _) = enc.decode(&buf[bom_len..]);
        return Some(cow.into_owned());
    }
    let (cow, _, _) = encoding_rs::UTF_8.decode(buf);
    Some(cow.into_owned())
}

/// Extract text content from a file on disk.
/// Files ≤ 1 MB: `tokio::fs::read`. Files > 1 MB: `memmap2`.
/// Returns `None` if the file is binary or content should be skipped.
pub async fn extract_content(path: &Path, max_bytes: u64) -> Result<Option<String>> {
    let meta = tokio::fs::metadata(path).await.map_err(AppError::Io)?;
    let file_size = meta.len();
    let limit = max_bytes as usize;

    let buf: Vec<u8> = if file_size <= 1024 * 1024 {
        let full = tokio::fs::read(path).await.map_err(AppError::Io)?;
        if full.len() > limit {
            full[..limit].to_vec()
        } else {
            full
        }
    } else {
        let path_buf = path.to_path_buf();
        tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
            let file = std::fs::File::open(&path_buf).map_err(AppError::Io)?;
            let mmap = unsafe { memmap2::Mmap::map(&file).map_err(AppError::Io)? };
            let end = limit.min(mmap.len());
            Ok(mmap[..end].to_vec())
        })
        .await
        .map_err(|e| AppError::Content(e.to_string()))??
    };

    if is_binary(&buf) {
        tracing::debug!("binary file: {}", path.display());
        return Ok(None);
    }

    let raw = if let Some((enc, bom_len)) = encoding_rs::Encoding::for_bom(&buf) {
        tracing::debug!("BOM encoding {} for {}", enc.name(), path.display());
        let (cow, _, _) = enc.decode(&buf[bom_len..]);
        cow.into_owned()
    } else {
        tracing::debug!("UTF-8 decode for {}", path.display());
        let (cow, _, _) = encoding_rs::UTF_8.decode(&buf);
        cow.into_owned()
    };

    // Truncate at nearest UTF-8 char boundary
    let content = if raw.len() > limit {
        let mut i = limit;
        while i > 0 && !raw.is_char_boundary(i) {
            i -= 1;
        }
        raw[..i].to_string()
    } else {
        raw
    };

    Ok(Some(content))
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn cfg(map: &[(&str, u64)]) -> ContentConfig {
        ContentConfig {
            extension_limits: map.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
        }
    }

    #[test]
    fn utf8_text_extracted() {
        assert_eq!(
            extract_content_from_bytes(b"hello world"),
            Some("hello world".to_string())
        );
    }

    #[test]
    fn png_binary_header_returns_none() {
        let mut bin = vec![0u8; 100];
        bin[0] = 0x89;
        assert!(is_binary(&bin));
        assert_eq!(extract_content_from_bytes(&bin), None);
    }

    #[test]
    fn content_limit_extension_match() {
        let c = cfg(&[("rs", 1024), ("*", 0)]);
        assert_eq!(content_limit(Path::new("foo.rs"), &c), Some(1024));
    }

    #[test]
    fn content_limit_wildcard_fallback() {
        let c = cfg(&[("*", 512)]);
        assert_eq!(content_limit(Path::new("foo.txt"), &c), Some(512));
    }

    #[test]
    fn content_limit_zero_means_none() {
        let c = cfg(&[("rs", 0)]);
        assert_eq!(content_limit(Path::new("foo.rs"), &c), None);
    }

    #[test]
    fn content_limit_no_match_returns_none() {
        let c = cfg(&[("rs", 1024)]);
        assert_eq!(content_limit(Path::new("foo.txt"), &c), None);
    }

    #[tokio::test]
    async fn small_text_file_extracted() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"fn main() {}").unwrap();
        let result = extract_content(f.path(), 4096).await.unwrap();
        assert_eq!(result, Some("fn main() {}".to_string()));
    }

    #[tokio::test]
    async fn file_truncated_to_max_bytes() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"abcdefghij").unwrap();
        let result = extract_content(f.path(), 5).await.unwrap();
        assert_eq!(result, Some("abcde".to_string()));
    }
}
