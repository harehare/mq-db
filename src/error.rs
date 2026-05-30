use thiserror::Error;

#[derive(Debug, Error)]
pub enum MqdbError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Parse error: {0}")]
    Parse(String),

    #[error("Storage error: {0}")]
    Storage(String),

    #[error("Document not found: id={0}")]
    DocumentNotFound(u32),

    #[error("SQL parse error: {0}")]
    SqlParse(String),

    #[error("SQL execution error: {0}")]
    SqlExec(String),

    #[error("mq error: {0}")]
    Mq(String),
}
