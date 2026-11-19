//! Renaming several items at once: find and replace, a name pattern with numbering and dates, and
//! case. A preview shows each new name and anything wrong with it before anything is renamed.

use super::*;

/// The case the new names end up in.
const CASES: [&str; 4] = ["Keep as is", "lower case", "UPPER CASE", "Title Case"];

/// How to make the new names.
#[derive(Clone, Default)]
struct Rule {
    find: String,
    replace: String,
    /// `find` is a regular expression (JavaScript's), and `replace` may use $1, $2…
    pattern: bool,
    match_case: bool,
    /// The new name before its extension; {name}, {n}, {n:3} and {date} are filled in.
    template: String,
    start: i64,
    case: usize,
}

/// One item's new name, or what's wrong with it.
#[derive(Clone, PartialEq)]
struct Row {
    entry: Entry,
    new: String,
    problem: Option<String>,
}

/// Find, as a JavaScript regular expression, or why it isn't one.
fn regex(r: &Rule) -> Result<Option<js_sys::RegExp>, String> {
    if r.find.is_empty() {
        return Ok(None);
    }
    let source = if r.pattern { r.find.clone() } else { escape(&r.find) };
    js_regex(&source, if r.match_case { "g" } else { "gi" }).map(Some)
}

/// `s` with regular expression characters escaped.
fn escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if r"\^$.|?*+()[]{}/-".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// "photo.jpg" as ("photo", ".jpg"); folders and names starting with a dot keep it all.
fn split_ext(name: &str, dir: bool) -> (String, String) {
    match name.rfind('.') {
        Some(i) if !dir && i > 0 => (name[..i].to_string(), name[i..].to_string()),
        _ => (name.to_string(), String::new()),
    }
}

/// The date `mtime` (seconds) as 2024-05-31.
fn day(mtime: i64) -> String {
    when(mtime).split(' ').next().unwrap_or_default().to_string()
}

/// `template` with {name}, {n}, {n:3} and {date} filled in; anything else in braces stays.
fn fill(template: &str, name: &str, n: i64, mtime: i64) -> String {
    let mut out = String::new();
    let mut rest = template;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let Some(j) = after.find('}') else {
            out.push_str(&rest[i..]);
            return out;
        };
        let token = &after[..j];
        match token {
            "name" => out.push_str(name),
            "n" => out.push_str(&n.to_string()),
            "date" => out.push_str(&day(mtime)),
            _ => match token.strip_prefix("n:").and_then(|w| w.parse::<usize>().ok()) {
                Some(w) if w <= 12 => out.push_str(&format!("{n:0w$}")),
                _ => out.push_str(&rest[i..i + j + 2]),
            },
        }
        rest = &after[j + 1..];
    }
    out.push_str(rest);
    out
}

fn recase(s: &str, case: usize) -> String {
    match case {
        1 => s.to_lowercase(),
        2 => s.to_uppercase(),
        3 => {
            let mut out = String::new();
            let mut start = true;
            for c in s.chars() {
                if start {
                    out.extend(c.to_uppercase());
                } else {
                    out.extend(c.to_lowercase());
                }
                start = !c.is_alphanumeric() && c != '\'';
            }
            out
        }
        _ => s.to_string(),
    }
}

/// What's wrong with `name` as a name, if anything.
fn bad_name(name: &str) -> Option<String> {
    if name.trim().is_empty() {
        Some("No name".into())
    } else if name.contains('/') {
        Some("Has a /".into())
    } else if name == "." || name == ".." {
        Some("Not a name".into())
    } else if name.len() > 255 {
        Some("Too long".into())
    } else if name != name.trim() {
        Some("Starts or ends with a space".into())
    } else {
        None
    }
}

