//! The session packet's tar dialect: POSIX ustar, with a pax `x` header only
//! for a path or link target that does not fit ustar's fields.
//!
//! The writer emits exactly one byte sequence per input (mtime 0, uid/gid 0,
//! empty user/group names, modes 0644/0755/0777) so a packet's digest is a
//! function of its content. The reader accepts that dialect and nothing else:
//! a header the writer could not have produced is refused, which keeps the
//! digest-to-content mapping one-to-one.

use std::io::{self, Read, Write};

use anyhow::{bail, ensure, Context, Result};

const BLOCK: usize = 512;
/// A pax header carries at most a path and a link target; anything larger is
/// not one this writer produced.
const MAX_PAX_BYTES: u64 = 64 * 1024;
/// The fixed name of every pax header, so headers are deterministic.
const PAX_NAME: &str = "././@PaxHeader";

/// What a tar entry is. The packet carries no other kinds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Kind {
    File { size: u64, executable: bool },
    Dir,
    Symlink { target: String },
}

/// One parsed entry header. `path` has no trailing `/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Header {
    pub(crate) path: String,
    pub(crate) kind: Kind,
}

pub(crate) struct Writer<W: Write> {
    out: W,
}

impl<W: Write> Writer<W> {
    pub(crate) fn new(out: W) -> Self {
        Self { out }
    }

    pub(crate) fn dir(&mut self, path: &str) -> Result<()> {
        self.header(&format!("{path}/"), b'5', 0o755, 0, "")
    }

    pub(crate) fn symlink(&mut self, path: &str, target: &str) -> Result<()> {
        self.header(path, b'2', 0o777, 0, target)
    }

    /// Append a regular file whose body is exactly `size` bytes of `body`.
    pub(crate) fn file(
        &mut self,
        path: &str,
        executable: bool,
        size: u64,
        body: &mut dyn Read,
    ) -> Result<()> {
        let mode = if executable { 0o755 } else { 0o644 };
        self.header(path, b'0', mode, size, "")?;
        let copied = io::copy(&mut body.take(size), &mut self.out)?;
        ensure!(copied == size, "{path}: shrank while being packed");
        self.pad(size)
    }

    /// Write the two zero blocks that end the archive and return the sink.
    pub(crate) fn finish(mut self) -> Result<W> {
        self.out.write_all(&[0u8; BLOCK * 2])?;
        Ok(self.out)
    }

    fn pad(&mut self, size: u64) -> Result<()> {
        let rem = (size % BLOCK as u64) as usize;
        if rem != 0 {
            self.out.write_all(&[0u8; BLOCK][..BLOCK - rem])?;
        }
        Ok(())
    }

    fn header(&mut self, name: &str, flag: u8, mode: u32, size: u64, link: &str) -> Result<()> {
        let split = split_ustar(name);
        let mut records = String::new();
        if split.is_none() {
            records.push_str(&pax_record("path", name));
        }
        if link.len() > 100 {
            records.push_str(&pax_record("linkpath", link));
        }
        if !records.is_empty() {
            let block = raw_header(PAX_NAME, "", b'x', 0o644, records.len() as u64, "");
            self.out.write_all(&block)?;
            self.out.write_all(records.as_bytes())?;
            self.pad(records.len() as u64)?;
        }
        let (prefix, short) = split.unwrap_or(("", truncate(name, 100)));
        let link = if link.len() > 100 {
            truncate(link, 100)
        } else {
            link
        };
        self.out
            .write_all(&raw_header(short, prefix, flag, mode, size, link))?;
        Ok(())
    }
}

/// Split a path into ustar's (prefix, name) fields, or `None` when it needs pax.
fn split_ustar(path: &str) -> Option<(&str, &str)> {
    if path.len() <= 100 {
        return Some(("", path));
    }
    path.match_indices('/')
        .map(|(i, _)| (&path[..i], &path[i + 1..]))
        .find(|(p, n)| p.len() <= 155 && !n.is_empty() && n.len() <= 100)
}

