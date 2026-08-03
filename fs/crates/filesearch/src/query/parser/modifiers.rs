use crate::query::ast::Modifiers;

// ---------------------------------------------------------------------------
// Modifier helpers
// ---------------------------------------------------------------------------

pub(super) enum ModifierKind {
    CaseSensitive(bool),
    MatchPath,
    FilesOnly,
    FoldersOnly,
    WholeFilename,
    WholeWord,
    ArchiveOnly(bool),
}

pub(super) fn modifier_from_kw(kw: &str, val: &str) -> Option<ModifierKind> {
    // Only treat as modifier when there's no value attached; otherwise it may
    // be a function with an empty-string value (e.g., archive: vs archiveof:).
    match kw {
        "cs" | "case"                    => Some(ModifierKind::CaseSensitive(true)),
        "i"  | "nocase"                  => Some(ModifierKind::CaseSensitive(false)),
        "wfn"                            => Some(ModifierKind::WholeFilename),
        "w"  | "wholeword" | "ww"        => Some(ModifierKind::WholeWord),
        "file"                           => Some(ModifierKind::FilesOnly),
        // "dir" / "folder" with no value = FoldersOnly modifier.
        // "dir:name" with a value falls through to build_predicate as Function::Parent.
        "folder" | "dir" if val.is_empty() => Some(ModifierKind::FoldersOnly),
        // path: with no value is the path modifier; with a value it's a function
        "path" if val.is_empty()         => Some(ModifierKind::MatchPath),
        "archive"   if val.is_empty()    => Some(ModifierKind::ArchiveOnly(true)),
        "noarchive" if val.is_empty()    => Some(ModifierKind::ArchiveOnly(false)),
        _ => None,
    }
}

pub(super) fn apply_modifier(mods: &mut Modifiers, m: ModifierKind) {
    match m {
        ModifierKind::CaseSensitive(v) => mods.case_sensitive    = Some(v),
        ModifierKind::MatchPath        => mods.match_path         = Some(true),
        ModifierKind::FilesOnly        => mods.files_only         = Some(true),
        ModifierKind::FoldersOnly      => mods.folders_only       = Some(true),
        ModifierKind::WholeFilename    => mods.whole_filename      = Some(true),
        ModifierKind::WholeWord        => mods.whole_word          = Some(true),
        ModifierKind::ArchiveOnly(v)   => mods.archive_only       = Some(v),
    }
}

// ---------------------------------------------------------------------------
// Comparison-capable keyword check
// ---------------------------------------------------------------------------

pub(super) fn is_comparison_kw(kw: &str) -> bool {
    matches!(
        kw,
        "size" | "dm" | "datemodified" | "date"
            | "dc" | "datecreated"
            | "da" | "dateaccessed"
            | "len" | "depth"
    )
}
