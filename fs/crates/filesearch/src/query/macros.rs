use crate::config::MacroConfig;
use super::ast::TypeMacro;

/// Return the list of file extensions for a type macro (lowercase, no dot).
pub fn expand_type_macro(m: TypeMacro) -> Vec<String> {
    let exts: &[&str] = match m {
        TypeMacro::Audio => &[
            "aac","ac3","aif","aiff","ape","caf","flac","m4a","mp1","mp2","mp3",
            "mpa","oga","ogg","opus","wav","wma","wv",
        ],
        TypeMacro::Video => &[
            "3g2","3gp","avi","divx","dvr-ms","flv","m1v","m2p","m2ts","m4v",
            "mkv","mov","mp4","mp4v","mpeg","mpg","mts","ogm","ogv","rm","rmvb",
            "swf","tp","ts","vob","webm","wmv",
        ],
        TypeMacro::Picture => &[
            "bmp","gif","ico","jpeg","jpg","png","psd","tif","tiff","webp",
        ],
        TypeMacro::Document => &[
            "doc","docm","docx","dot","dotm","dotx","epub","eml","log","msg",
            "odt","pdf","potm","potx","pps","ppsm","ppsx","ppt","pptm","pptx",
            "rtf","txt","xls","xlsm","xlsx","xlt","xltm","xltx","xml","xps",
        ],
        TypeMacro::Archive => &[
            "7z","br","bz","bz2","cab","gz","rar","tar","tgz","xz","z","zip","zst",
        ],
        TypeMacro::Executable => &[
            "bat","cmd","com","exe","gadget","msi","msp","pif","ps1","reg","scr","vbs",
        ],
        TypeMacro::Source => &[
            "ahk","asm","c","cc","cpp","cs","css","go","h","hpp","hs","html","java",
            "js","json","lua","m","pas","php","pl","py","rb","rs","sql","swift","toml",
            "ts","vb","xml","yaml","yml",
        ],
        TypeMacro::Font => &["fon","otf","ttc","ttf","woff","woff2"],
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
        "audio"                           => Some(TypeMacro::Audio),
        "video"                           => Some(TypeMacro::Video),
        "pic"|"picture"|"pictures"|"image"|"images" => Some(TypeMacro::Picture),
        "doc"|"document"|"documents"      => Some(TypeMacro::Document),
        "zip"|"archive"|"archives"        => Some(TypeMacro::Archive),
        "exe"|"executable"|"executables"  => Some(TypeMacro::Executable),
        "source"|"src"                    => Some(TypeMacro::Source),
        "font"|"fonts"                    => Some(TypeMacro::Font),
        _ => None,
    }
}
