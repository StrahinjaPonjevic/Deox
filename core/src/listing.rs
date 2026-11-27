//! Reads one folder in large batches. Names, sizes and times all come from the folder's own
//! records, so no file has to be opened.
//!
//! NTFS may update those records late for files that are open for writing or hard-linked.
//! Deletion reads each item again, so a stale record leads to the item being skipped.

use std::ffi::{c_void, OsString};
use std::fs::OpenOptions;
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, RawHandle};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::scan::Cancel;

const BUFFER_BYTES: usize = 64 * 1024;

const FILE_LIST_DIRECTORY: u32 = 0x1;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const FILE_FULL_DIRECTORY_INFO: i32 = 14;
const ERROR_FILE_NOT_FOUND: i32 = 2;
const ERROR_NO_MORE_FILES: i32 = 18;

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
/// Set in the tag of reparse points that stand for another path: symlinks and junctions.
/// Others, such as cloud or compressed files, are ordinary files and folders.
const NAME_SURROGATE: u32 = 0x2000_0000;

/// 100 ns intervals between 1601, where Windows time starts, and 1970.
const UNIX_EPOCH_TICKS: u64 = 116_444_736_000_000_000;

#[link(name = "kernel32")]
extern "system" {
    fn GetFileInformationByHandleEx(file: RawHandle, class: i32, info: *mut c_void, size: u32) -> i32;
}

/// A file or folder directly inside the folder being read.
pub(crate) struct Entry {
    pub name: OsString,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
}

/// Calls `visit` for everything directly inside `path`, except links and junctions.
/// Stops early, without an error, when cancelled.
pub(crate) fn read_folder(path: &Path, cancel: &Cancel, mut visit: impl FnMut(Entry)) -> io::Result<()> {
    let folder = OpenOptions::new().access_mode(FILE_LIST_DIRECTORY).custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(path)?;
    // `u64` elements give the alignment the records need.
    let mut buffer = vec![0u64; BUFFER_BYTES / 8];
    while !cancel.is_cancelled() {
        // SAFETY: the handle stays open for the call, and the buffer is writable for the size passed.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                folder.as_raw_handle(),
                FILE_FULL_DIRECTORY_INFO,
                buffer.as_mut_ptr().cast(),
                BUFFER_BYTES as u32,
            )
        };
        if ok == 0 {
            let error = io::Error::last_os_error();
            // An empty drive root reports "not found" instead of "no more files".
            return match error.raw_os_error() {
                Some(ERROR_NO_MORE_FILES | ERROR_FILE_NOT_FOUND) => Ok(()),
                _ => Err(error),
            };
        }
        // SAFETY: the same initialized memory, viewed as bytes.
        let bytes = unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), BUFFER_BYTES) };
        parse(bytes, &mut visit)?;
    }
    Ok(())
}

/// Follows the chain of `FILE_FULL_DIR_INFO` records at the start of `buffer`.
fn parse(buffer: &[u8], visit: &mut impl FnMut(Entry)) -> io::Result<()> {
    let malformed = || io::Error::new(io::ErrorKind::InvalidData, "the folder listing is malformed");
    let mut record = buffer;
    loop {
        let (next, entry) = parse_record(record).ok_or_else(malformed)?;
        if let Some(entry) = entry {
            visit(entry);
        }
        if next == 0 {
            return Ok(());
        }
        record = record.get(next..).ok_or_else(malformed)?;
    }
}

