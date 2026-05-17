use std::{io, ops::Range};

use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("The file you parsing is not a mrp file")]
    NotMrpError,
    
    #[error("The file you parsing maybe destroied (cannot ungzip): {0}")]
    GunzipError(String),
    
    #[error("The file you parsing maybe destroied (cannot read information): {0}")]
    ReadInfoError(String),
    
    #[error("The file you parsing maybe destroied (cannot decode to utf8)")]
    Utf8DecodeError,

    #[error(transparent)]
    IoError(#[from] io::Error),

    #[error("offset {0} out of range: {1:?}.")]
    OutOfRange(usize, Range<usize>),
    #[error("number overflowing.")]
    NumberOverflow,
    #[error("buffer overflowing, {0}.")]
    BufferOverflow(usize),
}

pub type Result<T> = ::std::result::Result<T, Error>;
