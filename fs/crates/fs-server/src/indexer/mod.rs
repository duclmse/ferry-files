pub mod archive;
pub mod content;
pub mod crawler;
pub mod exclusions;
pub mod file_lists;
pub mod mft;

/// The walk settings every crawl and scan shares: hidden entries included,
/// `.ignore` files honoured, git's ignore rules not. `git_global` and
/// `git_exclude` default to on even with `git_ignore` off, so a user's
/// global gitignore hid matching files inside repositories, and each folder
/// cost a `.git` probe (15% of a walk over 83k entries).
pub fn walk_builder(root: &std::path::Path) -> ignore::WalkBuilder {
    let mut wb = ignore::WalkBuilder::new(root);
    wb.hidden(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false);
    wb
}
