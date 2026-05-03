/// Marker trait for all project errors.
pub trait ProjectError: std::error::Error + Send + Sync + 'static {}

#[derive(thiserror::Error, Debug)]
pub enum CommonError {
    #[error("Config error: {0}")]
    Config(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Logger init error: {0}")]
    Logger(String),
}
