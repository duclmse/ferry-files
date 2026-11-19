use std::collections::HashMap;
use std::sync::atomic::Ordering;

use crate::metrics::Metrics;
use crate::queue::Job;

pub fn render(
    queue_depth: &HashMap<String, i64>,
    recent_jobs: &[Job],
    metrics: &Metrics,
) -> String {
    let pending = queue_depth.get("pending").copied().unwrap_or(0);
    let converting = queue_depth.get("converting").copied().unwrap_or(0);
    let transferring = queue_depth.get("transferring").copied().unwrap_or(0);
    let done = queue_depth.get("done").copied().unwrap_or(0);
    let failed = queue_depth.get("failed").copied().unwrap_or(0);

    let fp = metrics.files_processed.load(Ordering::Relaxed);
    let bt = metrics.bytes_transferred.load(Ordering::Relaxed);
    let cf = metrics.conversion_failures.load(Ordering::Relaxed);
    let tf = metrics.transfer_failures.load(Ordering::Relaxed);

    let rows: String = recent_jobs
        .iter()
        .map(|j| {
            let name = j
                .file_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("-");
            let id_short = &j.id[..8.min(j.id.len())];
            let error_short = j
                .error
                .as_deref()
                .map(|e| html_escape(truncate_chars(e, 80)))
                .unwrap_or_default();
            let (badge_bg, badge_fg) = status_badge(j.status.as_str());
            format!(
                "<tr>\
                 <td style='font-family:monospace'>{id_short}</td>\
                 <td>{}</td>\
                 <td><span style='background:{badge_bg};color:{badge_fg};\
                      padding:2px 6px;border-radius:3px'>{}</span></td>\
                 <td style='text-align:right'>{}</td>\
                 <td style='font-family:monospace'>{}</td>\
                 <td style='color:#888'>{error_short}</td>\
                 </tr>",
                html_escape(name),
                j.status.as_str(),
                j.retry_count,
                j.updated_at,
            )
        })
        .collect();

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>rar2zip</title>
<style>
  body {{font-family:sans-serif;margin:2rem;color:#222;}}
  h2   {{margin-bottom:.25rem;}}
  h3   {{margin-top:1.5rem;margin-bottom:.5rem;}}
  table{{border-collapse:collapse;width:100%;margin-bottom:.5rem;}}
  th,td{{border:1px solid #ddd;padding:5px 10px;text-align:left;}}
  th   {{background:#f5f5f5;font-weight:600;}}
  tr:nth-child(even){{background:#fafafa;}}
</style>
</head>
<body>
<h2>rar2zip dashboard</h2>
<h3>Queue Stats</h3>
<table>
<tr><th>Status</th><th>Count</th></tr>
<tr><td>pending</td><td>{pending}</td></tr>
<tr><td>converting</td><td>{converting}</td></tr>
<tr><td>transferring</td><td>{transferring}</td></tr>
<tr><td><span style='color:#28a745'>done</span></td><td>{done}</td></tr>
<tr><td><span style='color:#dc3545'>failed</span></td><td>{failed}</td></tr>
</table>
<h3>Transfer Stats</h3>
<table>
<tr><th>Metric</th><th>Value</th></tr>
<tr><td>files processed</td><td>{fp}</td></tr>
<tr><td>bytes transferred</td><td>{bt}</td></tr>
<tr><td>conversion failures</td><td>{cf}</td></tr>
<tr><td>transfer failures</td><td>{tf}</td></tr>
</table>
<h3>Recent Jobs (last 20)</h3>
<table>
<tr><th>ID</th><th>File</th><th>Status</th><th>Retries</th><th>Updated</th><th>Error</th></tr>
{rows}
</table>
<script>setTimeout(()=>location.reload(),5000);</script>
</body>
</html>"#
    )
}

fn status_badge(status: &str) -> (&'static str, &'static str) {
    match status {
        "done" => ("#28a745", "#fff"),
        "failed" => ("#dc3545", "#fff"),
        "converting" => ("#ffc107", "#000"),
        "transferring" => ("#fd7e14", "#fff"),
        _ => ("#6c757d", "#fff"),
    }
}

/// Error text often names a file, so slicing by bytes could split a
/// character and panic the handler.
fn truncate_chars(s: &str, max: usize) -> &str {
    s.char_indices().nth(max).map_or(s, |(at, _)| &s[..at])
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::{Status, Strategy};
    use std::path::PathBuf;

    fn job(name: &str, status: Status, error: Option<&str>) -> Job {
        Job {
            id: "0123456789abcdef".into(),
            file_path: PathBuf::from("/watch").join(name),
            file_hash: String::new(),
            status,
            strategy: Strategy::Disk,
            destinations: vec![],
            retry_count: 2,
            created_at: 0,
            updated_at: 1_700_000_000,
            error: error.map(str::to_owned),
        }
    }

    #[test]
    fn shows_counts_metrics_and_jobs_and_refreshes_itself() {
        let depth = HashMap::from([("pending".to_owned(), 3), ("failed".to_owned(), 1)]);
        let metrics = Metrics::new();
        metrics.files_processed.store(42, Ordering::Relaxed);
        let html = render(
            &depth,
            &[job("<b>.rar", Status::Failed, Some("bad \"crc\""))],
            &metrics,
        );
        assert!(html.contains("<tr><td>pending</td><td>3</td></tr>"));
        assert!(html.contains("<tr><td>converting</td><td>0</td></tr>"));
        assert!(html.contains("<td>files processed</td><td>42</td>"));
        assert!(html.contains("&lt;b&gt;.rar"));
        assert!(!html.contains("<b>.rar"));
        assert!(html.contains("bad &quot;crc&quot;"));
        assert!(html.contains(">01234567</td>"));
        assert!(html.contains("location.reload()"));
    }

    #[test]
    fn long_non_ascii_errors_are_cut_on_a_character() {
        let error = format!("open x {}: denied", "tệp".repeat(40));
        assert!(!error.is_char_boundary(80));
        let html = render(
            &HashMap::new(),
            &[job("a.rar", Status::Failed, Some(&error))],
            &Metrics::new(),
        );
        let cut: String = error.chars().take(80).collect();
        assert!(html.contains(&format!(">{cut}</td>")));
    }
}