/// The new names for `items` (in order), with problems marked. `taken` holds, by folder, the
/// lower-cased names of what else is there.
fn plan(items: &[Entry], r: &Rule, taken: &HashMap<String, HashSet<String>>) -> Result<Vec<Row>, String> {
    let re = regex(r)?;
    let replacement = if r.pattern { r.replace.clone() } else { r.replace.replace('$', "$$") };
    let template = if r.template.trim().is_empty() { "{name}" } else { r.template.as_str() };
    let mut rows: Vec<Row> = items
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let name = match &re {
                Some(re) => String::from(js_sys::JsString::from(e.name.as_str()).replace_by_pattern(re, &replacement)),
                None => e.name.clone(),
            };
            let (stem, ext) = split_ext(&name, e.dir);
            let new = recase(&format!("{}{ext}", fill(template, &stem, r.start + i as i64, e.mtime)), r.case);
            Row { entry: e.clone(), problem: bad_name(&new), new }
        })
        .collect();
    // Two items ending up with the same name, or one taking the name of something else there.
    let mut seen: HashMap<(String, String), usize> = HashMap::new();
    for row in &rows {
        *seen.entry((parent(&row.entry.path), row.new.to_lowercase())).or_default() += 1;
    }
    for row in &mut rows {
        if row.problem.is_some() || row.new == row.entry.name {
            continue;
        }
        let dir = parent(&row.entry.path);
        let lower = row.new.to_lowercase();
        if seen.get(&(dir.clone(), lower.clone())).copied().unwrap_or(0) > 1 {
            row.problem = Some("Same as another new name".into());
        } else if taken.get(&dir).is_some_and(|t| t.contains(&lower)) {
            row.problem = Some("Already there".into());
        }
    }
    Ok(rows)
}

/// Renames each (path, new name) on `src`, in an order where no item takes a name another still
/// has; items swapping names go by way of a temporary name. Returns what was renamed as (path
/// now, name before), and the error that stopped it, if one did.
pub(super) async fn rename_all(src: Src, items: Vec<(String, String)>) -> (Vec<(String, String)>, Option<String>) {
    // (path now, new name, name before)
    let mut pending: Vec<(String, String, String)> =
        items.into_iter().filter(|(path, new)| base(path) != *new).map(|(path, new)| (path.clone(), new, base(&path))).collect();
    let mut done = vec![];
    let mut aside = 0;
    while !pending.is_empty() {
        let free = pending.iter().position(|(path, new, _)| {
            let target = join(&parent(path), new).to_lowercase();
            !pending.iter().any(|(other, _, _)| other != path && other.to_lowercase() == target)
        });
        let (i, name) = match free {
            Some(i) => (i, pending[i].1.clone()),
            None => {
                // A ring of names: one steps aside first.
                aside += 1;
                (0, format!("{}.renaming-{aside}", pending[0].2))
            }
        };
        let path = pending[i].0.clone();
        match call::<String>("rename", json!({ "src": src.key(), "path": path, "name": name })).await {
            Ok(now) if free.is_some() => {
                let (_, _, before) = pending.remove(i);
                done.push((now, before));
            }
            Ok(now) => pending[i].0 = now,
            Err(e) => {
                let note = if base(&path) != pending[i].2 { format!(" ({} is called {} for now.)", pending[i].2, base(&path)) } else { String::new() };
                return (done, Some(format!("{e}{note}")));
            }
        }
    }
    (done, None)
}

/// New names for `items` with what `find` matches (any case; a regular expression with `pattern`,
/// where `replace` may use $1…) replaced, as (item, new name, what's wrong with it).
pub(super) fn replace_plan(items: &[Entry], find: &str, replace: &str, pattern: bool, taken: &HashMap<String, HashSet<String>>) -> Result<Vec<(Entry, String, Option<String>)>, String> {
    let r = Rule { find: find.into(), replace: replace.into(), pattern, template: "{name}".into(), start: 1, ..Default::default() };
    Ok(plan(items, &r, taken)?.into_iter().map(|x| (x.entry, x.new, x.problem)).collect())
}

impl Ctx {
    /// After renaming: the undo entry, the renamed items marked, their folders reloaded, and a
    /// notice. Returns the error to show, if renaming stopped on one.
    pub(super) fn renamed(self, p: Pane, src: Src, done: Vec<(String, String)>, err: Option<String>) -> Option<String> {
        if !done.is_empty() {
            let what = match done.as_slice() {
                [(now, before)] => format!("Renamed {before} to {}", base(now)),
                _ => format!("Renamed {} items", done.len()),
            };
            self.log_undo(format!("{what} in {}", self.place(src)), Undo::Renames { src, items: done.clone() });
            p.marked.set(done.iter().map(|(now, _)| now.clone()).collect());
            let mut dirs: Vec<String> = done.iter().map(|(now, _)| parent(now)).collect();
            dirs.sort();
            dirs.dedup();
            for d in dirs {
                self.changed(src, &d);
            }
        }
        match err {
            None => {
                p.notice.set(Notice::Ok(format!("Renamed {} item{}.", done.len(), if done.len() == 1 { "" } else { "s" })));
                None
            }
            Some(e) if done.is_empty() => Some(e),
            Some(e) => Some(format!("Renamed {}, then: {e}", done.len())),
        }
    }