/// The longest prefix of `s` that fits `max` bytes on a char boundary.
fn truncate(s: &str, max: usize) -> &str {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// One pax record: `"<len> <key>=<value>\n"`, where `<len>` counts itself.
fn pax_record(key: &str, value: &str) -> String {
    let body = key.len() + value.len() + 3; // ' ', '=', '\n'
    let mut len = body + 1;
    while len != body + len.to_string().len() {
        len = body + len.to_string().len();
    }
    format!("{len} {key}={value}\n")
}

fn raw_header(name: &str, prefix: &str, flag: u8, mode: u32, size: u64, link: &str) -> [u8; BLOCK] {
    let mut h = [0u8; BLOCK];
    h[..name.len()].copy_from_slice(name.as_bytes());
    octal(&mut h[100..108], u64::from(mode));
    octal(&mut h[108..116], 0); // uid
    octal(&mut h[116..124], 0); // gid
    octal(&mut h[124..136], size);
    octal(&mut h[136..148], 0); // mtime
    h[156] = flag;
    h[157..157 + link.len()].copy_from_slice(link.as_bytes());
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    octal(&mut h[329..337], 0); // devmajor
    octal(&mut h[337..345], 0); // devminor
    h[345..345 + prefix.len()].copy_from_slice(prefix.as_bytes());
    h[148..156].copy_from_slice(b"        ");
    let sum: u64 = h.iter().map(|&b| u64::from(b)).sum();
    h[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
    h[155] = b' ';
    h
}

/// Zero-padded octal digits followed by one NUL, filling `field`.
fn octal(field: &mut [u8], value: u64) {
    let digits = field.len() - 1;
    let s = format!("{value:0digits$o}");
    field[..digits].copy_from_slice(s.as_bytes());
    field[digits] = 0;
}

pub(crate) struct Reader<R: Read> {
    input: R,
    /// Body bytes of the current entry not yet consumed.
    pending: u64,
    done: bool,
}

impl<R: Read> Reader<R> {
    pub(crate) fn new(input: R) -> Self {
        Self {
            input,
            pending: 0,
            done: false,
        }
    }

    /// The next entry's header, or `None` at the end-of-archive marker. The
    /// previous entry's body must have been consumed with [`Reader::body`].
    pub(crate) fn next(&mut self) -> Result<Option<Header>> {
        ensure!(self.pending == 0, "tar: entry body not consumed");
        if self.done {
            return Ok(None);
        }
        let mut block = [0u8; BLOCK];
        self.input
            .read_exact(&mut block)
            .context("tar: truncated header")?;
        if block.iter().all(|&b| b == 0) {
            self.input
                .read_exact(&mut block)
                .context("tar: truncated end marker")?;
            ensure!(block.iter().all(|&b| b == 0), "tar: malformed end marker");
            // Nothing may follow the marker: trailing bytes would be content
            // the digest covers but no entry describes.
            let mut rest = Vec::new();
            (&mut self.input).take(1).read_to_end(&mut rest)?;
            ensure!(rest.is_empty(), "tar: data after the end-of-archive marker");
            self.done = true;
            return Ok(None);
        }
        let mut fields = parse_header(&block)?;
        if fields.flag == b'x' {
            ensure!(fields.name == PAX_NAME, "tar: unexpected pax header name");
            ensure!(fields.size <= MAX_PAX_BYTES, "tar: pax header too large");
            let mut body = vec![0u8; fields.size as usize];
            self.input
                .read_exact(&mut body)
                .context("tar: truncated pax header")?;
            self.skip_pad(fields.size)?;
            let (path, link) = parse_pax(&body)?;
            self.input
                .read_exact(&mut block)
                .context("tar: truncated header after pax")?;
            fields = parse_header(&block)?;
            ensure!(fields.flag != b'x', "tar: consecutive pax headers");
            if let Some(path) = path {
                fields.name = path;
            }
            if let Some(link) = link {
                fields.link = link;
            }
        }
        let kind = match fields.flag {
            b'0' => {
                ensure!(
                    matches!(fields.mode, 0o644 | 0o755),
                    "tar: {}: file mode {:o} is not 0644 or 0755",
                    fields.name,
                    fields.mode
                );
                ensure!(
                    fields.link.is_empty(),
                    "tar: {}: file has a link",
                    fields.name
                );
                Kind::File {
                    size: fields.size,
                    executable: fields.mode == 0o755,
                }
            }
            b'5' => {
                ensure!(
                    fields.mode == 0o755 && fields.size == 0,
                    "tar: bad directory header"
                );
                ensure!(
                    fields.link.is_empty(),
                    "tar: {}: dir has a link",
                    fields.name
                );
                let Some(stripped) = fields.name.strip_suffix('/') else {
                    bail!("tar: directory {} lacks a trailing /", fields.name);
                };
                fields.name = stripped.to_string();
                Kind::Dir
            }
            b'2' => {
                ensure!(
                    fields.mode == 0o777 && fields.size == 0,
                    "tar: bad symlink header"
                );
                Kind::Symlink {
                    target: fields.link,
                }
            }
            other => bail!(
                "tar: {}: entry type {:?} is not allowed in a session packet",
                fields.name,
                other as char
            ),
        };
        self.pending = match kind {
            Kind::File { size, .. } => size,
            _ => 0,
        };
        Ok(Some(Header {
            path: fields.name,
            kind,
        }))
    }

    pub(crate) fn into_inner(self) -> R {
        self.input
    }

    /// Copy the current entry's whole body into `sink`.
    pub(crate) fn body(&mut self, sink: &mut dyn Write) -> Result<u64> {
        let size = self.pending;
        let copied = io::copy(&mut (&mut self.input).take(size), sink)?;
        ensure!(copied == size, "tar: truncated entry body");
        self.pending = 0;
        self.skip_pad(size)?;
        Ok(size)
    }

    fn skip_pad(&mut self, size: u64) -> Result<()> {
        let rem = (size % BLOCK as u64) as usize;
        if rem != 0 {
            let mut pad = [0u8; BLOCK];
            self.input
                .read_exact(&mut pad[..BLOCK - rem])
                .context("tar: truncated padding")?;
            ensure!(pad.iter().all(|&b| b == 0), "tar: non-zero padding");
        }
        Ok(())
    }
}

struct Fields {
    name: String,
    link: String,
    flag: u8,
    mode: u32,
    size: u64,
}

fn parse_header(h: &[u8; BLOCK]) -> Result<Fields> {
    let stored = read_octal(&h[148..156]).context("tar: bad checksum field")?;
    let sum: u64 = h
        .iter()
        .enumerate()
        .map(|(i, &b)| {
            if (148..156).contains(&i) {
                32
            } else {
                u64::from(b)
            }
        })
        .sum();
    ensure!(stored == sum, "tar: header checksum mismatch");
    ensure!(
        &h[257..265] == b"ustar\x0000",
        "tar: not a POSIX ustar header"
    );
    for (range, what) in [(108..116, "uid"), (116..124, "gid"), (136..148, "mtime")] {
        ensure!(read_octal(&h[range])? == 0, "tar: non-zero {what}");
    }
    ensure!(
        h[265..329].iter().all(|&b| b == 0),
        "tar: non-empty user/group name"
    );
    for (range, what) in [(329..337, "devmajor"), (337..345, "devminor")] {
        ensure!(read_octal(&h[range])? == 0, "tar: non-zero {what}");
    }
    ensure!(
        h[500..].iter().all(|&b| b == 0),
        "tar: non-zero header padding"
    );
    let name = c_str(&h[..100])?;
    let prefix = c_str(&h[345..500])?;
    Ok(Fields {
        name: if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        },
        link: c_str(&h[157..257])?,
        flag: h[156],
        mode: read_octal(&h[100..108])? as u32,
        size: read_octal(&h[124..136])?,
    })
}

fn c_str(field: &[u8]) -> Result<String> {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    ensure!(
        field[end..].iter().all(|&b| b == 0),
        "tar: bytes after a field's NUL"
    );
    Ok(std::str::from_utf8(&field[..end])
        .context("tar: name is not UTF-8")?
        .to_string())
}

fn read_octal(field: &[u8]) -> Result<u64> {
    let text = std::str::from_utf8(field).context("tar: numeric field is not ASCII")?;
    let digits = text.trim_matches(|c| c == '\0' || c == ' ');
    ensure!(!digits.is_empty(), "tar: empty numeric field");
    u64::from_str_radix(digits, 8).with_context(|| format!("tar: bad octal {digits:?}"))
}

fn parse_pax(body: &[u8]) -> Result<(Option<String>, Option<String>)> {
    let text = std::str::from_utf8(body).context("tar: pax header is not UTF-8")?;
    let (mut path, mut link) = (None, None);
    let mut rest = text;
    while !rest.is_empty() {
        let (len, _) = rest.split_once(' ').context("tar: malformed pax record")?;
        let n: usize = len.parse().context("tar: malformed pax length")?;
        ensure!(
            n <= rest.len() && n > len.len() + 1,
            "tar: pax record overruns"
        );
        let record = &rest[len.len() + 1..n];
        let record = record
            .strip_suffix('\n')
            .context("tar: pax record lacks newline")?;
        let (key, value) = record.split_once('=').context("tar: pax record lacks =")?;
        let slot = match key {
            "path" => &mut path,
            "linkpath" => &mut link,
            other => bail!("tar: pax key {other:?} is not allowed in a session packet"),
        };
        ensure!(slot.is_none(), "tar: duplicate pax key {key}");
        *slot = Some(value.to_string());
        rest = &rest[n..];
    }
    Ok((path, link))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(entries: &[(&str, Kind, &[u8])]) -> Vec<(Header, Vec<u8>)> {
        let mut w = Writer::new(Vec::new());
        for (path, kind, body) in entries {
            match kind {
                Kind::Dir => w.dir(path).unwrap(),
                Kind::Symlink { target } => w.symlink(path, target).unwrap(),
                Kind::File { size, executable } => {
                    w.file(path, *executable, *size, &mut &body[..]).unwrap()
                }
            }
        }
        let bytes = w.finish().unwrap();
        assert_eq!(bytes.len() % BLOCK, 0);
        let mut r = Reader::new(&bytes[..]);
        let mut out = Vec::new();
        while let Some(h) = r.next().unwrap() {
            let mut body = Vec::new();
            r.body(&mut body).unwrap();
            out.push((h, body));
        }
        out
    }

    #[test]
    fn short_and_long_paths_round_trip() {
        let long_dir = "workspace/".to_string() + &"d".repeat(150);
        let long_file = format!("{long_dir}/{}", "f".repeat(90));
        let very_long = format!("workspace/{}", "x/".repeat(200)) + "leaf";
        let long_link = "../".repeat(40) + "t";
        let entries: Vec<(&str, Kind, &[u8])> = vec![
            ("workspace/a", Kind::Dir, b""),
            (
                "workspace/a/run.sh",
                Kind::File {
                    size: 3,
                    executable: true,
                },
                b"hi\n",
            ),
            (
                &long_file,
                Kind::File {
                    size: 0,
                    executable: false,
                },
                b"",
            ),
            (
                &very_long,
                Kind::File {
                    size: 600,
                    executable: false,
                },
                &[7u8; 600],
            ),
            (
                "workspace/l",
                Kind::Symlink {
                    target: long_link.clone(),
                },
                b"",
            ),
        ];
        let got = roundtrip(&entries);
        assert_eq!(got.len(), entries.len());
        for ((h, body), (path, kind, want)) in got.iter().zip(&entries) {
            assert_eq!(&h.path, path);
            assert_eq!(&h.kind, kind);
            assert_eq!(&body[..], *want);
        }
    }

    #[test]
    fn pax_record_length_counts_itself() {
        for n in [1usize, 5, 89, 90, 95, 994, 995, 1000] {
            let rec = pax_record("path", &"p".repeat(n));
            let (len, _) = rec.split_once(' ').unwrap();
            assert_eq!(len.parse::<usize>().unwrap(), rec.len(), "n={n}");
        }
    }

    #[test]
    fn foreign_headers_are_refused() {
        let mut w = Writer::new(Vec::new());
        w.file("workspace/f", false, 1, &mut &b"x"[..]).unwrap();
        let good = w.finish().unwrap();

        // A non-zero mtime is not a header this writer produces.
        let mut bad = good.clone();
        bad[136..147].copy_from_slice(b"00000000001");
        fix_checksum(&mut bad[..BLOCK]);
        assert!(Reader::new(&bad[..]).next().is_err());

        // A device entry is never allowed.
        let mut dev = good.clone();
        dev[156] = b'3';
        fix_checksum(&mut dev[..BLOCK]);
        assert!(Reader::new(&dev[..]).next().is_err());

        // A flipped byte breaks the checksum.
        let mut flipped = good.clone();
        flipped[0] ^= 1;
        assert!(Reader::new(&flipped[..]).next().is_err());

        // Trailing garbage after the end marker.
        let mut trailing = good;
        trailing.push(1);
        let mut r = Reader::new(&trailing[..]);
        r.next().unwrap().unwrap();
        r.body(&mut io::sink()).unwrap();
        assert!(r.next().is_err());
    }

    fn fix_checksum(h: &mut [u8]) {
        h[148..156].copy_from_slice(b"        ");
        let sum: u64 = h.iter().map(|&b| u64::from(b)).sum();
        h[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
        h[155] = b' ';
    }
}
