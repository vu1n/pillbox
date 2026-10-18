//! The session packet (`pillbox.session-packet/1`): one deterministic tar that
//! carries a workspace tree and a harness's state tree between turns, on every
//! placement. `pillbox checkpoint pack|unpack` are its only writer and reader.
//!
//! The format is specified by `proto/pillbox/session-packet-1.schema.json`
//! (the `packet.json` manifest) and `docs/session-packet.md` (the tar layout,
//! exclusions and limits); the Rust types here follow that schema, not the
//! reverse.
//!
//! Governed by docs/decisions.md 2026-10-11 (the packet is specified as JSON
//! Schema; the `--no-default-features` build carries every packet subcommand)
//! and 2026-10-18 (layout and exclusions).

mod tar;

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, Read, Seek, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::errors::PillboxError;

pub(crate) const FORMAT: &str = "pillbox.session-packet/1";
/// Largest packet `pack` writes or `unpack` reads, in bytes (1 GiB).
pub(crate) const MAX_PACKET_BYTES: u64 = 1 << 30;
/// Most entries one manifest may list.
pub(crate) const MAX_ENTRIES: usize = 250_000;
/// Largest `packet.json` `unpack` will parse.
const MAX_MANIFEST_BYTES: u64 = 64 << 20;
/// Longest entry path, symlink target or transcript path, in bytes.
const MAX_PATH_BYTES: usize = 4096;
const MANIFEST_NAME: &str = "packet.json";

/// Basenames never packed from either tree: harness OAuth credentials and the
/// vault's CA material.
const CREDENTIAL_FILES: &[&str] = &[
    ".credentials.json",
    "pillbox-vault-ca.crt",
    "pillbox-vault-ca.key",
];

/// The two trees a packet carries; each unpacks to its own directory.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tree {
    Workspace,
    Harness,
}

impl Tree {
    fn name(self) -> &'static str {
        match self {
            Tree::Workspace => "workspace",
            Tree::Harness => "harness",
        }
    }

    fn of(path: &str) -> Option<Tree> {
        match path.split('/').next()? {
            "workspace" => Some(Tree::Workspace),
            "harness" => Some(Tree::Harness),
            _ => None,
        }
    }
}

/// Is `name` a credential file that must not enter a packet from `tree`? The
/// harness tree also drops every agent's credential sentinel (`auth.json`,
/// `opencode.db`, …), since that tree is an agent home's state.
fn is_credential(tree: Tree, name: &str) -> bool {
    CREDENTIAL_FILES.contains(&name)
        || (tree == Tree::Harness
            && crate::agents::ALL
                .iter()
                .any(|a| a.cred_sentinel.rsplit('/').next() == Some(name)))
}

/// `packet.json`. Closed: unknown fields are refused, and absent optionals are
/// omitted (never `null`), matching the schema.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Manifest {
    pub(crate) format: String,
    pub(crate) harness: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) harness_session_id: Option<String>,
    /// Path of the harness transcript, relative to `harness/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) transcript: Option<String>,
    pub(crate) entries: Vec<Entry>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Entry {
    Dir {
        path: String,
    },
    File {
        path: String,
        size: u64,
        sha256: String,
        executable: bool,
    },
    Symlink {
        path: String,
        target: String,
    },
}

impl Entry {
    pub(crate) fn path(&self) -> &str {
        match self {
            Entry::Dir { path } | Entry::File { path, .. } | Entry::Symlink { path, .. } => path,
        }
    }

    fn header(&self) -> tar::Header {
        let kind = match self {
            Entry::Dir { .. } => tar::Kind::Dir,
            Entry::File {
                size, executable, ..
            } => tar::Kind::File {
                size: *size,
                executable: *executable,
            },
            Entry::Symlink { target, .. } => tar::Kind::Symlink {
                target: target.clone(),
            },
        };
        tar::Header {
            path: self.path().to_string(),
            kind,
        }
    }
}

/// A path `pack` left out, reported so nothing is dropped silently.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub(crate) struct Excluded {
    pub(crate) path: String,
    /// `credential` (a file on the exclusion list) or `special` (a socket,
    /// FIFO or device, which a packet cannot carry).
    pub(crate) reason: &'static str,
}

