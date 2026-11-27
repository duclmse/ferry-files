//! The PDF's Info tab: what the PDF says about itself.

use super::*;

/// The Info tab: what the PDF says about itself, the rows it leaves out left out.
pub(super) fn info_list(info: RwSignal<Option<Result<PdfInfo, String>>>) -> impl IntoView {
    move || match info.get() {
        None => view! { <p class="hint pdf-info-note">"Reading…"</p> }.into_any(),
        Some(Err(e)) => {
            view! { <p class="hint pdf-info-note">{format!("Couldn't read its info: {e}")}</p> }
                .into_any()
        }
        Some(Ok(i)) => {
            let rows = info_rows(&i);
            let files = i.attachments.clone();
            view! {
                <dl class="pdf-info">
                    {rows.into_iter().map(|(k, v)| view! { <dt>{k}</dt><dd>{v}</dd> }).collect_view()}
                </dl>
                {(!files.is_empty()).then(|| view! {
                    <div class="pdf-info-files">
                        <div class="pdf-info-head">"Attached files"</div>
                        <ul>{files.into_iter().map(|(name, n)| view! {
                            <li><span class="name">{name}</span><span class="size">{crate::app::util::human_size(n)}</span></li>
                        }).collect_view()}</ul>
                    </div>
                })}
            }
            .into_any()
        }
    }
}

/// The Info tab's rows, as label and value.
pub(super) fn info_rows(i: &PdfInfo) -> Vec<(&'static str, String)> {
    let mut rows = vec![];
    for (k, v) in [
        ("Title", &i.title),
        ("Author", &i.author),
        ("Subject", &i.subject),
        ("Keywords", &i.keywords),
    ] {
        if let Some(v) = v {
            rows.push((k, v.clone()));
        }
    }
    rows.push(("Pages", i.pages.to_string()));
    if let Some([w, h]) = i.page_size {
        rows.push(("Page size", page_size(w as f64, h as f64)));
    }
    for (k, v) in [
        ("Created", &i.created),
        ("Modified", &i.modified),
        ("Made with", &i.creator),
        ("PDF made by", &i.producer),
    ] {
        if let Some(v) = v {
            rows.push((k, v.clone()));
        }
    }
    if let Some(v) = &i.version {
        rows.push(("PDF version", v.clone()));
    }
    rows.push(("Encrypted", if i.encrypted { "Yes" } else { "No" }.into()));
    rows
}

/// A page size in points as millimetres, named when it's a usual paper size (either way up).
pub(super) fn page_size(w: f64, h: f64) -> String {
    const PAPER: [(&str, f64, f64); 7] = [
        ("A3", 297.0, 420.0),
        ("A4", 210.0, 297.0),
        ("A5", 148.0, 210.0),
        ("Letter", 215.9, 279.4),
        ("Legal", 215.9, 355.6),
        ("Tabloid", 279.4, 431.8),
        ("B5", 176.0, 250.0),
    ];
    let mm = |pt: f64| pt * 25.4 / 72.0;
    let (wm, hm) = (mm(w), mm(h));
    let near = |a: f64, b: f64| (a - b).abs() < 2.0;
    let name = PAPER
        .iter()
        .find(|(_, pw, ph)| (near(wm, *pw) && near(hm, *ph)) || (near(wm, *ph) && near(hm, *pw)));
    let size = format!("{:.0} × {:.0} mm", wm, hm);
    match name {
        Some((n, ..)) => format!("{n}, {size}"),
        None => size,
    }
}
