use thiserror::Error;

/// Errors mirroring `FlipperError` in the iOS FlipperKit.
#[derive(Debug, Error)]
pub enum Error {
    #[error("flipper is not connected")]
    NotConnected,
    #[error("flipper did not answer in time")]
    Timeout,
    #[error("frame of {0} bytes exceeds the limit")]
    FrameTooLarge(usize),
    #[error("malformed RPC frame")]
    MalformedFrame,
    #[error("flipper RPC error: {0}")]
    Rpc(String),
    #[error("invalid path: {0}")]
    InvalidPath(String),
    #[error("unexpected response from flipper")]
    UnexpectedResponse,
    #[error("cancelled")]
    Cancelled,
    #[error("transport: {0}")]
    Transport(String),
    #[error("protobuf decode: {0}")]
    Decode(#[from] prost::DecodeError),
    #[error("encode: {0}")]
    Encode(String),
}

impl Error {
    /// True for errors that mean the link itself is gone or unusable.
    pub fn is_link_error(&self) -> bool {
        matches!(
            self,
            Error::NotConnected
                | Error::FrameTooLarge(_)
                | Error::MalformedFrame
                | Error::Transport(_)
        )
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
