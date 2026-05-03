use common::CommonError;

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

    #[error("SSH error: {0}")]
    Ssh(#[from] ssh2::Error),

    #[error("Alert error: {0}")]
    Alert(String),

    #[error("Common error: {0}")]
    Common(#[from] CommonError),
}

pub type Result<T> = std::result::Result<T, AppError>;
