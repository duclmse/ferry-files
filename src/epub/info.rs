//! The book's info: title, authors and the other metadata, and the cover.

use super::*;

fn field(
    form: RwSignal<Meta>,
    label: &'static str,
    wide: bool,
    rows: u32,
    get: fn(&Meta) -> String,
    set: fn(&mut Meta, String),
) -> impl IntoView {
    let value = move || form.with(get);
    let input = move |ev| form.update(|m| set(m, event_target_value(&ev)));
    view! {
        <label class="field" class:wide=wide>
            <span>{label}</span>
            {if rows > 1 {
                view! { <textarea rows=rows prop:value=value on:input=input></textarea> }.into_any()
            } else {
                view! { <input prop:value=value on:input=input/> }.into_any()
            }}
        </label>
    }
}

fn lines(v: &[String]) -> String {
    v.join("\n")
}

/// Keeps every line, even empty ones, so typing Enter in the box isn't undone. The backend drops blanks.
fn split_lines(s: String) -> Vec<String> {
    if s.is_empty() {
        vec![]
    } else {
        s.split('\n').map(String::from).collect()
    }
}

#[component]
pub(super) fn InfoView() -> impl IntoView {
    let ep = expect_context::<Ep>();
    let saved = move || {
        ep.book
            .with_untracked(|b| b.as_ref().map(|b| b.meta.clone()).unwrap_or_default())
    };
    let form = RwSignal::new(saved());
    // When the book's info changes (after Apply), show the cleaned-up values.
    Effect::new(move |prev: Option<Meta>| {
        let m = ep
            .book
            .with(|b| b.as_ref().map(|b| b.meta.clone()).unwrap_or_default());
        if prev.as_ref().is_some_and(|p| *p != m) {
            form.set(m.clone());
        }
        m
    });
    let changed = move || {
        ep.book
            .with(|b| b.as_ref().is_some_and(|b| b.meta != form.get()))
    };
    let book = move || ep.book.get();
    view! {
        <div class="scroll">
            <form class="card" on:submit=move |e| { e.prevent_default(); ep.set_meta(form.get_untracked()); }>
                <h2>"Book info"</h2>
                <p class="sub">{move || { ep.book.track(); format!("Changes stay in the app until you save the book to {}.", ep.place()) }}</p>
                <div class="grid">
                    {field(form, "Title *", true, 1, |m| m.title.clone(), |m, v| m.title = v)}
                    {field(form, "Authors (one per line)", true, 2, |m| lines(&m.creators), |m, v| m.creators = split_lines(v))}
                    // In pairs, a line each: identifier and language, publisher and date.
                    {field(form, "Identifier * (ISBN, UUID…)", false, 1, |m| m.identifier.clone(), |m, v| m.identifier = v)}
                    {field(form, "Language * (en, vi, ja…)", false, 1, |m| m.language.clone(), |m, v| m.language = v)}
                    {field(form, "Publisher", false, 1, |m| m.publisher.clone(), |m, v| m.publisher = v)}
                    {field(form, "Publication date (YYYY-MM-DD)", false, 1, |m| m.date.clone(), |m, v| m.date = v)}
                    {field(form, "Subjects (one per line)", true, 2, |m| lines(&m.subjects), |m, v| m.subjects = split_lines(v))}
                    {field(form, "Description", true, 4, |m| m.description.clone(), |m, v| m.description = v)}
                </div>
                <div class="actions">
                    <span class="spacer"></span>
                    <button type="button" class="btn" title="Discard changes" aria-label="Discard changes" disabled=move || !changed()
                        on:click=move |_| form.set(saved())>{ico(Icon::Restore)}</button>
                    <button type="submit" class="btn primary" title="Apply" aria-label="Apply" disabled=move || !changed() || ep.busy.get()>{ico(Icon::Check)}</button>
                </div>
            </form>
            <div class="card">
                <h2>"Cover"</h2>
                <div class="cover-row">
                    {move || match book().and_then(|b| b.cover) {
                        Some(url) => view! { <img class="cover" src=url alt="Book cover"/> }.into_any(),
                        None => view! { <div class="cover none">"No cover image"</div> }.into_any(),
                    }}
                    <div>
                        <p class="sub">{format!("Pick an image on {} (JPEG, PNG, GIF, WebP or SVG).", crate::os::the_computer())}</p>
                        <button class="btn" disabled=move || ep.busy.get() on:click=move |_| ep.cover()>
                            {move || if book().is_some_and(|b| b.cover.is_some()) { "Replace cover…" } else { "Add a cover…" }}
                        </button>
                    </div>
                </div>
            </div>
            <div class="card">
                <h2>"Package"</h2>
                {move || book().map(|b| {
                    let s = b.stats;
                    view! {
                        <dl class="stats">
                            <dt>"EPUB version"</dt><dd>{if s.version.is_empty() { "unknown".to_string() } else { s.version }}</dd>
                            <dt>"Chapters"</dt><dd>{s.chapters}</dd>
                            <dt>"Files"</dt><dd>{format!("{} ({})", s.files, human_size(s.bytes))}</dd>
                            <dt>"Images"</dt><dd>{s.images}</dd>
                            <dt>"Stylesheets"</dt><dd>{s.styles}</dd>
                            <dt>"Fonts"</dt><dd>{s.fonts}</dd>
                            <dt>{format!("On {}", ep.place())}</dt><dd class="path">{b.path}</dd>
                        </dl>
                    }
                })}
            </div>
        </div>
    }
}
