//! Crate-local error type.
//!
//! Kept deliberately small and `oxideav-core`-free so the standalone
//! build path stays decoupled from the framework error type. A small
//! `From<std::io::Error>` lets `?` work over the disc I/O layer.

use std::fmt;

/// Result alias for the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// What can go wrong when mounting a DVD or walking its file system.
#[derive(Debug)]
pub enum Error {
    /// Underlying I/O error (sector read, file open).
    Io(std::io::Error),
    /// An ISO 9660 structure (PVD, directory record, path table) is
    /// malformed or violates the spec's invariants.
    InvalidIso9660(&'static str),
    /// A UDF descriptor (tag, AVDP, VDS member, FSD, FID, FE) is
    /// malformed or violates the spec's invariants.
    InvalidUdf(&'static str),
    /// The disc image looks like a valid optical disc but isn't a
    /// DVD-Video disc (no `VIDEO_TS/` directory, or `VIDEO_TS/` is
    /// present but `VIDEO_TS.IFO` is absent).
    NotDvdVideo(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::InvalidIso9660(s) => write!(f, "invalid ISO 9660 structure: {s}"),
            Self::InvalidUdf(s) => write!(f, "invalid UDF structure: {s}"),
            Self::NotDvdVideo(s) => write!(f, "not a DVD-Video disc: {s}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
