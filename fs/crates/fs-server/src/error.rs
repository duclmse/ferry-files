use fs_common::CommonError;

#[derive(thiserror::Error, Debug)]
pub enum AppError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Config error: {0}")]
    Config(String),

    #[error("Index error: {0}")]
    Index(String),

    #[error("Query error: {0}")]
    Query(String),

    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("Tantivy error: {0}")]
    Tantivy(#[from] tantivy::TantivyError),

    #[error("gRPC error: {0}")]
    Grpc(String),

    #[error("IPC error: {0}")]
    Ipc(String),

    #[error("Content extraction error: {0}")]
    Content(String),

    #[error("Service error: {0}")]
    Service(String),

    #[error("Common error: {0}")]
    Common(#[from] CommonError),
}

#[cfg(feature = "grpc")]
impl From<AppError> for tonic::Status {
    fn from(e: AppError) -> Self {
        tonic::Status::internal(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, AppError>;
