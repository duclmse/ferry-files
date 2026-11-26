//! Subtitles: the files beside a video, their languages, which are shown, and ones dropped on
//! the player.

use super::*;

/// A subtitle file loaded for a video: its name, and its cues as WebVTT (the backend turns
/// .srt and .sub files into that).
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub(super) name: String,
    pub(super) vtt: String,
}

/// The extensions of subtitle files.
pub(super) const SUB_EXTS: &[&str] = &["srt", "sub", "vtt"];

pub(super) fn is_subtitle(path: &str) -> bool {
    path.rsplit_once('.')
        .is_some_and(|(_, ext)| SUB_EXTS.iter().any(|e| ext.eq_ignore_ascii_case(e)))
}

/// How many subtitles show at once.
pub(super) const SHOWN_MAX: usize = 2;

/// Languages by their two-letter code, with the other ways file names (and Settings) give them.
pub(super) const LANGS: &[(&str, &[&str])] = &[
    ("en", &["eng", "english"]),
    ("vi", &["vie", "vietnamese"]),
    ("fr", &["fre", "fra", "french", "francais"]),
    ("de", &["ger", "deu", "german", "deutsch"]),
    ("es", &["spa", "spanish", "espanol"]),
    ("it", &["ita", "italian"]),
    ("pt", &["por", "portuguese"]),
    ("ru", &["rus", "russian"]),
    ("ja", &["jpn", "japanese", "jp"]),
    ("zh", &["chi", "zho", "chinese", "chs", "cht"]),
    ("ko", &["kor", "korean"]),
    ("ar", &["ara", "arabic"]),
    ("hi", &["hin", "hindi"]),
    ("th", &["tha", "thai"]),
    ("id", &["ind", "indonesian"]),
    ("nl", &["dut", "nld", "dutch"]),
    ("pl", &["pol", "polish"]),
    ("tr", &["tur", "turkish"]),
    ("sv", &["swe", "swedish"]),
    ("uk", &["ukr", "ukrainian"]),
];

/// A language as its two-letter code where it's known ("English", "eng", "en-US": "en"), else
/// as it's written, in lower case. None when it's not a language's (a year, a word).
pub(super) fn lang_code(s: &str) -> Option<String> {
    let s = s.trim().to_lowercase();
    let base = s.split('-').next().unwrap_or("");
    if let Some((code, _)) = LANGS
        .iter()
        .find(|(c, other)| *c == base || other.contains(&base))
    {
        return Some(code.to_string());
    }
    (matches!(base.len(), 2 | 3) && base.chars().all(|c| c.is_ascii_alphabetic()))
        .then(|| base.to_string())
}

/// The language a subtitle file's name gives after the video's name: "Movie.en.srt",
/// "Movie_English.srt", "Movie.en.forced.srt".
pub(super) fn track_lang(name: &str) -> Option<String> {
    let stem = name.rsplit_once('.').map_or(name, |(s, _)| s);
    let parts: Vec<&str> = stem.split(['.', '_', ' ']).collect();
    // The first part is the video's name.
    parts.iter().skip(1).rev().take(2).find_map(|p| {
        let code = lang_code(p)?;
        // A short word that isn't a known language counts only as the last part.
        (LANGS.iter().any(|(c, _)| *c == code) || Some(p) == parts.last()).then_some(code)
    })
}

/// The languages typed in Settings ("en, vi"), up to `SHOWN_MAX`.
pub(super) fn pref_langs(s: &str) -> Vec<String> {
    s.split([',', ' ', ';'])
        .filter_map(lang_code)
        .take(SHOWN_MAX)
        .collect()
}

/// Which of the subtitle files `names` show at first: one in each of the languages `prefs`, in
/// that order; none of them there, the first file.
pub(super) fn default_shown(names: &[String], prefs: &[String]) -> Vec<usize> {
    let langs: Vec<Option<String>> = names.iter().map(|n| track_lang(n)).collect();
    let mut out = vec![];
    for p in prefs {
        if let Some(i) =
            (0..names.len()).find(|i| langs[*i].as_ref() == Some(p) && !out.contains(i))
        {
            out.push(i);
        }
    }
    if out.is_empty() && !names.is_empty() {
        out.push(0);
    }
    out
}

