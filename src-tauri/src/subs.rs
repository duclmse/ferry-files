//! Subtitles for the video player: finding the files beside a video with a name like it, and
//! turning SubRip (.srt), MicroDVD and SubViewer (.sub) files into WebVTT, which the web view
//! shows. VobSub's .sub holds pictures, not text, and isn't shown.

/// The extensions of subtitle files.
pub const EXTS: &[&str] = &["srt", "sub", "vtt"];

/// Subtitle files bigger than this aren't read.
pub const MAX: u64 = 8 * 1024 * 1024;

/// The frame rate a MicroDVD file is taken to have when it doesn't say.
const FPS: f64 = 23.976;

/// Whether `name` is a subtitle file.
pub fn is_subtitle(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(_, ext)| EXTS.iter().any(|e| ext.eq_ignore_ascii_case(e)))
}

/// Whether `name` is a subtitle file for the video `video`: the video's name without its
/// extension, then the extension or more after a separator ("Movie.srt", "Movie.en.srt",
/// "Movie_en.srt"), ignoring case.
pub fn goes_with(video: &str, name: &str) -> bool {
    if !is_subtitle(name) {
        return false;
    }
    let stem = video.rsplit_once('.').map_or(video, |(s, _)| s).to_lowercase();
    let name = name.to_lowercase();
    name.strip_prefix(&stem).and_then(|rest| rest.chars().next()).is_some_and(|c| matches!(c, '.' | '_' | '-' | ' '))
}

/// The text of a subtitle file: UTF-8 (with or without a BOM), UTF-16 with a BOM, or else
/// Windows-1252, which older files often are.
pub fn decode(data: &[u8]) -> String {
    if let Some(rest) = data.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    let utf16 = |rest: &[u8], le: bool| {
        let units: Vec<u16> = rest.chunks_exact(2).map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) }).collect();
        String::from_utf16_lossy(&units)
    };
    if let Some(rest) = data.strip_prefix(&[0xFF, 0xFE]) {
        return utf16(rest, true);
    }
    if let Some(rest) = data.strip_prefix(&[0xFE, 0xFF]) {
        return utf16(rest, false);
    }
    match std::str::from_utf8(data) {
        Ok(s) => s.to_string(),
        Err(_) => data.iter().map(|&b| cp1252(b)).collect(),
    }
}

/// A Windows-1252 byte as a character.
fn cp1252(b: u8) -> char {
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8D}', 'Ž', '\u{8F}',
        '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9D}', 'ž', 'Ÿ',
    ];
    match b {
        0x80..=0x9F => HIGH[(b - 0x80) as usize],
        _ => b as char,
    }
}

/// The subtitle file `name` (its bytes `data`) as WebVTT.
pub fn to_vtt(data: &[u8], name: &str) -> Result<String, String> {
    if data.starts_with(&[0, 0, 1, 0xBA]) {
        return Err("This .sub file holds pictures (VobSub), which can't be shown.".into());
    }
    let text = decode(data).replace("\r\n", "\n").replace('\r', "\n");
    let text = text.trim_start_matches('\u{FEFF}');
    if text.starts_with("WEBVTT") {
        return Ok(text.to_string());
    }
    let cues = if text.trim_start().starts_with('{') {
        microdvd(text)
    } else if name.to_lowercase().ends_with(".sub") && !text.contains("-->") {
        subviewer(text)
    } else {
        srt(text)
    };
    if cues.is_empty() {
        return Err("No subtitles found in this file.".into());
    }
    let mut out = String::from("WEBVTT\n\n");
    for (start, end, text) in cues {
        out.push_str(&format!("{} --> {}\n{}\n\n", stamp(start), stamp(end), text.trim_end()));
    }
    Ok(out)
}

/// A cue: start and end in milliseconds, and its text (lines with tags VTT knows).
type Cue = (u64, u64, String);

