//! Opening an indexed file with the system's default app, for the REST and
//! IPC `open` requests. A file inside a ZIP is extracted to a temp file first.

pub(crate) async fn open_with_os(path: &str) -> Result<(), String> {
    let (prog, args) =
        os_open_command(path).ok_or_else(|| "open not supported on this platform".to_owned())?;
    tokio::process::Command::new(prog)
        .args(args)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(target_os = "macos")]
fn os_open_command(path: &str) -> Option<(&'static str, Vec<&str>)> {
    Some(("open", vec![path]))
}

#[cfg(target_os = "linux")]
fn os_open_command(path: &str) -> Option<(&'static str, Vec<&str>)> {
    Some(("xdg-open", vec![path]))
}

#[cfg(target_os = "windows")]
fn os_open_command(path: &str) -> Option<(&'static str, Vec<&str>)> {
    Some(("cmd", vec!["/C", "start", "", path]))
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn os_open_command(_path: &str) -> Option<(&'static str, Vec<&str>)> {
    None
}

pub(crate) async fn extract_and_open(virtual_path: &str) -> Result<String, String> {
    let temp_path = extract_to_temp(virtual_path).await?;

    let cleanup_path = temp_path.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        let _ = tokio::fs::remove_file(&cleanup_path).await;
    });

    open_with_os(&temp_path).await?;
    Ok(temp_path)
}

/// Extracts a `zip!/entry` (or nested `a.zip!/b.zip!/entry`) virtual path to a
/// kept temp file and returns its path.
async fn extract_to_temp(virtual_path: &str) -> Result<String, String> {
    if !virtual_path.contains("!/") {
        return Err("invalid virtual path".to_owned());
    }
    let virtual_path = virtual_path.to_owned();

    let temp_path = tokio::task::spawn_blocking(move || -> Result<String, String> {
        let basename = virtual_path
            .rsplit(['/', '!'])
            .find(|s| !s.is_empty())
            .unwrap_or("extracted_file");

        let mut tmp = tempfile::Builder::new()
            .prefix("fs_")
            .suffix(&format!("_{basename}"))
            .tempfile()
            .map_err(|e| e.to_string())?;

        crate::indexer::archive::extract_virtual_entry(&virtual_path, &mut tmp)
            .map_err(|e| e.to_string())?;
        let (_, path) = tmp.keep().map_err(|e| e.to_string())?;
        Ok(path.to_string_lossy().into_owned())
    })
    .await
    .map_err(|e| e.to_string())??;

    Ok(temp_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn extract_and_open_no_separator_returns_err() {
        let result = extract_and_open("no-separator-here").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("invalid virtual path"));
    }

    fn write_zip(dir: &std::path::Path) -> String {
        use std::io::Write;
        let path = dir.join("bundle.zip");
        let mut zw = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        zw.start_file("inner/lib.rs", zip::write::SimpleFileOptions::default())
            .unwrap();
        zw.write_all(b"pub fn hello() {}\n").unwrap();
        zw.finish().unwrap();
        path.to_string_lossy().into_owned()
    }

    #[tokio::test]
    async fn extract_to_temp_writes_inner_entry() {
        let dir = tempfile::tempdir().unwrap();
        let zip = write_zip(dir.path());
        let out = extract_to_temp(&format!("{zip}!/inner/lib.rs"))
            .await
            .unwrap();
        assert!(out.ends_with("_lib.rs"), "{out}");
        assert_eq!(std::fs::read(&out).unwrap(), b"pub fn hello() {}\n");
        std::fs::remove_file(&out).unwrap();
    }

    #[tokio::test]
    async fn extract_to_temp_missing_entry_is_err() {
        let dir = tempfile::tempdir().unwrap();
        let zip = write_zip(dir.path());
        let err = extract_to_temp(&format!("{zip}!/nope.txt"))
            .await
            .unwrap_err();
        assert!(err.contains("entry not found"), "{err}");
    }
}
