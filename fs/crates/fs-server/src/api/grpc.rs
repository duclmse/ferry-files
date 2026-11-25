//! gRPC API server - Tonic implementation of the `FileSearch` service.

use std::sync::{atomic::Ordering, Arc};

use tokio::sync::{watch, RwLock};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use crate::config::Config;
use crate::indexer::crawler::IndexMetrics;
use crate::query::{evaluator, parser};
use crate::store::{tantivy_store::TantivyStore, FileRecord, FileStore};

use super::dto::archive_entry;

// Include tonic-generated types and the file descriptor set used by reflection.
pub mod proto {
    tonic::include_proto!("fs");

    pub const FILE_DESCRIPTOR_SET: &[u8] = tonic::include_file_descriptor_set!("fs_descriptor");
}

use proto::{
    change_config_request,
    file_search_server::{FileSearch, FileSearchServer},
    ChangeConfigRequest, ConfigApplied, ConfigValue, GetConfigRequest, ListWatchPathsRequest,
    Notice as NoticeMsg, ReindexRequest, ReindexResponse, SearchRequest, SearchResponse,
    SearchResult, StatsRequest, StatsResponse, SubscribeRequest, WatchPathRequest,
    WatchPathsResponse,
};

// ---------------------------------------------------------------------------
// Service struct
// ---------------------------------------------------------------------------