/// The offset of the following record (0 for the last) and the entry, unless it is one to leave
/// out. `None` if the record does not fit in `record`.
fn parse_record(record: &[u8]) -> Option<(usize, Option<Entry>)> {
    let u32_at = |at: usize| Some(u32::from_le_bytes(record.get(at..at + 4)?.try_into().ok()?));
    let u64_at = |at: usize| Some(u64::from_le_bytes(record.get(at..at + 8)?.try_into().ok()?));

    let next = u32_at(0)? as usize;
    let modified = u64_at(24)?;
    let size = u64_at(40)?;
    let attributes = u32_at(56)?;
    let name_bytes = u32_at(60)? as usize;
    // For a reparse point this field holds the reparse tag.
    let reparse_tag = u32_at(64)?;
    let mut name: Vec<u16> = record
        .get(68..68usize.checked_add(name_bytes)?)?
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&pair| u16::from_le_bytes(pair))
        .collect();
    // Some network drivers count a terminating NUL as part of the name.
    while name.last() == Some(&0) {
        name.pop();
    }

    let is_link = attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 && reparse_tag & NAME_SURROGATE != 0;
    let dot = u16::from(b'.');
    // A separator in a name would make the joined path point somewhere else.
    let unusable = name.is_empty()
        || name == [dot]
        || name == [dot, dot]
        || name.iter().any(|&c| c == u16::from(b'\\') || c == u16::from(b'/'));
    let entry = (!is_link && !unusable).then(|| Entry {
        name: OsString::from_wide(&name),
        is_dir: attributes & FILE_ATTRIBUTE_DIRECTORY != 0,
        size,
        modified: system_time(modified),
    });
    Some((next, entry))
}

fn system_time(ticks: u64) -> Option<SystemTime> {
    let span = |ticks: u64| Duration::new(ticks / 10_000_000, (ticks % 10_000_000) as u32 * 100);
    match ticks.checked_sub(UNIX_EPOCH_TICKS) {
        Some(after) => UNIX_EPOCH.checked_add(span(after)),
        None => UNIX_EPOCH.checked_sub(span(UNIX_EPOCH_TICKS - ticks)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One record as Windows lays it out. `last` leaves the offset to the next record at zero.
    fn record(name: &str, attributes: u32, reparse_tag: u32, last: bool) -> Vec<u8> {
        let name: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let length = (68 + name.len()).next_multiple_of(8);
        let mut bytes = vec![0u8; length];
        let next = if last { 0 } else { length as u32 };
        bytes[0..4].copy_from_slice(&next.to_le_bytes());
        bytes[40..48].copy_from_slice(&7u64.to_le_bytes());
        bytes[56..60].copy_from_slice(&attributes.to_le_bytes());
        bytes[60..64].copy_from_slice(&(name.len() as u32).to_le_bytes());
        bytes[64..68].copy_from_slice(&reparse_tag.to_le_bytes());
        bytes[68..68 + name.len()].copy_from_slice(&name);
        bytes
    }

    fn names(buffer: &[u8]) -> Vec<(OsString, bool)> {
        let mut found = Vec::new();
        parse(buffer, &mut |entry| found.push((entry.name, entry.is_dir))).unwrap();
        found
    }

    #[test]
    fn dots_links_and_trailing_nuls_are_handled() {
        const JUNCTION: u32 = 0xA000_0003;
        const CLOUD: u32 = 0x9000_001A;
        let folder = FILE_ATTRIBUTE_DIRECTORY;
        let reparse = FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT;
        let buffer = [
            record(".", folder, 0, false),
            record("..", folder, 0, false),
            record("junction", reparse, JUNCTION, false),
            record("cloud", reparse, CLOUD, false),
            record(r"..\outside", 0, 0, false),
            record("share.txt\0", 0, 0, true),
        ]
        .concat();

        assert_eq!(names(&buffer), vec![("cloud".into(), true), ("share.txt".into(), false)]);
    }

    #[test]
    fn a_record_that_overruns_the_buffer_is_an_error() {
        let mut cut = record("file.txt", 0, 0, true);
        cut.truncate(70);
        let mut dangling = record("a", 0, 0, false);
        dangling[0..4].copy_from_slice(&4096u32.to_le_bytes());

        assert!(parse(&cut, &mut |_| {}).is_err());
        assert!(parse(&dangling, &mut |_| {}).is_err());
    }
}
