use crate::query::ast::Modifiers;

// ---------------------------------------------------------------------------
// Modifier helpers
// ---------------------------------------------------------------------------

pub(super) enum ModifierKind {
    CaseSensitive(bool),
    MatchPath(bool),
    FilesOnly,
    FoldersOnly,
    WholeFilename(bool),
    WholeWord(bool),
    ArchiveOnly(bool),
    Regex(bool),
    Wildcards(bool),
    /// Accepted for Everything compatibility. Comparisons are already UTF-8
    /// aware, and accents always match exactly.
    Ignored,
}

pub(super) fn modifier_from_kw(kw: &str, val: &str) -> Option<ModifierKind> {
    // With a value attached these are functions (`dir:name`, `regex:a.b`) or
    // drive letters (`i:\data`); any other modifier applies to the attached
    // value, so `case:Foo` means `case: Foo` and `path:src` means `path: src`.
    if !val.is_empty() && (kw.len() == 1 || matches!(kw, "dir" | "regex" | "archive" | "noarchive"))
    {
        return None;
    }
    let m = match kw {
        "cs" | "case" => ModifierKind::CaseSensitive(true),
        "i" | "nocase" => ModifierKind::CaseSensitive(false),
        "path" => ModifierKind::MatchPath(true),
        "nopath" => ModifierKind::MatchPath(false),
        "wfn" | "wholefilename" => ModifierKind::WholeFilename(true),
        "nowfn" | "nowholefilename" => ModifierKind::WholeFilename(false),
        "w" | "wholeword" | "wholewords" | "ww" => ModifierKind::WholeWord(true),
        "nowholeword" | "nowholewords" | "noww" => ModifierKind::WholeWord(false),
        "file" | "files" => ModifierKind::FilesOnly,
        "folder" | "folders" | "dir" => ModifierKind::FoldersOnly,
        "archive" => ModifierKind::ArchiveOnly(true),
        "noarchive" => ModifierKind::ArchiveOnly(false),
        "regex" => ModifierKind::Regex(true),
        "noregex" => ModifierKind::Regex(false),
        "wildcards" => ModifierKind::Wildcards(true),
        "nowildcards" => ModifierKind::Wildcards(false),
        "ascii" | "utf8" | "noascii" | "diacritics" | "nodiacritics" => ModifierKind::Ignored,
        _ => return None,
    };
    Some(m)
}

pub(super) fn apply_modifier(mods: &mut Modifiers, m: ModifierKind) {
    match m {
        ModifierKind::CaseSensitive(v) => mods.case_sensitive = Some(v),
        ModifierKind::MatchPath(v) => mods.match_path = Some(v),
        ModifierKind::FilesOnly => mods.files_only = Some(true),
        ModifierKind::FoldersOnly => mods.folders_only = Some(true),
        ModifierKind::WholeFilename(v) => mods.whole_filename = Some(v),
        ModifierKind::WholeWord(v) => mods.whole_word = Some(v),
        ModifierKind::ArchiveOnly(v) => mods.archive_only = Some(v),
        ModifierKind::Regex(v) => mods.regex = Some(v),
        ModifierKind::Wildcards(v) => mods.wildcards = Some(v),
        ModifierKind::Ignored => {}
    }
}

// ---------------------------------------------------------------------------
// Comparison-capable keyword check
// ---------------------------------------------------------------------------

pub(super) fn is_comparison_kw(kw: &str) -> bool {
    matches!(
        kw,
        "size"
            | "dm"
            | "datemodified"
            | "date"
            | "dc"
            | "datecreated"
            | "da"
            | "dateaccessed"
            | "len"
            | "depth"
    )
}
