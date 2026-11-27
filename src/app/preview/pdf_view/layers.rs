//! What's laid over a drawn PDF page: its text, to select and search, and its links.

use super::*;

/// How wide `t` is in the text over a PDF page (its font, `.pdf-text`) at 100px.
pub(super) fn run_width(t: &str) -> f64 {
    thread_local! {
        static PEN: Option<web_sys::CanvasRenderingContext2d> = document()
            .create_element("canvas")
            .ok()
            .and_then(|c| c.dyn_into::<web_sys::HtmlCanvasElement>().ok())
            .and_then(|c| c.get_context("2d").ok().flatten())
            .and_then(|c| c.dyn_into().ok())
            .inspect(|c: &web_sys::CanvasRenderingContext2d| c.set_font("100px sans-serif"));
    }
    PEN.with(|p| {
        p.as_ref()
            .and_then(|p| p.measure_text(t).ok())
            .map_or(0.0, |m| m.width())
    })
}

/// A page's text laid over it, transparent: each run in its box, its font as tall as the box and
/// stretched to its width, so what's selected is what's under it. `aspect`: the page's width over
/// its height, as the box is in fractions of each.
pub(super) fn pdf_text_layer(runs: &[PdfRun], aspect: f64) -> impl IntoView {
    let spans = runs
        .iter()
        .map(|r| {
            let [x, y, w, h] = r.b;
            let m = run_width(&r.t);
            let k = if m > 0.0 && h > 0.0 {
                w * 100.0 * aspect / (m * h)
            } else {
                1.0
            };
            let at = format!(
                "left: {}%; top: {}%; font-size: calc(var(--ih) * {h}); transform: scaleX({k})",
                x * 100.0,
                y * 100.0
            );
            view! { <span style=at>{format!("{}{}", r.t, r.e)}</span> }
        })
        .collect_view();
    view! { <div class="pdf-text">{spans}</div> }
}

/// A page's links over its text: one goes to its page (`go`), or opens in the browser or mail app.
pub(super) fn pdf_link_layer(
    links: &[PdfLink],
    go: impl Fn(usize) + Copy + 'static,
) -> impl IntoView {
    links
        .iter()
        .map(|l| {
            let [x, y, w, h] = l.b;
            let at = format!("left: {}%; top: {}%; width: {}%; height: {}%", x * 100.0, y * 100.0, w * 100.0, h * 100.0);
            let (page, uri) = (l.page, l.uri.clone());
            let tip = match (&page, &uri) {
                (Some(n), _) => format!("Page {n}"),
                (_, Some(u)) => u.clone(),
                _ => String::new(),
            };
            let click = move |ev: web_sys::MouseEvent| {
                ev.prevent_default();
                if let Some(n) = page {
                    go(n);
                } else if let Some(u) = uri.clone() {
                    spawn_local(async move {
                        if let Err(e) = call::<()>("open_link", json!({ "url": u })).await {
                            web_sys::console::warn_1(&e.into());
                        }
                    });
                }
            };
            view! { <a class="pdf-link" href="#" style=at title=tip.clone() aria-label=tip on:click=click></a> }
        })
        .collect_view()
}
