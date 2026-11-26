//! Where each video or sound was stopped, so it goes on from there next time.

use super::*;

pub(super) const RESUME_KEY: &str = "ferry-files.resume";
/// How many are remembered; the ones played longest ago are forgotten first.
pub(super) const RESUME_KEEP: usize = 300;
/// Stopped before this, or this close to the end: it starts from the beginning next time.
pub(super) const RESUME_EDGE: f64 = 5.0;

pub(in crate::app) fn resume_key(e: &Entry) -> String {
    format!("{}|{}|{}", e.path, e.size, e.mtime)
}

pub(super) fn resumes() -> serde_json::Map<String, serde_json::Value> {
    stored(RESUME_KEY)
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub(super) fn resume_at(key: &str) -> Option<f64> {
    resumes().get(key)?.get(0)?.as_f64()
}

/// Remembers where `m` is, or forgets it when it's near the start or the end.
pub(super) fn remember(key: &str, m: &web_sys::HtmlMediaElement) {
    let (t, d) = (m.current_time(), m.duration());
    if !d.is_finite() || d <= 0.0 {
        return;
    }
    let mut all = resumes();
    if t >= RESUME_EDGE && t < d - RESUME_EDGE {
        all.insert(key.to_string(), json!([t, js_sys::Date::now()]));
        if all.len() > RESUME_KEEP {
            let mut by_age: Vec<(String, f64)> = all
                .iter()
                .map(|(k, v)| (k.clone(), v.get(1).and_then(|s| s.as_f64()).unwrap_or(0.0)))
                .collect();
            by_age.sort_by(|a, b| a.1.total_cmp(&b.1));
            for (k, _) in by_age.into_iter().take(all.len() - RESUME_KEEP) {
                all.remove(&k);
            }
        }
    } else if all.remove(key).is_none() {
        return;
    }
    store(RESUME_KEY, &serde_json::Value::Object(all).to_string());
}

/// Remembering where a video or sound is as it plays: now and then, when it's paused, and when
/// it's closed. Returns the handlers for `loadedmetadata` (going on from where it was stopped,
/// or from `start`), `timeupdate`, `pause` and `ended`.
#[derive(Clone, Copy)]
pub(super) struct Resume {
    key: StoredValue<String>,
    last: StoredValue<f64>,
}

impl Resume {
    pub(super) fn new(
        key: String,
        el: StoredValue<Option<web_sys::HtmlMediaElement>, LocalStorage>,
    ) -> Self {
        let r = Resume {
            key: StoredValue::new(key),
            last: StoredValue::new(0.0),
        };
        let key = r.key;
        on_cleanup(move || {
            if let (Some(Some(m)), Some(k)) = (el.try_get_value(), key.try_get_value()) {
                remember(&k, &m);
            }
        });
        r
    }

    /// Once the length is known: on from `start`, or from where it was stopped. Says where.
    pub(super) fn restore(&self, m: &web_sys::HtmlMediaElement, start: Option<f64>) -> Option<f64> {
        let at = start.or_else(|| resume_at(&self.key.get_value()))?;
        let d = m.duration();
        let at = if d.is_finite() { at.min(d - 1.0) } else { at };
        (at > 0.0).then(|| {
            m.set_current_time(at);
            self.last.set_value(at);
            at
        })
    }

    pub(super) fn tick(&self, m: &web_sys::HtmlMediaElement) {
        if (m.current_time() - self.last.get_value()).abs() >= 5.0 {
            self.save(m);
        }
    }

    pub(super) fn save(&self, m: &web_sys::HtmlMediaElement) {
        self.last.set_value(m.current_time());
        remember(&self.key.get_value(), m);
    }
}

// ---- The keys (keymap.rs) for the preview's video or sound ----