pub(crate) struct PackOptions {
    pub(crate) workspace: PathBuf,
    pub(crate) harness_state: Option<PathBuf>,
    pub(crate) harness: String,
    pub(crate) harness_session_id: Option<String>,
    pub(crate) transcript: Option<String>,
    pub(crate) out: PathBuf,
}

#[derive(Debug)]
pub(crate) struct Packed {
    pub(crate) digest: String,
    pub(crate) bytes: u64,
    pub(crate) manifest: Manifest,
    pub(crate) excluded: Vec<Excluded>,
}

pub(crate) struct UnpackOptions {
    pub(crate) packet: PathBuf,
    pub(crate) expect: String,
    pub(crate) workspace: PathBuf,
    pub(crate) harness_state: Option<PathBuf>,
    pub(crate) allow_nonempty: bool,
}

#[derive(Debug)]
pub(crate) struct Unpacked {
    pub(crate) digest: String,
    pub(crate) manifest: Manifest,
}

const PACK: &str = "checkpoint pack";
const UNPACK: &str = "checkpoint unpack";

fn refuse(action: &'static str, reason: impl Into<String>) -> anyhow::Error {
    PillboxError::runtime(action, reason).into()
}

fn usage(action: &'static str, reason: impl Into<String>) -> anyhow::Error {
    PillboxError::usage(action, reason).into()
}

pub(crate) fn pack(opts: &PackOptions) -> Result<Packed> {
    if !opts.workspace.is_dir() {
        return Err(usage(
            PACK,
            format!(
                "--workspace {} is not a directory",
                opts.workspace.display()
            ),
        ));
    }
    if let Some(dir) = &opts.harness_state {
        if !dir.is_dir() {
            return Err(usage(
                PACK,
                format!("--harness-state {} is not a directory", dir.display()),
            ));
        }
    } else if opts.transcript.is_some() {
        return Err(usage(PACK, "--transcript needs --harness-state"));
    }

    let mut planned: Vec<(Entry, Option<PathBuf>)> = Vec::new();
    let mut excluded = Vec::new();
    walk(
        Tree::Workspace,
        &opts.workspace,
        &mut planned,
        &mut excluded,
    )?;
    if let Some(dir) = &opts.harness_state {
        walk(Tree::Harness, dir, &mut planned, &mut excluded)?;
    }
    planned.sort_by(|a, b| a.0.path().cmp(b.0.path()));
    excluded.sort_by(|a, b| a.path.cmp(&b.path));

    let manifest = Manifest {
        format: FORMAT.to_string(),
        harness: opts.harness.clone(),
        harness_session_id: opts.harness_session_id.clone(),
        transcript: opts.transcript.clone(),
        entries: planned.iter().map(|(e, _)| e.clone()).collect(),
    };
    validate(&manifest).map_err(|e| refuse(PACK, format!("{e:#}")))?;
    let manifest_bytes = serde_json::to_vec(&manifest)?;

    let parent = match opts.out.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let tmp = tempfile::Builder::new()
        .prefix(".pillbox-packet-")
        .tempfile_in(&parent)
        .with_context(|| format!("create a temp file in {}", parent.display()))?;
    let sink = Hashing::new(io::BufWriter::new(tmp.as_file()), MAX_PACKET_BYTES);
    let mut w = tar::Writer::new(sink);
    w.file(
        MANIFEST_NAME,
        false,
        manifest_bytes.len() as u64,
        &mut &manifest_bytes[..],
    )?;
    for (entry, source) in &planned {
        match entry {
            Entry::Dir { path } => w.dir(path)?,
            Entry::Symlink { path, target } => w.symlink(path, target)?,
            Entry::File {
                path,
                size,
                sha256,
                executable,
            } => {
                let source = source.as_ref().expect("file entries carry a source");
                let mut body = Hashing::new(
                    File::open(source).with_context(|| format!("open {}", source.display()))?,
                    u64::MAX,
                );
                w.file(path, *executable, *size, &mut body)?;
                let grew = body.read(&mut [0u8; 1])? != 0;
                if grew || &body.hex() != sha256 {
                    return Err(refuse(PACK, format!("{path} changed while being packed")));
                }
            }
        }
    }
    let (bytes, digest) = {
        let sink = w.finish()?;
        let (bytes, digest) = (sink.count, format!("sha256:{}", sink.hex()));
        let file = sink.inner.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;
        (bytes, digest)
    };
    tmp.persist(&opts.out)
        .with_context(|| format!("write {}", opts.out.display()))?;
    Ok(Packed {
        digest,
        bytes,
        manifest,
        excluded,
    })
}