/// Milliseconds as "00:01:02.345".
fn stamp(ms: u64) -> String {
    format!("{:02}:{:02}:{:02}.{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
}

/// "01:02:03,456" (or "." before the fraction, a short fraction, or no hours) in milliseconds.
fn time(s: &str) -> Option<u64> {
    let s = s.trim();
    let (hms, frac) = match s.rfind([',', '.']) {
        Some(i) => (&s[..i], &s[i + 1..]),
        None => (s, ""),
    };
    let parts: Vec<u64> = hms.split(':').map(|p| p.trim().parse().ok()).collect::<Option<_>>()?;
    let secs = match parts[..] {
        [h, m, s] => h * 3600 + m * 60 + s,
        [m, s] => m * 60 + s,
        _ => return None,
    };
    let frac: String = frac.chars().take_while(char::is_ascii_digit).take(3).collect();
    let ms = if frac.is_empty() { 0 } else { frac.parse::<u64>().ok()? * 10u64.pow(3 - frac.len() as u32) };
    Some(secs * 1000 + ms)
}

/// SubRip: numbered blocks of "start --> end" and lines of text.
fn srt(text: &str) -> Vec<Cue> {
    let mut cues = vec![];
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let Some((a, b)) = line.split_once("-->") else { continue };
        // Text after the end time (positions) is left out.
        let b = b.split_whitespace().next().unwrap_or("");
        let (Some(start), Some(end)) = (time(a), time(b)) else { continue };
        let mut body = vec![];
        while let Some(l) = lines.peek() {
            if l.trim().is_empty() {
                break;
            }
            body.push(clean(l));
            lines.next();
        }
        cues.push((start, end, body.join("\n")));
    }
    cues
}

/// MicroDVD: "{start}{end}text" in frames, with "|" between lines. A first line of
/// "{1}{1}23.976" gives the frame rate.
fn microdvd(text: &str) -> Vec<Cue> {
    let mut fps = FPS;
    let mut cues = vec![];
    for (i, line) in text.lines().enumerate() {
        let Some((a, rest)) = frames(line) else { continue };
        let Some((b, body)) = frames(rest) else { continue };
        if i == 0 && a <= 1 && b <= 1 {
            if let Some(f) = body.trim().parse::<f64>().ok().filter(|f| *f > 1.0 && *f < 200.0) {
                fps = f;
                continue;
            }
        }
        let ms = |f: u64| (f as f64 * 1000.0 / fps).round() as u64;
        let body: Vec<String> = body.split('|').map(|l| clean(&strip_braces(l))).collect();
        cues.push((ms(a), ms(b), body.join("\n")));
    }
    cues
}

/// "{123}" at the start of `s`: the number, and what follows.
fn frames(s: &str) -> Option<(u64, &str)> {
    let rest = s.trim_start().strip_prefix('{')?;
    let (n, rest) = rest.split_once('}')?;
    Some((n.trim().parse().ok()?, rest))
}

/// SubViewer: a line "00:00:01.00,00:00:03.00", then the text, with "[br]" between lines.
fn subviewer(text: &str) -> Vec<Cue> {
    let mut cues = vec![];
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let Some((a, b)) = line.split_once(',') else { continue };
        let (Some(start), Some(end)) = (time(a), time(b)) else { continue };
        if !a.contains(':') || !b.contains(':') {
            continue;
        }
        let mut body = vec![];
        for l in lines.by_ref() {
            if l.trim().is_empty() {
                break;
            }
            body.extend(l.split("[br]").flat_map(|p| p.split('|')).map(clean));
        }
        cues.push((start, end, body.join("\n")));
    }
    cues
}

