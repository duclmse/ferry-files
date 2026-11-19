//! The terminal under a pane: type a command, it runs in the pane's folder on the pane's device
//! (this computer, a phone over adb, an SFTP server over ssh) and what it printed shows above.
//! One command at a time, without input: not for programs that ask questions or draw the screen.
//! `cd` moves the pane, so the files and the terminal stay in the same folder.

use super::*;
use crate::api::Ran;

/// A command run in the terminal, and what it printed (None while it runs).
#[derive(Clone)]
pub(super) struct TermLine {
    pub dir: String,
    pub cmd: String,
    pub ran: Option<Result<Ran, String>>,
}

/// Commands kept for ↑/↓.
const HISTORY_MAX: usize = 100;

/// Where `cd arg` goes from `cwd`: an absolute path, ~ (`home`), .. and names, one at a time.
pub(super) fn cd_target(cwd: &str, arg: &str, home: &str) -> String {
    let arg = arg.trim().trim_matches(|c| c == '"' || c == '\'');
    let (mut at, rest) = match arg {
        "" | "~" => return home.to_string(),
        a if a.starts_with("~/") => (home.to_string(), &a[2..]),
        a if a.starts_with('/') => ("/".to_string(), &a[1..]),
        a => (cwd.to_string(), a),
    };
    for part in rest.split('/') {
        match part {
            "" | "." => {}
            ".." => at = parent(&at),
            name => at = join(&at, name),
        }
    }
    at
}

impl Ctx {
    /// Whether `src` has a command line: this computer, a phone over adb, an SFTP server.
    pub(super) fn has_shell(self, src: Src) -> bool {
        src == Src::Mac || self.src_info(src).is_some_and(|i| matches!(i.transport.as_str(), "adb" | "sftp"))
    }

    /// Shows or hides the terminal under the pane.
    pub(super) fn toggle_terminal(self, p: Pane) {
        if !self.has_shell(p.src.get_untracked()) {
            p.error("This device has no command line. The terminal works on this computer, phones connected with adb, and SFTP servers.");
            return;
        }
        p.term.update(|t| *t = !*t);
    }

    /// Runs `text` in the pane's folder; `cd` and `clear` are done here.
    fn run_in_term(self, p: Pane, text: String) {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        p.term_hist.update_value(|h| {
            h.retain(|x| *x != text);
            h.push(text.clone());
            if h.len() > HISTORY_MAX {
                h.remove(0);
            }
        });
        let (src, dir) = (p.src.get_untracked(), p.cwd.get_untracked());
        if text == "clear" {
            p.term_log.set(vec![]);
            return;
        }
        if text == "cd" || text.starts_with("cd ") {
            let home = if src == Src::Mac { self.mac.get_untracked().map(|i| i.default_dir) } else { self.src_info(src).map(|i| i.default_dir) };
            let to = cd_target(&dir, text.strip_prefix("cd").unwrap_or_default(), &home.unwrap_or_else(|| "/".into()));
            p.term_log.update(|l| l.push(TermLine { dir, cmd: text, ran: None }));
            let at = p.term_log.with_untracked(Vec::len) - 1;
            // Listed like any folder the pane goes to; it says when there's no such folder.
            self.load_select(p, src, to, None);
            p.term_log.update(|l| l[at].ran = Some(Ok(Ran { out: String::new(), code: 0, timed_out: false })));
            return;
        }
        p.term_log.update(|l| l.push(TermLine { dir: dir.clone(), cmd: text.clone(), ran: None }));
        let at = p.term_log.with_untracked(Vec::len) - 1;
        spawn_local(async move {
            let r = call::<Ran>("run_command", json!({ "src": src.key(), "dir": dir, "command": text })).await;
            let changed = r.is_ok();
            p.term_log.update(|l| {
                if let Some(line) = l.get_mut(at) {
                    line.ran = Some(r);
                }
            });
            // The command may have made, changed or deleted files here.
            if changed && p.src.get_untracked() == src && p.cwd.get_untracked() == dir {
                self.reload(p);
            }
        });
    }
}