pub struct FileSearchService {
    pub sqlite: Arc<FileStore>,
    pub tantivy: Arc<TantivyStore>,
    pub config: Arc<RwLock<Config>>,
    pub metrics: Arc<IndexMetrics>,
    pub reindex_tx: watch::Sender<bool>,
    pub watch_list: Arc<crate::watch_list::WatchList>,
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
            .run_query(&req.query, req.limit, req.offset)
            .await
            .map_err(|e| *e)?;
        Ok(Response::new(SearchResponse {
            results,
            total: total as i32,
            took_ms: took_ms as i64,
        }))
    }

    type SearchStreamStream = ReceiverStream<Result<SearchResult, Status>>;
    type SubscribeStream = ReceiverStream<Result<NoticeMsg, Status>>;

    async fn subscribe(
        &self,
        request: Request<SubscribeRequest>,
    ) -> Result<Response<Self::SubscribeStream>, Status> {
        use fs_protocol::ipc_protocol::{Notice, NoticeKind};
        let mut kinds = Vec::new();
        for k in request.into_inner().kinds {
            kinds.push(
                NoticeKind::parse(&k)
                    .ok_or_else(|| Status::invalid_argument(format!("unknown kind: {k}")))?,
            );
        }
        let mut rx = self.metrics.subscribe();
        let (tx, out) = tokio::sync::mpsc::channel(16);
        tokio::spawn(async move {
            while let Some(n) = crate::indexer::crawler::next_notice(&mut rx).await {
                if !n.wanted(&kinds) {
                    continue;
                }
                let msg = match n {
                    Notice::WatchPaths { roots } => NoticeMsg {
                        kind: "watch_paths".into(),
                        paths: roots,
                        ..Default::default()
                    },
                    Notice::Root {
                        root,
                        state,
                        entries,
                    } => NoticeMsg {
                        kind: "root".into(),
                        paths: vec![root],
                        state: state.label().into(),
                        count: entries,
                    },
                    Notice::Applied { events, folders } => NoticeMsg {
                        kind: "applied".into(),
                        paths: folders,
                        count: u64::from(events),
                        ..Default::default()
                    },
                    Notice::Lagged(missed) => NoticeMsg {
                        kind: "lagged".into(),
                        count: missed,
                        ..Default::default()
                    },
                };
                // The client went away.
                if tx.send(Ok(msg)).await.is_err() {
                    return;
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(out)))
    }

    async fn search_stream(
        &self,
        request: Request<SearchRequest>,
    ) -> Result<Response<Self::SearchStreamStream>, Status> {
        let req = request.into_inner();
        let (results, _, _) = self
            .run_query(&req.query, req.limit, req.offset)
            .await
            .map_err(|e| *e)?;

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
        let total_files = self
            .sqlite
            .total_files()
            .map_err(|e| Status::internal(e.to_string()))?;
        let m = &self.metrics;
        Ok(Response::new(StatsResponse {
            total_files,
            total_dirs: m.total_dirs.load(Ordering::Relaxed) as i64,
            content_indexed: m.content_indexed.load(Ordering::Relaxed) as i64,
            last_crawl_secs: m.last_crawl_secs.load(Ordering::Relaxed) as i64,
            events_processed: m.events_processed.load(Ordering::Relaxed) as i64,
            index_size_human: String::new(),
            archive_entries_indexed: m.archive_entries_indexed.load(Ordering::Relaxed) as i64,
            archives_crawled: m.archives_crawled.load(Ordering::Relaxed) as i64,
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

    async fn list_watch_paths(
        &self,
        _request: Request<ListWatchPathsRequest>,
    ) -> Result<Response<WatchPathsResponse>, Status> {
        let paths = self.config.read().await.watch_paths.clone();
        Ok(Response::new(self.watch_paths(&paths, false)))
    }

    async fn add_watch_path(
        &self,
        request: Request<WatchPathRequest>,
    ) -> Result<Response<WatchPathsResponse>, Status> {
        let req = self.authorized(request).await?;
        let outcome = self.watch_list.add(&req.path).await;
        self.watch_change(outcome)
    }

    async fn remove_watch_path(
        &self,
        request: Request<WatchPathRequest>,
    ) -> Result<Response<WatchPathsResponse>, Status> {
        let req = self.authorized(request).await?;
        let outcome = self.watch_list.remove(&req.path, req.forget).await;
        self.watch_change(outcome)
    }

    async fn get_config(
        &self,
        request: Request<GetConfigRequest>,
    ) -> Result<Response<ConfigValue>, Status> {
        let req = self.authorized(request).await?;
        let key = Some(req.key).filter(|k| !k.is_empty());
        let v = self
            .watch_list
            .config()
            .get(key.as_deref())
            .await
            .map_err(config_status)?;
        Ok(Response::new(ConfigValue {
            text: v.text,
            from_file: v.from_file,
        }))
    }

    async fn change_config(
        &self,
        request: Request<ChangeConfigRequest>,
    ) -> Result<Response<ConfigApplied>, Status> {
        use change_config_request::Change;
        use fs_protocol::ipc_protocol::ConfigChange;
        let req = self.authorized(request).await?;
        let change = match req.change {
            Some(Change::Set(s)) => ConfigChange::Set {
                key: s.key,
                value: s.value,
            },
            Some(Change::Unset(key)) => ConfigChange::Unset { key },
            Some(Change::Replace(text)) => ConfigChange::Replace { text },
            Some(Change::Reload(true)) => ConfigChange::Reload,
            Some(Change::Reload(false)) | None => {
                return Err(Status::invalid_argument("no change given"))
            }
        };
        let a = self
            .watch_list
            .config()
            .change(change)
            .await
            .map_err(config_status)?;
        Ok(Response::new(ConfigApplied {
            restart: a.restart,
            reloaded: a.reloaded,
            problems: a.problems,
        }))
    }
}

fn config_status(e: crate::config_edit::ConfigError) -> Status {
    use crate::config_edit::ConfigError;
    match e {
        ConfigError::Invalid(_) => Status::invalid_argument(e.to_string()),
        ConfigError::NotFound(_) => Status::not_found(e.to_string()),
        ConfigError::Refused(_) => Status::permission_denied(e.to_string()),
        ConfigError::Io(_) => Status::internal(e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

pub fn service<F>(
    svc: FileSearchService,
    interceptor: F,
) -> tonic::codegen::InterceptedService<FileSearchServer<FileSearchService>, F>
where
    F: tonic::service::Interceptor,
{
    FileSearchServer::with_interceptor(svc, interceptor)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

impl FileSearchService {
    /// The request, once its token is checked.
    async fn authorized<T>(&self, request: Request<T>) -> Result<T, Status> {
        let header = request
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        crate::api::auth::require_grpc_token(&self.config.read().await.api, header.as_deref())?;
        Ok(request.into_inner())
    }

    fn watch_paths(&self, paths: &[std::path::PathBuf], changed: bool) -> WatchPathsResponse {
        let roots: Vec<proto::RootStatus> = self
            .metrics
            .roots(paths)
            .into_iter()
            .map(|r| proto::RootStatus {
                state: r.state.label().to_owned(),
                entries: r.entries,
                started: r.started,
                finished: r.finished,
                path: r.path,
            })
            .collect();
        WatchPathsResponse {
            watch_paths: roots.iter().map(|r| r.path.clone()).collect(),
            crawled: self.metrics.last_crawl_secs.load(Ordering::Relaxed) != 0,
            changed,
            absorbed: Vec::new(),
            roots,
        }
    }

    // Status is what tonic's handlers return; boxing it here gains nothing.
    #[allow(clippy::result_large_err)]
    fn watch_change(
        &self,
        outcome: Result<crate::watch_list::Outcome, crate::watch_list::WatchListError>,
    ) -> Result<Response<WatchPathsResponse>, Status> {
        use crate::watch_list::WatchListError;
        match outcome {
            Ok(o) => {
                let mut response = self.watch_paths(&o.watch_paths, o.changed);
                response.absorbed = o
                    .absorbed
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect();
                Ok(Response::new(response))
            }
            Err(e @ WatchListError::Invalid(_)) => Err(Status::invalid_argument(e.to_string())),
            Err(e @ WatchListError::NotFound(_)) => Err(Status::not_found(e.to_string())),
            Err(e @ WatchListError::Config(_)) => Err(Status::internal(e.to_string())),
        }
    }

    async fn run_query(
        &self,
        query_str: &str,
        limit: i32,
        offset: i32,
    ) -> Result<(Vec<SearchResult>, usize, u64), Box<Status>> {
        // Single config lock - reads macros and the default limit then releases.
        let (macros, default_limit) = {
            let cfg = self.config.read().await;
            (cfg.macros.clone(), cfg.search.default_limit)
        };
        let expr = parser::parse(query_str, &macros)
            .map_err(|e| Box::new(Status::invalid_argument(e.to_string())))?;

        let mut plan = evaluator::plan(&expr, &Default::default())
            .map_err(|e| Box::new(Status::invalid_argument(e.to_string())))?;
        let (limit, offset) = resolve_page(limit, offset, default_limit);
        plan.limit = Some(limit);
        plan.offset = offset;

        let result = evaluator::execute_plan(
            plan,
            Arc::clone(&self.sqlite),
            Arc::clone(&self.tantivy),
            Arc::clone(&self.config),
        )
        .await
        .map_err(|e| Box::new(Status::internal(e.to_string())))?;

        // Consume hits by value - moves Strings rather than cloning them.
        let results = result
            .hits
            .into_iter()
            .map(file_record_into_result)
            .collect();
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
        path: r.path,
        name: r.name,
        extension: r.extension.unwrap_or_default(),
        size_bytes: r.size_bytes as i64,
        mtime: r.mtime,
        score: 0.0,
        snippet: String::new(),
        is_dir: r.is_dir,
        archive_path,
        archive_entry: entry,
    }
}

/// proto3 has no "unset" for scalars, so a client that omits `limit` sends 0;
/// treating that literally returned no results. Negative values would wrap
/// when cast to `usize`.
fn resolve_page(limit: i32, offset: i32, default_limit: usize) -> (usize, usize) {
    let limit = usize::try_from(limit)
        .ok()
        .filter(|&l| l > 0)
        .unwrap_or(default_limit);
    (limit, usize::try_from(offset).unwrap_or(0))
}

#[cfg(test)]
mod page_tests {
    use super::resolve_page;

    #[test]
    fn unset_or_negative_limit_uses_default() {
        assert_eq!(resolve_page(0, 0, 100), (100, 0));
        assert_eq!(resolve_page(-5, 0, 100), (100, 0));
    }

    #[test]
    fn explicit_limit_and_offset_pass_through() {
        assert_eq!(resolve_page(10, 20, 100), (10, 20));
    }

    #[test]
    fn negative_offset_clamps_to_zero() {
        assert_eq!(resolve_page(10, -1, 100), (10, 0));
    }
}