/// Takes out MicroDVD's style codes ("{y:i}").
fn strip_braces(s: &str) -> String {
    let mut out = String::new();
    let mut depth = 0;
    for c in s.chars() {
        match c {
            '{' => depth += 1,
            '}' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// A line of text for VTT: the tags it knows (<i>, <b>, <u>) kept, others (<font>) and ASS
/// codes ("{\an8}") taken out, and "&" and stray "<" written so they show.
fn clean(line: &str) -> String {
    let line = if line.contains("{\\") { strip_braces(line) } else { line.to_string() };
    let mut out = String::new();
    let mut rest = line.as_str();
    while let Some(i) = rest.find(['<', '&']) {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        if rest.starts_with('&') {
            out.push_str("&amp;");
            rest = &rest[1..];
            continue;
        }
        match rest.find('>') {
            Some(j) => {
                let tag = rest[1..j].trim().to_lowercase();
                if matches!(tag.as_str(), "i" | "b" | "u" | "/i" | "/b" | "/u") {
                    out.push_str(&format!("<{tag}>"));
                }
                rest = &rest[j + 1..];
            }
            None => {
                out.push_str("&lt;");
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_files_named_like_the_video() {
        assert!(goes_with("Movie.mp4", "Movie.srt"));
        assert!(goes_with("Movie.mp4", "movie.EN.srt"));
        assert!(goes_with("Movie.2020.mkv", "Movie.2020_en.sub"));
        assert!(goes_with("Movie.mp4", "Movie.vtt"));
        assert!(!goes_with("Movie.mp4", "Movie2.srt"));
        assert!(!goes_with("Movie.mp4", "Movie.txt"));
        assert!(!goes_with("Movie.mp4", "Other.srt"));
    }

    #[test]
    fn turns_srt_into_vtt() {
        let srt = "\u{FEFF}1\r\n00:00:01,500 --> 00:00:03,000 X1:0\r\n<i>Hello</i> & <font color=\"red\">you</font>\r\n{\\an8}Two\r\n\r\n2\r\n01:02:03,4 --> 01:02:04,000\r\na < b\r\n";
        let vtt = to_vtt(srt.as_bytes(), "a.srt").unwrap();
        assert_eq!(
            vtt,
            "WEBVTT\n\n00:00:01.500 --> 00:00:03.000\n<i>Hello</i> &amp; you\nTwo\n\n01:02:03.400 --> 01:02:04.000\na &lt; b\n\n"
        );
    }

    #[test]
    fn turns_microdvd_into_vtt() {
        let sub = "{1}{1}25\n{25}{50}{y:i}One|Two\n{100}{125}Three\n";
        assert_eq!(
            to_vtt(sub.as_bytes(), "a.sub").unwrap(),
            "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nOne\nTwo\n\n00:00:04.000 --> 00:00:05.000\nThree\n\n"
        );
        // No frame rate given.
        assert!(to_vtt(b"{0}{24}Hi", "a.sub").unwrap().contains("00:00:00.000 --> 00:00:01.001\nHi"));
    }

    #[test]
    fn turns_subviewer_into_vtt() {
        let sub = "[INFORMATION]\n[TITLE]x\n[END INFORMATION]\n\n00:00:01.20,00:00:02.50\nOne[br]Two\n\n00:01:00.00,00:01:01.00\nThree\n";
        assert_eq!(
            to_vtt(sub.as_bytes(), "a.sub").unwrap(),
            "WEBVTT\n\n00:00:01.200 --> 00:00:02.500\nOne\nTwo\n\n00:01:00.000 --> 00:01:01.000\nThree\n\n"
        );
    }

    #[test]
    fn reads_other_encodings_and_says_what_it_cant_show() {
        assert_eq!(decode(b"caf\xe9 \x93x\x94"), "café “x”");
        assert_eq!(decode(&[0xFF, 0xFE, b'h', 0, b'i', 0]), "hi");
        assert_eq!(decode(&[0xFE, 0xFF, 0, b'h', 0, b'i']), "hi");
        assert!(to_vtt(&[0, 0, 1, 0xBA, 0x44], "a.sub").unwrap_err().contains("VobSub"));
        assert!(to_vtt(b"nothing here", "a.srt").is_err());
        assert_eq!(to_vtt(b"WEBVTT\n\n00:01.000 --> 00:02.000\nx\n", "a.vtt").unwrap(), "WEBVTT\n\n00:01.000 --> 00:02.000\nx\n");
    }
}
