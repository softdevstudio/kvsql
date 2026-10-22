//! The byte store underneath the engine: a real file, or the in-memory
//! simulator in [`crate::sim`] that tests use to cut power at any write.

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::Path;

use crate::error::{Error, Result};

/// Positional reads and writes plus a durability barrier.
///
/// `sync` must not return until every earlier `write_at` is durable.
pub trait Storage: Send + Sync {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()>;
    fn write_at(&self, offset: u64, data: &[u8]) -> io::Result<()>;
    fn sync(&self) -> io::Result<()>;
    fn len(&self) -> io::Result<u64>;
    fn is_empty(&self) -> io::Result<bool> {
        Ok(self.len()? == 0)
    }
}

/// A file on disk, locked against a second writer for as long as it is open.
pub struct FileStorage {
    file: File,
}

impl FileStorage {
    pub fn open(path: &Path) -> Result<FileStorage> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(Error::Locked),
            Err(TryLockError::Error(e)) => return Err(Error::Io(e)),
        }
        Ok(FileStorage { file })
    }
}

impl Storage for FileStorage {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        read_exact_at(&self.file, offset, buf)
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        write_all_at(&self.file, offset, data)
    }

    fn sync(&self) -> io::Result<()> {
        // fdatasync still flushes the file length when it grew, which is all
        // the metadata a later read needs; the timestamps it skips are not.
        // On macOS both calls use F_FULLFSYNC.
        self.file.sync_data()
    }

    fn len(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }
}

#[cfg(unix)]
fn read_exact_at(f: &File, offset: u64, buf: &mut [u8]) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    f.read_exact_at(buf, offset)
}

#[cfg(unix)]
fn write_all_at(f: &File, offset: u64, data: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    f.write_all_at(data, offset)
}

#[cfg(windows)]
fn read_exact_at(f: &File, mut offset: u64, mut buf: &mut [u8]) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match f.seek_read(buf, offset) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "read past end of file",
                ))
            }
            Ok(n) => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(windows)]
fn write_all_at(f: &File, mut offset: u64, mut data: &[u8]) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !data.is_empty() {
        match f.seek_write(data, offset) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "write returned 0")),
            Ok(n) => {
                data = &data[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