#[component]
pub(super) fn Terminal(pane: Pane) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let p = pane;
    let text = RwSignal::new(String::new());
    // Where ↑/↓ are in the history (its length: a new command).
    let back = StoredValue::new(0usize);
    let input = NodeRef::<html::Input>::new();
    let log = NodeRef::<html::Div>::new();
    input.on_load(move |el| {
        request_animation_frame(move || {
            let _ = el.focus();
        });
    });
    // Kept scrolled to the latest output.
    Effect::new(move |_| {
        p.term_log.track();
        if let Some(el) = log.get() {
            request_animation_frame(move || el.set_scroll_top(el.scroll_height()));
        }
    });
    let running = move || p.term_log.with(|l| l.last().is_some_and(|x| x.ran.is_none()));
    let place = move || match p.src.get() {
        Src::Mac => ctx.mac.get().map(|i| i.label).unwrap_or_default(),
        src => ctx.src_info(src).map(|i| i.label).unwrap_or_default(),
    };
    let keydown = move |e: web_sys::KeyboardEvent| {
        e.stop_propagation();
        match e.key().as_str() {
            "Enter" if !running() => {
                ctx.run_in_term(p, text.get_untracked());
                text.set(String::new());
                back.set_value(p.term_hist.with_value(Vec::len));
            }
            "ArrowUp" | "ArrowDown" => {
                e.prevent_default();
                let n = p.term_hist.with_value(Vec::len);
                let at = back.get_value().min(n);
                let to = if e.key() == "ArrowUp" { at.saturating_sub(1) } else { (at + 1).min(n) };
                back.set_value(to);
                text.set(p.term_hist.with_value(|h| h.get(to).cloned()).unwrap_or_default());
            }
            "Escape" => p.term.set(false),
            _ => {}
        }
    };
    view! {
        <div class="term" on:mousedown=move |e| e.stop_propagation() on:click=move |_| ctx.active.set(p.id)>
            <div class="term-head">
                <span class="where">{move || format!("{} · {}", place(), p.cwd.get())}</span>
                <button class="btn icon-only small" title="Clear" aria-label="Clear the terminal" on:click=move |_| p.term_log.set(vec![])>{ico(Icon::Trash)}</button>
                <button class="btn icon-only small" title="Close the terminal" aria-label="Close the terminal" on:click=move |_| p.term.set(false)>{ico(Icon::Close)}</button>
            </div>
            <div class="term-log" node_ref=log>
                {move || p.term_log.with(Vec::is_empty).then(|| view! {
                    <p class="hint">"Commands run in this folder, one at a time, and can't ask questions. cd moves the pane; ↑ brings back earlier commands."</p>
                })}
                {move || p.term_log.get().into_iter().map(|line| view! {
                    <div class="cmd"><span class="dir">{base(&line.dir)}</span>" $ "{line.cmd}</div>
                    {match line.ran {
                        None => view! { <div class="running"><span class="spin"></span>"Running…"</div> }.into_any(),
                        Some(Err(e)) => view! { <pre class="out error">{e}</pre> }.into_any(),
                        Some(Ok(r)) => view! {
                            {(!r.out.trim_end().is_empty()).then(|| view! { <pre class="out">{r.out.trim_end().to_string()}</pre> })}
                            {r.timed_out.then(|| view! { <div class="code error">"Stopped after 2 minutes."</div> })}
                            {(r.code != 0 && !r.timed_out).then(|| view! { <div class="code error">{format!("Exit code {}", r.code)}</div> })}
                        }.into_any(),
                    }}
                }).collect_view()}
            </div>
            <div class="term-input">
                <span class="prompt" aria-hidden="true">"$"</span>
                <input node_ref=input type="text" spellcheck="false" autocomplete="off" aria-label="Command"
                    placeholder=move || if running() { "Running…" } else { "Type a command and press Enter" }
                    prop:value=move || text.get()
                    on:input=move |e| text.set(event_target_value(&e))
                    on:keydown=keydown/>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cd_targets() {
        assert_eq!(cd_target("/a/b", "..", "/home"), "/a");
        assert_eq!(cd_target("/a/b", "c/../d", "/home"), "/a/b/d");
        assert_eq!(cd_target("/a/b", "/x", "/home"), "/x");
        assert_eq!(cd_target("/a/b", "", "/home"), "/home");
        assert_eq!(cd_target("/a/b", "~/docs", "/home"), "/home/docs");
        assert_eq!(cd_target("/", "..", "/home"), "/");
    }
}
