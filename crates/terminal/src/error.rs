use std::{collections::TryReserveError, error::Error, fmt};

#[derive(Debug)]
pub enum StorageError {
    InvalidDimensions,
    CapacityExceeded,
    OutOfBounds,
    EmptyText,
    TextTooLong,
    InvalidAscii,
    InvalidText,
    Allocation(TryReserveError),
}

impl fmt::Display for StorageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDimensions => formatter.write_str("Grid dimensions must be positive"),
            Self::CapacityExceeded => formatter.write_str("Grid dimensions exceed storage limits"),
            Self::OutOfBounds => formatter.write_str("Cell range exceeds the grid bounds"),
            Self::EmptyText => formatter.write_str("A display unit must contain text"),
            Self::TextTooLong => formatter.write_str("Display-unit text exceeds the storage limit"),
            Self::InvalidAscii => {
                formatter.write_str("ASCII writes require bytes from space to tilde")
            }
            Self::InvalidText => {
                formatter.write_str("Text callbacks require printable ASCII or one scalar")
            }
            Self::Allocation(error) => {
                write!(formatter, "Screen storage allocation failed: {error}")
            }
        }
    }
}

impl Error for StorageError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Allocation(error) => Some(error),
            _ => None,
        }
    }
}

impl From<TryReserveError> for StorageError {
    fn from(error: TryReserveError) -> Self {
        Self::Allocation(error)
    }
}
