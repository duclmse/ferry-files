use fs_common::CommonError;

#[derive(thiserror::Error, Debug)]
pub enum AppError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Config error: {0}")]
    Config(String),

    #[error("Conversion error: {0}")]
    Conversion(String),

    #[error("Transfer error: {0}")]
    Transfer(String),

    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),

    // Ssh variant added in Phase 4 alongside the ssh2 dep
    #[error("SSH error: {0}")]
    Ssh(String),

    #[error("Alert error: {0}")]
    Alert(String),

    #[error("Common error: {0}")]
    Common(#[from] CommonError),
}

pub type Result<T> = std::result::Result<T, AppError>;
