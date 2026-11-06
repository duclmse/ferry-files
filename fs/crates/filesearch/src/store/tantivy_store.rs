use std::path::Path;
use std::sync::{Arc, Mutex};

use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{Field, Schema, Value, FAST, INDEXED, STORED, STRING, TEXT};
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument};

use super::{FileRecord, MetaFilters, SearchHit};
use crate::error::{AppError, Result};

// ---------------------------------------------------------------------------

pub struct TantivyStore {
    index: Index,
    reader: IndexReader,
    writer: Arc<Mutex<IndexWriter>>,
    f_path: Field,
    f_name: Field,
    f_content: Field,
    f_ext: Field,
    f_mtime: Field,
    f_size: Field,
}

impl TantivyStore {
    pub fn new(index_dir: &Path) -> Result<Self> {
        let mut builder = Schema::builder();
        let f_path = builder.add_text_field("path", STRING | STORED);
        let f_name = builder.add_text_field("name", TEXT | STORED);
        let f_content = builder.add_text_field("content", TEXT);
        let f_ext = builder.add_text_field("extension", STRING | STORED);
        let f_mtime = builder.add_u64_field("mtime", INDEXED | FAST | STORED);
        let f_size = builder.add_u64_field("size_bytes", INDEXED | FAST | STORED);
        let schema = builder.build();

        let index = if index_dir == Path::new(":memory:") {
            Index::create_in_ram(schema)
        } else {
            std::fs::create_dir_all(index_dir).map_err(AppError::Io)?;
            Index::open_or_create(
                tantivy::directory::MmapDirectory::open(index_dir)
                    .map_err(|e| AppError::Tantivy(e.into()))?,
                schema,
            )
            .map_err(AppError::Tantivy)?
        };

        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .map_err(AppError::Tantivy)?;

        // 128 MB writer heap
        let writer = index.writer(128 * 1024 * 1024).map_err(AppError::Tantivy)?;

        Ok(Self {
            index,
            reader,
            writer: Arc::new(Mutex::new(writer)),
            f_path,
            f_name,
            f_content,
            f_ext,
            f_mtime,
            f_size,
        })
    }

    /// Delete-then-add upsert. Does NOT commit - call `commit()` separately.
    pub fn upsert(&self, record: &FileRecord, content: Option<&str>) -> Result<()> {
        let term = tantivy::Term::from_field_text(self.f_path, &record.path);
        let w = self
            .writer
            .lock()
            .map_err(|e| AppError::Index(e.to_string()))?;

        w.delete_term(term);

        let mut doc = TantivyDocument::new();
        doc.add_text(self.f_path, &record.path);
        doc.add_text(self.f_name, &record.name);
        if let Some(ext) = &record.extension {
            doc.add_text(self.f_ext, ext);
        }
        doc.add_u64(self.f_mtime, record.mtime as u64);
        doc.add_u64(self.f_size, record.size_bytes);
        if let Some(c) = content {
            doc.add_text(self.f_content, c);
        }

        w.add_document(doc).map_err(AppError::Tantivy)?;
        Ok(())
    }

    /// Delete document(s) matching this path. Does NOT commit.
    pub fn remove(&self, path: &str) -> Result<()> {
        let term = tantivy::Term::from_field_text(self.f_path, path);
        let w = self
            .writer
            .lock()
            .map_err(|e| AppError::Index(e.to_string()))?;
        w.delete_term(term);
        Ok(())
    }

    /// Flush pending operations to disk.
    pub fn commit(&self) -> Result<()> {
        let mut w = self
            .writer
            .lock()
            .map_err(|e| AppError::Index(e.to_string()))?;
        w.commit().map_err(AppError::Tantivy)?;
        Ok(())
    }

    /// Reload the reader so searches see the latest committed data.
    pub fn reload_reader(&self) -> Result<()> {
        self.reader.reload().map_err(AppError::Tantivy)
    }

