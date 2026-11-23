//! EPUB books: the book in memory (`epub`), the XML tokenizer its edits splice by (`xml`), work on
//! many books at once (`batch`, `library`) and the editor's commands (`editor`).

pub mod batch;
pub(crate) mod editor;
pub mod epub;
pub(crate) mod library;
pub mod xml;
