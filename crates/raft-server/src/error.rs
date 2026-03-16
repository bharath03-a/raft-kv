use thiserror::Error;

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Encode(#[from] bincode::Error),

    #[error("connection closed")]
    ConnectionClosed,

    #[error("cluster peer {0} unreachable")]
    PeerUnreachable(u64),
}
