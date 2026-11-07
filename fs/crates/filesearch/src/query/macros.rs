use super::ast::TypeMacro;
use crate::config::MacroConfig;

const AUDIO_EXTS: &[&str] = &[
    "aac", "ac3", "aif", "aifc", "aiff", "au", "cda", "dts", "fla", "flac", "it", "m1a", "m2a",
    "m3u", "m4a", "mid", "midi", "mka", "mod", "mp2", "mp3", "mpa", "ogg", "opus", "ra", "rmi",
    "s3m", "snd", "umx", "voc", "wav", "wma", "xm",
];
const VIDEO_EXTS: &[&str] = &[
    "3g2", "3gp", "3gp2", "3gpp", "amr", "amv", "asf", "avi", "bdmv", "bik", "d2v", "divx", "drc",
    "dsa", "dsm", "dss", "dsv", "evo", "f4v", "flc", "fli", "flic", "flv", "hdmov", "ifo", "ivf",
    "m1v", "m2p", "m2t", "m2ts", "m2v", "m4b", "m4p", "m4v", "mkv", "mov", "mp2v", "mp4", "mp4v",
    "mpe", "mpeg", "mpg", "mpls", "mpv2", "mpv4", "mqv", "ogm", "ogv", "pss", "pva", "qt", "ram",
    "ratdvd", "rm", "rmm", "rmvb", "roq", "rp", "rpm", "rsf", "rts", "smk", "srt", "swf", "tp",
    "tpr", "ts", "vob", "vp6", "webm", "wm", "wmp", "wmv", "wmx", "wvx",
];
const PICTURE_EXTS: &[&str] = &[
    "ani", "bmp", "gif", "ico", "jfif", "jpeg", "jpg", "pcx", "png", "psd", "tga", "tif", "tiff",
    "webp",
];
const DOCUMENT_EXTS: &[&str] = &[
    "c", "chm", "cpp", "csv", "cxx", "doc", "docm", "docx", "dot", "dotm", "dotx", "h", "hpp",
    "htm", "html", "hxx", "ini", "java", "lua", "mht", "mhtml", "nfo", "odp", "ods", "odt", "pdf",
    "potm", "potx", "ppam", "pps", "ppsm", "ppsx", "ppt", "pptm", "pptx", "ps", "py", "rb", "rc",
    "rs", "rtf", "sldm", "sldx", "sql", "svg", "tex", "toml", "txt", "vb", "xlam", "xls", "xlsb",
    "xlsm", "xlsx", "xltm", "xltx", "xml", "yaml", "yml",
];
const ARCHIVE_EXTS: &[&str] = &[
    "7z", "ace", "arj", "bz2", "cab", "gz", "gzip", "jar", "r00", "r01", "r02", "r03", "r04",
    "r05", "r06", "r07", "r08", "r09", "rar", "tar", "taz", "tgz", "xz", "z", "zip",
];
const EXECUTABLE_EXTS: &[&str] = &["bat", "cmd", "exe", "msi", "ps1", "sh"];

/// Return the list of file extensions for a type macro (lowercase, no dot).
pub fn expand_type_macro(m: TypeMacro) -> Vec<String> {
    let exts: &[&str] = match m {
        // The first six must stay identical to Everything's built-in filters
        // (docs/reference/query-language.md §1.5); `source:` and `font:` are
        // filesearch extensions.
        TypeMacro::Audio => AUDIO_EXTS,
        TypeMacro::Video => VIDEO_EXTS,
        TypeMacro::Picture => PICTURE_EXTS,
        TypeMacro::Document => DOCUMENT_EXTS,
        TypeMacro::Archive => ARCHIVE_EXTS,
        TypeMacro::Executable => EXECUTABLE_EXTS,
        TypeMacro::Source => &[
            "ahk", "asm", "c", "cc", "cpp", "cs", "css", "go", "h", "hpp", "hs", "html", "java",
            "js", "json", "lua", "m", "pas", "php", "pl", "py", "rb", "rs", "sql", "swift", "toml",
            "ts", "vb", "xml", "yaml", "yml",
        ],
        TypeMacro::Font => &["fon", "otf", "ttc", "ttf", "woff", "woff2"],
    };
    exts.iter().map(|s| (*s).to_owned()).collect()
}

/// Look up a user-defined macro by name. No recursive expansion.
pub fn expand_user_macro(name: &str, macros: &MacroConfig) -> Option<String> {
    macros.entries.get(name).cloned()
}

/// Parse a type-macro keyword (lowercase).
pub fn keyword_to_type_macro(kw: &str) -> Option<TypeMacro> {
    match kw {
        "audio" => Some(TypeMacro::Audio),
        "video" => Some(TypeMacro::Video),
        "pic" | "picture" | "pictures" | "image" | "images" => Some(TypeMacro::Picture),
        "doc" | "document" | "documents" => Some(TypeMacro::Document),
        "zip" | "archive" | "archives" => Some(TypeMacro::Archive),
        "exe" | "executable" | "executables" => Some(TypeMacro::Executable),
        "source" | "src" => Some(TypeMacro::Source),
        "font" | "fonts" => Some(TypeMacro::Font),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_macros_match_everything_filters() {
        let spec = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/reference/query-language.md"
        ))
        .unwrap();
        for kw in ["audio", "zip", "doc", "exe", "pic", "video"] {
            let prefix = format!("| `{kw}:`");
            let row = spec.lines().find(|l| l.starts_with(&prefix)).unwrap();
            let list = row
                .split("`ext:")
                .nth(1)
                .unwrap()
                .split('`')
                .next()
                .unwrap();
            let expected: Vec<String> = list.split(';').map(str::to_owned).collect();
            let m = keyword_to_type_macro(kw).unwrap();
            assert_eq!(expand_type_macro(m), expected, "{kw}:");
        }
    }
}
