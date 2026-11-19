use std::path::Path;

use crate::config::{ContentConfig, ExclusionConfig};

use super::exclusions::is_network_mount;

/// The content limits that apply under `root`: none on a network mount when
/// `skip_network_content` is set, and none above `max_content_size_bytes`.
/// Applying both here keeps the crawl, the watcher and archive indexing in
/// agreement, since all three read limits only through `content_limit`.
pub fn effective_config(
    content: &ContentConfig,
    excl: &ExclusionConfig,
    root: &Path,
) -> ContentConfig {
    if excl.skip_network_content && is_network_mount(root) {
        return ContentConfig::default();
    }
    content.capped(excl.max_content_size_bytes)
}

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

/// The first `max_bytes` of a file as text, reading no more than that.
/// `None` for a binary or unreadable file. Blocking: for crawl threads.
pub fn read_content(path: &Path, max_bytes: u64) -> Option<String> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len().min(max_bytes);
    let mut buf = Vec::with_capacity(len as usize);
    file.take(max_bytes).read_to_end(&mut buf).ok()?;
    extract_content_from_bytes(&buf)
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
    fn read_content_stops_at_the_limit() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(&b"x".repeat(3 * 1024 * 1024)).unwrap();
        let text = read_content(f.path(), 10).unwrap();
        assert_eq!(text, "x".repeat(10));
        assert!(read_content(Path::new("/no/such/file"), 10).is_none());
    }

    #[test]
    fn cap_lowers_limits_but_keeps_zero_as_skip() {
        let capped = cfg(&[("rs", 524_288), ("md", 1000), ("bin", 0)]).capped(4096);
        assert_eq!(content_limit(Path::new("a.rs"), &capped), Some(4096));
        assert_eq!(content_limit(Path::new("a.md"), &capped), Some(1000));
        assert_eq!(content_limit(Path::new("a.bin"), &capped), None);
        let uncapped = cfg(&[("rs", 524_288)]).capped(0);
        assert_eq!(content_limit(Path::new("a.rs"), &uncapped), Some(524_288));
    }

    #[test]
    fn effective_config_applies_the_global_cap() {
        let excl = ExclusionConfig {
            max_content_size_bytes: 100,
            skip_network_content: true,
            ..Default::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let eff = effective_config(&cfg(&[("rs", 524_288)]), &excl, dir.path());
        assert_eq!(content_limit(Path::new("a.rs"), &eff), Some(100));
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

    #[test]
    fn small_text_file_extracted() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"fn main() {}").unwrap();
        assert_eq!(read_content(f.path(), 4096), Some("fn main() {}".into()));
    }

    #[test]
    fn file_truncated_to_max_bytes() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"abcdefghij").unwrap();
        assert_eq!(read_content(f.path(), 5), Some("abcde".into()));
    }
}