/// Add every entry under `root` to `planned` as `<tree>/<relative path>`,
/// without following symlinks. File digests are computed here.
fn walk(
    tree: Tree,
    root: &Path,
    planned: &mut Vec<(Entry, Option<PathBuf>)>,
    excluded: &mut Vec<Excluded>,
) -> Result<()> {
    let mut stack = vec![(root.to_path_buf(), tree.name().to_string())];
    let mut total: u64 = 0;
    while let Some((dir, prefix)) = stack.pop() {
        let entries = fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))?;
        for item in entries {
            let item = item.with_context(|| format!("read an entry of {}", dir.display()))?;
            let source = item.path();
            let Some(name) = item.file_name().to_str().map(str::to_string) else {
                return Err(refuse(
                    PACK,
                    format!("{} has a non-UTF-8 name", source.display()),
                ));
            };
            let path = format!("{prefix}/{name}");
            let file_type = item.file_type()?;
            if file_type.is_symlink() {
                let target = fs::read_link(&source)?;
                let Some(target) = target.to_str() else {
                    return Err(refuse(PACK, format!("{path}: non-UTF-8 symlink target")));
                };
                planned.push((
                    Entry::Symlink {
                        path,
                        target: target.to_string(),
                    },
                    None,
                ));
            } else if file_type.is_dir() {
                planned.push((Entry::Dir { path: path.clone() }, None));
                stack.push((source, path));
            } else if !file_type.is_file() {
                excluded.push(Excluded {
                    path,
                    reason: "special",
                });
            } else if is_credential(tree, &name) {
                excluded.push(Excluded {
                    path,
                    reason: "credential",
                });
            } else {
                let meta = item.metadata()?;
                total = total.saturating_add(meta.len());
                if total > MAX_PACKET_BYTES {
                    return Err(refuse(
                        PACK,
                        format!(
                            "{} holds more than the {MAX_PACKET_BYTES}-byte packet limit",
                            root.display()
                        ),
                    ));
                }
                let mut hasher = Hashing::new(io::sink(), u64::MAX);
                let size = io::copy(
                    &mut File::open(&source)
                        .with_context(|| format!("open {}", source.display()))?,
                    &mut hasher,
                )?;
                planned.push((
                    Entry::File {
                        path,
                        size,
                        sha256: hasher.hex(),
                        executable: meta.permissions().mode() & 0o111 != 0,
                    },
                    Some(source),
                ));
            }
            if planned.len() > MAX_ENTRIES {
                return Err(refuse(
                    PACK,
                    format!("more than {MAX_ENTRIES} entries to pack"),
                ));
            }
        }
    }
    Ok(())
}