    /// Find and replace in names: in what's picked when that's several items, else in everything
    /// the pane shows.
    pub(super) fn replace_names(self, p: Pane) {
        let picked = p.picked();
        let items = if picked.len() > 1 { picked } else { p.entries.get_untracked() };
        if !items.is_empty() && self.renamable(p) {
            self.panel.set(Some(Panel::Rename(p.id, items)));
        }
    }

    /// Whether the pane's items can be renamed: not at the top of an MTP phone.
    pub(super) fn renamable(self, p: Pane) -> bool {
        !(p.cwd.get_untracked() == "/" && self.src_info(p.src.get_untracked()).is_some_and(|i| i.transport == "mtp"))
    }

    /// F2 or Rename… on several items: the batch rename dialog.
    pub(super) fn ask_rename(self, p: Pane) {
        let items = p.picked();
        match items.as_slice() {
            [] => {}
            [one] => p.renaming.set(Some(one.path.clone())),
            _ => self.panel.set(Some(Panel::Rename(p.id, items))),
        }
    }
}

#[component]
pub(super) fn BatchRename(pane: usize, items: Vec<Entry>) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let p = ctx.panes.into_iter().find(|p| p.id == pane).unwrap_or_else(|| ctx.act());
    let src = p.src.get_untracked();
    let items = StoredValue::new(items);
    let rule = RwSignal::new(Rule { template: "{name}".into(), start: 1, ..Default::default() });
    let set = move |f: fn(&mut Rule, String)| move |ev: leptos::ev::Event| rule.update(|r| f(r, event_target_value(&ev)));
    let check = move |f: fn(&mut Rule, bool)| move |ev: leptos::ev::Event| rule.update(|r| f(r, event_target_checked(&ev)));
    let error = RwSignal::new(None::<String>);
    let working = RwSignal::new(false);

    // What else is in each folder, for marking names already taken.
    let taken = RwSignal::new(None::<HashMap<String, HashSet<String>>>);
    spawn_local(async move {
        let picked: HashSet<String> = items.with_value(|v| v.iter().map(|e| e.path.clone()).collect());
        let mut dirs: Vec<String> = items.with_value(|v| v.iter().map(|e| parent(&e.path)).collect());
        dirs.sort();
        dirs.dedup();
        let mut map = HashMap::new();
        for dir in dirs {
            match call::<Vec<Entry>>("list", json!({ "src": src.key(), "path": dir, "current": false })).await {
                Ok(list) => {
                    let names = list.into_iter().filter(|e| !picked.contains(&e.path)).map(|e| e.name.to_lowercase()).collect();
                    map.insert(dir, names);
                }
                Err(e) => error.set(Some(e)),
            }
        }
        taken.set(Some(map));
    });

    let rows = Memo::new(move |_| {
        let r = rule.get();
        taken.with(|t| t.as_ref().map(|t| items.with_value(|v| plan(v, &r, t))))
    });
    let changes = move || rows.with(|r| match r {
        Some(Ok(rows)) => rows.iter().filter(|x| x.new != x.entry.name).count(),
        _ => 0,
    });
    let problems = move || rows.with(|r| match r {
        Some(Ok(rows)) => rows.iter().filter(|x| x.problem.is_some()).count(),
        _ => 0,
    });

    // Something to rename, and nothing wrong.
    let ready = move || changes() != 0 && problems() == 0;
    let close = move || ctx.panel.set(None);
    let apply = move || {
        let Some(Ok(list)) = rows.get_untracked() else { return };
        if list.iter().any(|x| x.problem.is_some()) {
            return;
        }
        let plan: Vec<(String, String)> = list.iter().map(|x| (x.entry.path.clone(), x.new.clone())).collect();
        working.set(true);
        error.set(None);
        spawn_local(async move {
            let (done, err) = rename_all(src, plan).await;
            working.set(false);
            match ctx.renamed(p, src, done, err) {
                None => close(),
                Some(e) => error.set(Some(e)),
            }
        });
    };

    let tokens = "{name} is the name (after Find), {n} counts up from Start ({n:3} gives 001), {date} is the date modified. The extension stays.";
    view! {
        <div class="overlay" on:click=move |_| if !working.get_untracked() { close() }>
            <div class="dialog wide batch-rename" on:click=|e| e.stop_propagation()
                on:keydown=move |e| match e.key().as_str() {
                    "Escape" => close(),
                    "Enter" if e.target().and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok()).is_some() => apply(),
                    _ => {}
                }>
                <h2>{format!("Rename {} items", items.with_value(Vec::len))}</h2>
                <div class="rename-form">
                    <label>"Find"</label>
                    <input type="text" spellcheck="false" autocomplete="off" prop:value=move || rule.with(|r| r.find.clone())
                        on:input=set(|r, v| r.find = v) autofocus/>
                    <label>"Replace with"</label>
                    <input type="text" spellcheck="false" autocomplete="off" prop:value=move || rule.with(|r| r.replace.clone())
                        on:input=set(|r, v| r.replace = v)/>
                    <span></span>
                    <span class="checks">
                        <label class="row"><input type="checkbox" prop:checked=move || rule.with(|r| r.pattern) on:change=check(|r, v| r.pattern = v)/>"Pattern (regular expression; $1 in Replace)"</label>
                        <label class="row"><input type="checkbox" prop:checked=move || rule.with(|r| r.match_case) on:change=check(|r, v| r.match_case = v)/>"Match case"</label>
                    </span>
                    <label>"New name"</label>
                    <input type="text" spellcheck="false" autocomplete="off" prop:value=move || rule.with(|r| r.template.clone())
                        on:input=set(|r, v| r.template = v)/>
                    <span></span>
                    <span class="hint">{tokens}</span>
                    <label>"Start"</label>
                    <span class="checks">
                        <input class="start" type="number" prop:value=move || rule.with(|r| r.start.to_string())
                            on:input=set(|r, v| r.start = v.trim().parse().unwrap_or(1))/>
                        <label class="row">"Case"
                            <select on:change=set(|r, v| r.case = v.parse().unwrap_or(0))>
                                {CASES.iter().enumerate().map(|(i, c)| view! { <option value=i.to_string() selected=move || rule.with(|r| r.case == i)>{*c}</option> }).collect_view()}
                            </select>
                        </label>
                    </span>
                </div>
                {move || error.get().map(|e| view! { <p class="error">{e}</p> })}
                <div class="rename-preview">
                    {move || match rows.get() {
                        None => view! { <p class="hint">"Checking the folder…"</p> }.into_any(),
                        Some(Err(e)) => view! { <p class="error">{e}</p> }.into_any(),
                        Some(Ok(rows)) => view! {
                            <table>
                                <thead><tr><th>"Now"</th><th>"New name"</th><th></th></tr></thead>
                                <tbody>
                                    {rows.into_iter().map(|x| {
                                        let same = x.new == x.entry.name;
                                        let bad = x.problem.is_some();
                                        view! {
                                            <tr class:same=same class:bad=bad>
                                                <td>{x.entry.name}</td>
                                                <td>{x.new}</td>
                                                <td class="why">{x.problem.unwrap_or_else(|| if same { "No change".into() } else { String::new() })}</td>
                                            </tr>
                                        }
                                    }).collect_view()}
                                </tbody>
                            </table>
                        }.into_any(),
                    }}
                </div>
                <div class="actions">
                    {move || (problems() > 0).then(|| view! { <span class="hint">{format!("Fix the {} marked name{} first.", problems(), if problems() == 1 { "" } else { "s" })}</span> })}
                    <span class="grow"></span>
                    <button class="btn" disabled=move || working.get() on:click=move |_| close()>"Cancel"</button>
                    <button class="btn primary" disabled=move || working.get() || !ready()
                        on:click=move |_| apply()>{move || if working.get() { "Renaming…".to_string() } else { format!("Rename {}", changes()) }}</button>
                </div>
            </div>
        </div>
    }
}