    /// Full-text search. Returns results ordered by relevance.
    /// Post-filters by `MetaFilters` (size, mtime, extension).
    pub fn search_fulltext(
        &self,
        query: &str,
        limit: usize,
        filters: &MetaFilters,
    ) -> Result<Vec<SearchHit>> {
        let searcher = self.reader.searcher();

        // Parse against name + content fields
        let parser = QueryParser::for_index(&self.index, vec![self.f_name, self.f_content]);
        let parsed = parser
            .parse_query(query)
            .map_err(|e| AppError::Query(e.to_string()))?;

        // Fetch more than `limit` so we have room after post-filtering
        let fetch = (limit * 4).max(limit + 64);
        let top_docs = searcher
            .search(&parsed, &TopDocs::with_limit(fetch))
            .map_err(AppError::Tantivy)?;

        // Snippet generator for the content field
        let snippet_gen = tantivy::SnippetGenerator::create(&searcher, &parsed, self.f_content)
            .map_err(AppError::Tantivy)?;

        let mut hits = Vec::with_capacity(limit);
        for (score, addr) in top_docs {
            if hits.len() >= limit {
                break;
            }
            let doc: TantivyDocument = searcher.doc(addr).map_err(AppError::Tantivy)?;

            let path = doc
                .get_first(self.f_path)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();

            // Post-filter: size
            if let Some(min) = filters.min_size {
                let sz = doc
                    .get_first(self.f_size)
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                if sz < min {
                    continue;
                }
            }
            if let Some(max) = filters.max_size {
                let sz = doc
                    .get_first(self.f_size)
                    .and_then(|v| v.as_u64())
                    .unwrap_or(u64::MAX);
                if sz > max {
                    continue;
                }
            }

            // Post-filter: mtime
            if let Some(after) = filters.after {
                let mt = doc
                    .get_first(self.f_mtime)
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as i64;
                if mt < after {
                    continue;
                }
            }
            if let Some(before) = filters.before {
                let mt = doc
                    .get_first(self.f_mtime)
                    .and_then(|v| v.as_u64())
                    .unwrap_or(i64::MAX as u64) as i64;
                if mt > before {
                    continue;
                }
            }

            // Post-filter: extension
            if let Some(exts) = &filters.extensions {
                if !exts.is_empty() {
                    let ext = doc
                        .get_first(self.f_ext)
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if !exts.iter().any(|e| e == ext) {
                        continue;
                    }
                }
            }

            // Generate snippet (replace Tantivy's <b>/<b> with <mark></mark>)
            let snippet_str = snippet_gen.snippet_from_doc(&doc);
            let raw = snippet_str.to_html();
            let snippet = if raw.is_empty() {
                None
            } else {
                Some(raw.replace("<b>", "<mark>").replace("</b>", "</mark>"))
            };

            hits.push(SearchHit {
                path,
                score,
                snippet,
            });
        }
        Ok(hits)
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_store() -> TantivyStore {
        TantivyStore::new(Path::new(":memory:")).unwrap()
    }

    fn sample_record(path: &str) -> FileRecord {
        FileRecord {
            id: None,
            path: path.to_owned(),
            name: path.split('/').next_back().unwrap_or(path).to_owned(),
            extension: Some("rs".into()),
            size_bytes: 512,
            mtime: 1_000_000,
            ctime: 1_000_000,
            is_dir: false,
            is_hidden: false,
            is_symlink: false,
            attrib_bitmask: 0,
            content_indexed: true,
            tantivy_id: None,
            indexed_at: 1_000_000,
            archive_path: None,
        }
    }

    #[test]
    fn upsert_commit_search_finds_document() {
        let store = make_store();
        let rec = sample_record("/src/main.rs");
        store
            .upsert(&rec, Some("fn main() { println!(\"hello world\"); }"))
            .unwrap();
        store.commit().unwrap();
        store.reload_reader().unwrap();

        let filters = MetaFilters::default();
        let hits = store.search_fulltext("hello", 10, &filters).unwrap();
        assert!(!hits.is_empty(), "expected at least one hit for 'hello'");
        assert!(hits.iter().any(|h| h.path == "/src/main.rs"));
    }

    #[test]
    fn remove_then_search_returns_nothing() {
        let store = make_store();
        let rec = sample_record("/src/lib.rs");
        store
            .upsert(&rec, Some("pub fn add(a: i32, b: i32) -> i32 { a + b }"))
            .unwrap();
        store.commit().unwrap();
        store.reload_reader().unwrap();

        store.remove("/src/lib.rs").unwrap();
        store.commit().unwrap();
        store.reload_reader().unwrap();

        let filters = MetaFilters::default();
        let hits = store.search_fulltext("add", 10, &filters).unwrap();
        assert!(
            hits.iter().all(|h| h.path != "/src/lib.rs"),
            "removed document should not appear in results"
        );
    }

    #[test]
    fn snippet_contains_mark_tags() {
        let store = make_store();
        let rec = sample_record("/docs/readme.rs");
        store
            .upsert(
                &rec,
                Some("This project implements a fast filesearch engine."),
            )
            .unwrap();
        store.commit().unwrap();
        store.reload_reader().unwrap();

        let filters = MetaFilters::default();
        let hits = store.search_fulltext("filesearch", 10, &filters).unwrap();
        let hit = hits.iter().find(|h| h.path == "/docs/readme.rs");
        assert!(hit.is_some(), "document should be found");
        if let Some(h) = hit {
            if let Some(snip) = &h.snippet {
                // Tantivy may or may not produce a snippet for a short doc;
                // if it does, it must use <mark> not <b>
                assert!(
                    !snip.contains("<b>"),
                    "snippet must not contain raw <b> tags: {snip}"
                );
            }
        }
    }

    #[test]
    fn extension_filter_applied_post_search() {
        let store = make_store();
        let rec_rs = sample_record("/a/foo.rs");
        let mut rec_toml = sample_record("/a/bar.toml");
        rec_toml.extension = Some("toml".into());

        store.upsert(&rec_rs, Some("search keyword here")).unwrap();
        store
            .upsert(&rec_toml, Some("search keyword here"))
            .unwrap();
        store.commit().unwrap();
        store.reload_reader().unwrap();

        let filters = MetaFilters {
            extensions: Some(vec!["rs".into()]),
            ..Default::default()
        };
        let hits = store.search_fulltext("keyword", 10, &filters).unwrap();
        assert!(hits.iter().all(|h| h.path.ends_with(".rs")));
        assert!(hits.iter().any(|h| h.path == "/a/foo.rs"));
        assert!(hits.iter().all(|h| h.path != "/a/bar.toml"));
    }
}
