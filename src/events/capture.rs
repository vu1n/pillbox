//! Reading files a guest wrote: the server-agent event captures (opencode SSE,
//! codex NDJSON), structured-stdout captures and agent transcripts. Everything a
//! worker microVM writes is untrusted, and dispatch/eval drain these in-process,
//! so one open rule and one bounded line reader serve every such path.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

/// Longest capture line (and opencode SSE frame) accepted: the same bound the
/// §0 log puts on one record, since a longer input could not land as one anyway.
pub(crate) const MAX_CAPTURE_LINE_BYTES: usize = super::log::MAX_LOG_LINE_BYTES;

/// Open a guest-writable file for reading. A guest can plant a symlink (to a host
/// file or `/dev/zero`) or a FIFO at the path, so it is opened without following a
/// symlink and without blocking, and anything but a regular file is refused.
/// `NotFound` passes through unchanged so a follower can keep waiting for it.
pub(crate) fn open_guest_file(path: &Path) -> io::Result<File> {
    // Symlinks are not followed and a FIFO cannot block the open; only a regular file is read.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .map_err(|e| {
            if e.raw_os_error() == Some(libc::ELOOP) {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("refusing symlinked capture {}", path.display()),
                )
            } else {
                e
            }
        })?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing non-regular capture {}", path.display()),
        ));
    }
    // O_NONBLOCK stays set: it has no effect on reads of a regular file.
    Ok(file)
}

/// What [`read_line_capped`] found.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CappedLine {
    /// The reader is exhausted and no bytes were pending.
    Eof,
    /// One line (without its `\n`) is in the buffer.
    Line,
    /// A line longer than the cap was read and discarded; it held this many bytes.
    Oversized(usize),
}

/// Read one `\n`-terminated line into `buf` (cleared first, `\n` dropped), holding
/// at most `max` bytes: a longer line is consumed to its end and discarded, so an
/// unbounded guest line costs bounded memory and the reader stays line-aligned.
/// An unterminated final line counts as a line.
pub(crate) fn read_line_capped<R: BufRead>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max: usize,
) -> io::Result<CappedLine> {
    buf.clear();
    let mut seen = 0usize;
    loop {
        let available = match reader.fill_buf() {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if available.is_empty() {
            return Ok(match seen {
                0 => CappedLine::Eof,
                n if n > max => CappedLine::Oversized(n),
                _ => CappedLine::Line,
            });
        }
        let (take, done) = match available.iter().position(|&b| b == b'\n') {
            Some(i) => (i, true),
            None => (available.len(), false),
        };
        seen = seen.saturating_add(take);
        if seen <= max {
            buf.extend_from_slice(&available[..take]);
        } else {
            buf.clear();
        }
        reader.consume(take + usize::from(done));
        if done {
            return Ok(if seen > max {
                CappedLine::Oversized(seen)
            } else {
                CappedLine::Line
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::time::Duration;

    #[test]
    fn a_symlinked_capture_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("host-secret");
        std::fs::write(&target, "secret\n").unwrap();
        let link = dir.path().join("events.sse");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let err = open_guest_file(&link).unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
        // /dev/zero through a symlink is refused the same way.
        let zero = dir.path().join("zero.sse");
        std::os::unix::fs::symlink("/dev/zero", &zero).unwrap();
        assert!(open_guest_file(&zero).is_err());
    }

    #[test]
    fn a_fifo_or_device_capture_is_refused_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("events.sse");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || tx.send(open_guest_file(&fifo).map(|_| ())).unwrap());
        let result = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("opening a FIFO with no writer must not block");
        assert!(result.unwrap_err().to_string().contains("non-regular"));
        assert!(open_guest_file(Path::new("/dev/zero")).is_err());
    }

    #[test]
    fn a_regular_file_opens_and_a_missing_one_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.sse");
        assert_eq!(
            open_guest_file(&path).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        std::fs::write(&path, "data: {}\n").unwrap();
        let mut text = String::new();
        io::Read::read_to_string(&mut open_guest_file(&path).unwrap(), &mut text).unwrap();
        assert_eq!(text, "data: {}\n");
    }

    #[test]
    fn an_oversized_line_is_skipped_and_the_next_line_reads() {
        let mut input = vec![b'x'; 100];
        input.extend_from_slice(b"\nok\r\ntail");
        let mut reader = io::BufReader::with_capacity(7, Cursor::new(input));
        let mut buf = Vec::new();
        assert_eq!(
            read_line_capped(&mut reader, &mut buf, 10).unwrap(),
            CappedLine::Oversized(100)
        );
        assert!(buf.is_empty());
        assert_eq!(
            read_line_capped(&mut reader, &mut buf, 10).unwrap(),
            CappedLine::Line
        );
        assert_eq!(buf, b"ok\r");
        assert_eq!(
            read_line_capped(&mut reader, &mut buf, 10).unwrap(),
            CappedLine::Line
        );
        assert_eq!(buf, b"tail");
        assert_eq!(
            read_line_capped(&mut reader, &mut buf, 10).unwrap(),
            CappedLine::Eof
        );
    }
}