/// The cues of `vtt` placed at the top of the picture (the second subtitles shown), unless a
/// cue says where it goes.
pub(super) fn at_top(vtt: &str) -> String {
    let mut out = String::with_capacity(vtt.len() + vtt.len() / 8);
    for line in vtt.lines() {
        out.push_str(line);
        if line.contains("-->") && !line.contains("line:") {
            out.push_str(" line:0");
        }
        out.push('\n');
    }
    out
}

#[derive(Deserialize)]
pub(super) struct SubFile {
    pub(super) path: String,
}

/// Reads the subtitle file at `path` on `src`.
pub(super) async fn fetch_track(src: Src, path: String) -> Result<Track, String> {
    let vtt = call::<String>("subtitle", json!({ "src": src.key(), "path": path })).await?;
    let name = path.rsplit('/').next().unwrap_or(&path).to_string();
    Ok(Track { name, vtt })
}

pub(super) type DropHandler = Rc<dyn Fn(Src, Vec<String>)>;

thread_local! {
    /// What each player does with subtitle files dropped on it: (player, handler).
    pub(super) static DROPS: RefCell<Vec<(u64, DropHandler)>> = const { RefCell::new(Vec::new()) };
}

/// Hands the subtitle files among `paths` (on `src`) to the player `el` is in. False when it
/// isn't in one, or none of them are subtitles.
pub(in crate::app) fn drop_on_player(el: &web_sys::Element, src: Src, paths: Vec<String>) -> bool {
    let Some(id) = el
        .closest(".vplayer[data-player]")
        .ok()
        .flatten()
        .and_then(|p| p.get_attribute("data-player")?.parse::<u64>().ok())
    else {
        return false;
    };
    let subs: Vec<String> = paths.into_iter().filter(|p| is_subtitle(p)).collect();
    let Some(f) = DROPS.with(|d| {
        d.borrow()
            .iter()
            .find(|(i, _)| *i == id)
            .map(|(_, f)| f.clone())
    }) else {
        return false;
    };
    if subs.is_empty() {
        return false;
    }
    f(src, subs);
    true
}

pub(super) fn next_player() -> u64 {
    NEXT_PLAYER.with(|n| {
        n.set(n.get() + 1);
        n.get()
    })
}

/// Shows `t`'s cues on `track` ("showing"), or hides them ("disabled").
pub(super) fn track_mode(track: &web_sys::Element, mode: &str) {
    if let Ok(t) = js_sys::Reflect::get(track, &"track".into()) {
        let _ = js_sys::Reflect::set(&t, &"mode".into(), &mode.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knows_subtitle_files() {
        assert!(is_subtitle("/a/Movie.en.SRT"));
        assert!(is_subtitle("b.sub"));
        assert!(is_subtitle("c.vtt"));
        assert!(!is_subtitle("d.mp4"));
        assert!(!is_subtitle("srt"));
    }

    #[test]
    fn finds_languages_in_names() {
        assert_eq!(track_lang("Movie.en.srt").as_deref(), Some("en"));
        assert_eq!(track_lang("Movie_English.srt").as_deref(), Some("en"));
        assert_eq!(track_lang("Movie.vie.forced.srt").as_deref(), Some("vi"));
        assert_eq!(track_lang("Movie.pt-BR.srt").as_deref(), Some("pt"));
        assert_eq!(track_lang("Movie.srt"), None);
        assert_eq!(track_lang("Movie.2020.srt"), None);
        assert_eq!(pref_langs("English, vi, fr"), ["en", "vi"]);
    }

    #[test]
    fn shows_the_languages_picked_first() {
        let names: Vec<String> = ["Movie.en.srt", "Movie.fr.srt", "Movie.vi.srt"]
            .map(String::from)
            .to_vec();
        assert_eq!(default_shown(&names, &["vi".into(), "en".into()]), [2, 0]);
        assert_eq!(default_shown(&names, &["de".into(), "fr".into()]), [1]);
        assert_eq!(default_shown(&names, &["de".into()]), [0]);
        assert_eq!(default_shown(&names, &[]), [0]);
        assert!(default_shown(&[], &["en".into()]).is_empty());
    }

    #[test]
    fn puts_the_second_subtitles_at_the_top() {
        assert_eq!(
            at_top("WEBVTT\n\n00:01.000 --> 00:02.000\nHi\n"),
            "WEBVTT\n\n00:01.000 --> 00:02.000 line:0\nHi\n"
        );
        assert_eq!(
            at_top("00:01.000 --> 00:02.000 line:3\n"),
            "00:01.000 --> 00:02.000 line:3\n"
        );
    }
}