/// Check every rule of the schema and `docs/session-packet.md` that a manifest
/// alone can show. `pack` runs it on what it is about to write, `unpack` on
/// what it read.
pub(crate) fn validate(m: &Manifest) -> Result<()> {
    ensure!(m.format == FORMAT, "format is {:?}, not {FORMAT}", m.format);
    ensure!(
        valid_harness(&m.harness),
        "harness {:?} must match ^[a-z][a-z0-9_]{{0,63}}$",
        m.harness
    );
    if let Some(id) = &m.harness_session_id {
        ensure!(
            valid_session_id(id),
            "harness_session_id {id:?} must match ^[A-Za-z0-9][A-Za-z0-9._:-]{{0,255}}$"
        );
    }
    ensure!(
        m.entries.len() <= MAX_ENTRIES,
        "more than {MAX_ENTRIES} entries"
    );
    let symlinks: BTreeSet<&str> = m
        .entries
        .iter()
        .filter(|e| matches!(e, Entry::Symlink { .. }))
        .map(Entry::path)
        .collect();
    let mut dirs: BTreeSet<&str> = BTreeSet::new();
    let mut total: u64 = 0;
    let mut previous: Option<&str> = None;
    for entry in &m.entries {
        let path = entry.path();
        check_path(path)?;
        if let Some(prev) = previous {
            ensure!(prev < path, "entries are not sorted and unique at {path}");
        }
        previous = Some(path);
        let parent = &path[..path.rfind('/').expect("checked")];
        ensure!(
            (Tree::of(parent).is_some() && !parent.contains('/')) || dirs.contains(parent),
            "{path}: parent {parent} is not a dir entry"
        );
        match entry {
            Entry::Dir { .. } => {
                dirs.insert(path);
            }
            Entry::File { size, sha256, .. } => {
                ensure!(
                    sha256.len() == 64
                        && sha256
                            .bytes()
                            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
                    "{path}: sha256 is not 64 lowercase hex digits"
                );
                total = total.saturating_add(*size);
            }
            Entry::Symlink { target, .. } => check_link(path, target, &symlinks)?,
        }
    }
    ensure!(
        total <= MAX_PACKET_BYTES,
        "entries total more than {MAX_PACKET_BYTES} bytes"
    );
    if let Some(t) = &m.transcript {
        let full = format!("harness/{t}");
        check_path(&full).with_context(|| format!("transcript {t:?}"))?;
        ensure!(
            m.entries
                .iter()
                .any(|e| matches!(e, Entry::File { path, .. } if *path == full)),
            "transcript {t:?} is not a file in the packet"
        );
    }
    Ok(())
}

