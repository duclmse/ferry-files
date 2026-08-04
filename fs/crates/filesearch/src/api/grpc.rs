//! gRPC API server - Tonic implementation of the `FileSearch` service.

use std::sync::{atomic::Ordering, Arc};

use tokio::sync::{watch, RwLock};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use crate::config::Config;
use crate::indexer::crawler::IndexMetrics;
use crate::query::{evaluator, parser};
use crate::store::{FileStore, tantivy_store::TantivyStore, FileRecord};

use super::dto::archive_entry;

// Include tonic-generated types and the file descriptor set used by reflection.
pub mod proto {
    tonic::include_proto!("filesearch");

    pub const FILE_DESCRIPTOR_SET: &[u8] =
        tonic::include_file_descriptor_set!("filesearch_descriptor");
}

use proto::{
    file_search_server::{FileSearch, FileSearchServer},
    ReindexRequest, ReindexResponse,
    SearchRequest, SearchResponse, SearchResult,
    StatsRequest, StatsResponse,
};

// ---------------------------------------------------------------------------
// Service struct
// ---------------------------------------------------------------------------

pub struct FileSearchService {
    pub sqlite:     Arc<FileStore>,
    pub tantivy:    Arc<TantivyStore>,
    pub config:     Arc<RwLock<Config>>,
    pub metrics:    Arc<IndexMetrics>,
    pub reindex_tx: watch::Sender<bool>,
}

// ---------------------------------------------------------------------------
// Tonic service implementation
// ---------------------------------------------------------------------------

#[tonic::async_trait]
impl FileSearch for FileSearchService {
    async fn search(
        &self,
        request: Request<SearchRequest>,
    ) -> Result<Response<SearchResponse>, Status> {
        let req = request.into_inner();
        let (results, total, took_ms) = self
            .run_query(&req.query, req.limit as usize, req.offset as usize)
            .await?;
        Ok(Response::new(SearchResponse {
            results,
            total:    total as i32,
            took_ms:  took_ms as i64,
        }))
    }

    type SearchStreamStream = ReceiverStream<Result<SearchResult, Status>>;

    async fn search_stream(
        &self,
        request: Request<SearchRequest>,
    ) -> Result<Response<Self::SearchStreamStream>, Status> {
        let req = request.into_inner();
        let (results, _, _) = self
            .run_query(&req.query, req.limit as usize, req.offset as usize)
            .await?;

        // Fan results out through a bounded channel so the caller can start
        // consuming before all records are serialised.
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        tokio::spawn(async move {
            for r in results {
                if tx.send(Ok(r)).await.is_err() {
                    break;
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn get_stats(
        &self,
        _request: Request<StatsRequest>,
    ) -> Result<Response<StatsResponse>, Status> {
        let total_files = self.sqlite.total_files()
            .map_err(|e| Status::internal(e.to_string()))?;
        let m = &self.metrics;
        Ok(Response::new(StatsResponse {
            total_files,
            total_dirs:              m.total_dirs.load(Ordering::Relaxed) as i64,
            content_indexed:         m.content_indexed.load(Ordering::Relaxed) as i64,
            last_crawl_secs:         m.last_crawl_secs.load(Ordering::Relaxed) as i64,
            events_processed:        m.events_processed.load(Ordering::Relaxed) as i64,
            index_size_human:        String::new(),
            archive_entries_indexed: m.archive_entries_indexed.load(Ordering::Relaxed) as i64,
            archives_crawled:        m.archives_crawled.load(Ordering::Relaxed) as i64,
        }))
    }

    async fn reindex(
        &self,
        request: Request<ReindexRequest>,
    ) -> Result<Response<ReindexResponse>, Status> {
        let full = request.into_inner().full;
        let _ = self.reindex_tx.send(full);
        Ok(Response::new(ReindexResponse { accepted: true }))
    }
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

pub fn service(svc: FileSearchService) -> FileSearchServer<FileSearchService> {
    FileSearchServer::new(svc)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

impl FileSearchService {
    async fn run_query(
        &self,
        query_str: &str,
        limit:     usize,
        offset:    usize,
    ) -> Result<(Vec<SearchResult>, usize, u64), Status> {
        // Single config lock - reads macros then releases.
        let macros = self.config.read().await.macros.clone();
        let expr = parser::parse(query_str, &macros)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;

        let mut plan = evaluator::plan(&expr, &Default::default())
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        plan.limit  = Some(limit);
        plan.offset = offset;

        let result = evaluator::execute_plan(
            plan,
            Arc::clone(&self.sqlite),
            Arc::clone(&self.tantivy),
            Arc::clone(&self.config),
        )
        .await
        .map_err(|e| Status::internal(e.to_string()))?;

        // Consume hits by value - moves Strings rather than cloning them.
        let results = result.hits.into_iter().map(file_record_into_result).collect();
        Ok((results, result.total, result.took_ms))
    }
}

/// Converts an owned `FileRecord` into a proto `SearchResult`, moving all
/// `String` fields to avoid heap copies.
fn file_record_into_result(r: FileRecord) -> SearchResult {
    // Compute archive_entry while path is still accessible, then move path.
    let entry = archive_entry(&r);
    let archive_path = r.archive_path.unwrap_or_default();
    SearchResult {
        path:          r.path,
        name:          r.name,
        extension:     r.extension.unwrap_or_default(),
        size_bytes:    r.size_bytes as i64,
        mtime:         r.mtime,
        score:         0.0,
        snippet:       String::new(),
        is_dir:        r.is_dir,
        archive_path,
        archive_entry: entry,
    }
}