fn valid_harness(s: &str) -> bool {
    let mut bytes = s.bytes();
    s.len() <= 64
        && bytes.next().is_some_and(|b| b.is_ascii_lowercase())
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn valid_session_id(s: &str) -> bool {
    let mut bytes = s.bytes();
    s.len() <= 256
        && bytes.next().is_some_and(|b| b.is_ascii_alphanumeric())
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

/// An entry path: `workspace/…` or `harness/…`, `/`-separated, with no empty,
/// `.` or `..` component and no NUL.
fn check_path(path: &str) -> Result<()> {
    ensure!(
        path.len() <= MAX_PATH_BYTES,
        "path longer than {MAX_PATH_BYTES} bytes"
    );
    ensure!(!path.contains('\0'), "{path:?}: NUL in path");
    ensure!(
        !path.starts_with('/'),
        "{path:?}: absolute paths are refused"
    );
    let mut parts = path.split('/');
    ensure!(
        matches!(parts.next(), Some("workspace" | "harness")),
        "{path:?}: not under workspace/ or harness/"
    );
    let mut depth = 0;
    for part in parts {
        ensure!(
            !matches!(part, "" | "." | ".."),
            "{path:?}: empty, . or .. component"
        );
        depth += 1;
    }
    ensure!(depth > 0, "{path:?}: names a tree root, not an entry");
    Ok(())
}

/// A symlink target must be relative and resolve, lexically, inside the
/// link's own tree, without passing through another symlink in the packet
/// (whose own target a `..` after it would climb out of).
fn check_link(path: &str, target: &str, symlinks: &BTreeSet<&str>) -> Result<()> {
    ensure!(
        !target.is_empty() && target.len() <= MAX_PATH_BYTES && !target.contains('\0'),
        "{path}: empty, oversized or NUL symlink target"
    );
    ensure!(
        !target.starts_with('/'),
        "{path}: symlink target {target:?} is absolute"
    );
    let mut resolved: Vec<&str> = path.split('/').collect();
    resolved.pop(); // the link's own name
    let parts: Vec<&str> = target
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    for (i, part) in parts.iter().enumerate() {
        if *part == ".." {
            ensure!(
                resolved.len() > 1,
                "{path}: symlink target {target:?} escapes its tree"
            );
            resolved.pop();
            continue;
        }
        resolved.push(part);
        ensure!(
            i + 1 == parts.len() || !symlinks.contains(resolved.join("/").as_str()),
            "{path}: symlink target {target:?} passes through another symlink"
        );
    }
    Ok(())
}

pub(crate) fn unpack(opts: &UnpackOptions) -> Result<Unpacked> {
    let expect = opts.expect.as_str();
    let well_formed = expect.strip_prefix("sha256:").is_some_and(|h| {
        h.len() == 64 && h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    });
    if !well_formed {
        return Err(usage(
            UNPACK,
            format!("--expect {expect:?} is not sha256:<64 lowercase hex>"),
        ));
    }
    let mut file =
        File::open(&opts.packet).with_context(|| format!("open {}", opts.packet.display()))?;
    let len = file.metadata()?.len();
    if len > MAX_PACKET_BYTES {
        return Err(refuse(
            UNPACK,
            format!("packet is {len} bytes, over the {MAX_PACKET_BYTES}-byte limit"),
        ));
    }
    // The digest is checked before anything is written.
    let mut whole = Hashing::new(io::sink(), MAX_PACKET_BYTES);
    io::copy(&mut (&mut file).take(MAX_PACKET_BYTES + 1), &mut whole)?;
    let digest = format!("sha256:{}", whole.hex());
    if digest != expect {
        return Err(refuse(
            UNPACK,
            format!("packet digest {digest} does not match --expect {expect}"),
        ));
    }
    file.rewind()?;
    // Hashed again while it is read, so a packet replaced after the first
    // pass is caught before anything is moved into place.
    let mut reader = tar::Reader::new(Hashing::new(io::BufReader::new(file), MAX_PACKET_BYTES));

    let manifest = read_manifest(&mut reader).map_err(|e| refuse(UNPACK, format!("{e:#}")))?;
    let has_harness = manifest
        .entries
        .iter()
        .any(|e| Tree::of(e.path()) == Some(Tree::Harness));
    let harness_target = match (&opts.harness_state, has_harness) {
        (Some(dir), _) => Some(dir.as_path()),
        (None, true) => {
            return Err(usage(
                UNPACK,
                "the packet carries harness state; pass --harness-state DIR",
            ))
        }
        (None, false) => None,
    };
    let workspace = Target::open(&opts.workspace, opts.allow_nonempty)?;
    let harness = harness_target
        .map(|dir| Target::open(dir, opts.allow_nonempty))
        .transpose()?;

    let mut entries = manifest.entries.iter();
    loop {
        let header = reader
            .next()
            .map_err(|e| refuse(UNPACK, format!("{e:#}")))?;
        let (header, entry) = match (header, entries.next()) {
            (None, None) => break,
            (Some(h), Some(e)) if h == e.header() => (h, e),
            (Some(h), _) => {
                return Err(refuse(
                    UNPACK,
                    format!("tar entry {} does not match the manifest", h.path),
                ))
            }
            (None, Some(e)) => {
                return Err(refuse(
                    UNPACK,
                    format!("manifest entry {} is missing from the tar", e.path()),
                ))
            }
        };
        let (tree_root, rel) = header.path.split_once('/').expect("validated");
        let target = match Tree::of(tree_root) {
            Some(Tree::Workspace) => &workspace,
            _ => harness.as_ref().expect("harness target resolved above"),
        };
        let dest = target.staging.path().join(rel);
        match entry {
            Entry::Dir { .. } => {
                fs::create_dir(&dest).with_context(|| format!("create {}", dest.display()))?
            }
            Entry::Symlink { target, .. } => std::os::unix::fs::symlink(target, &dest)
                .with_context(|| format!("create {}", dest.display()))?,
            Entry::File {
                path,
                sha256,
                executable,
                ..
            } => {
                let out = File::create_new(&dest)
                    .with_context(|| format!("create {}", dest.display()))?;
                let mut sink = Hashing::new(io::BufWriter::new(&out), u64::MAX);
                reader
                    .body(&mut sink)
                    .map_err(|e| refuse(UNPACK, format!("{e:#}")))?;
                if &sink.hex() != sha256 {
                    return Err(refuse(
                        UNPACK,
                        format!("{path} does not match its manifest sha256"),
                    ));
                }
                sink.inner.flush()?;
                drop(sink);
                let mode = if *executable { 0o755 } else { 0o644 };
                out.set_permissions(fs::Permissions::from_mode(mode))?;
            }
        }
    }
    let input = reader.into_inner();
    if format!("sha256:{}", input.hex()) != expect {
        return Err(refuse(UNPACK, "packet changed while being unpacked"));
    }
    workspace.commit()?;
    if let Some(h) = harness {
        h.commit()?;
    }
    Ok(Unpacked { digest, manifest })
}

fn read_manifest<R: Read>(reader: &mut tar::Reader<R>) -> Result<Manifest> {
    let first = reader.next()?.context("empty packet")?;
    let tar::Kind::File { size, .. } = first.kind else {
        bail!("first entry is not {MANIFEST_NAME}");
    };
    ensure!(
        first.path == MANIFEST_NAME,
        "first entry is {}, not {MANIFEST_NAME}",
        first.path
    );
    ensure!(
        size <= MAX_MANIFEST_BYTES,
        "{MANIFEST_NAME} is over {MAX_MANIFEST_BYTES} bytes"
    );
    let mut bytes = Vec::with_capacity(size as usize);
    reader.body(&mut bytes)?;
    let manifest: Manifest =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {MANIFEST_NAME}"))?;
    // Only the canonical encoding is accepted, which also refuses `null` for
    // an absent optional and any field order or whitespace `pack` would not write.
    ensure!(
        serde_json::to_vec(&manifest)? == bytes,
        "{MANIFEST_NAME} is not in canonical form"
    );
    validate(&manifest)?;
    Ok(manifest)
}

/// An unpack destination: entries are written into a staging directory inside
/// it (same filesystem), then moved into place only after every check passed.
struct Target {
    dir: PathBuf,
    staging: tempfile::TempDir,
}

impl Target {
    fn open(dir: &Path, allow_nonempty: bool) -> Result<Target> {
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        if !allow_nonempty && fs::read_dir(dir)?.next().is_some() {
            return Err(usage(
                UNPACK,
                format!(
                    "{} is not empty (pass --allow-nonempty to unpack over it)",
                    dir.display()
                ),
            ));
        }
        let staging = tempfile::Builder::new()
            .prefix(".pillbox-unpack-")
            .tempdir_in(dir)
            .with_context(|| format!("create a staging dir in {}", dir.display()))?;
        Ok(Target {
            dir: dir.to_path_buf(),
            staging,
        })
    }

    fn commit(self) -> Result<()> {
        for child in fs::read_dir(self.staging.path())? {
            let child = child?;
            merge_move(&child.path(), &self.dir.join(child.file_name()))?;
        }
        Ok(())
    }
}

/// Move `src` to `dst`. A directory merges into an existing real directory;
/// anything else replaces what is at `dst` (a symlink there is removed, never
/// followed).
fn merge_move(src: &Path, dst: &Path) -> Result<()> {
    let existing = match fs::symlink_metadata(dst) {
        Ok(meta) => Some(meta),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("stat {}", dst.display())),
    };
    let src_is_dir = fs::symlink_metadata(src)?.is_dir();
    match existing {
        None => {}
        Some(meta) if meta.is_dir() && src_is_dir => {
            for child in fs::read_dir(src)? {
                let child = child?;
                merge_move(&child.path(), &dst.join(child.file_name()))?;
            }
            fs::remove_dir(src)?;
            return Ok(());
        }
        Some(meta) if meta.is_dir() => fs::remove_dir_all(dst)?,
        Some(_) => fs::remove_file(dst)?,
    }
    fs::rename(src, dst).with_context(|| format!("move into {}", dst.display()))
}

/// A reader or writer that SHA-256-hashes and counts the bytes through it, and
/// fails once more than `limit` have passed.
struct Hashing<T> {
    inner: T,
    hasher: Sha256,
    count: u64,
    limit: u64,
}

impl<T> Hashing<T> {
    fn new(inner: T, limit: u64) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            count: 0,
            limit,
        }
    }

    fn hex(&self) -> String {
        format!("{:x}", self.hasher.clone().finalize())
    }

    fn account(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.count += bytes.len() as u64;
        if self.count > self.limit {
            return Err(io::Error::other(format!(
                "packet exceeds the {}-byte limit",
                self.limit
            )));
        }
        self.hasher.update(bytes);
        Ok(())
    }
}

impl<T: Write> Write for Hashing<T> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.account(&buf[..n])?;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<T: Read> Read for Hashing<T> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.account(&buf[..n])?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests;
