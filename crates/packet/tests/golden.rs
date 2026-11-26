//! A packet pillbox 29d5306 wrote (`pillbox checkpoint pack`, before this crate
//! existed) must still unpack, and repacking what it restored must give the
//! same bytes and digest: the format is frozen, not just self-consistent.
//!
//! The fixture's trees hold a nested file, an executable, two relative
//! symlinks, an empty directory and a Claude transcript; the manifest carries
//! `harness`, `harness_session_id` and `transcript`.

use std::fs;
use std::path::PathBuf;

use pillbox_packet::{pack, unpack, PackOptions, UnpackOptions, FORMAT};
use tempfile::TempDir;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/golden-29d5306")
        .join(name)
}

#[test]
fn packet_from_29d5306_unpacks_and_repacks_byte_identical() {
    let golden = fixture("packet");
    let digest = fs::read_to_string(fixture("digest"))
        .unwrap()
        .trim()
        .to_string();
    let t = TempDir::new().unwrap();
    let (ws, hs) = (t.path().join("ws"), t.path().join("hs"));

    let unpacked = unpack(&UnpackOptions {
        packet: golden.clone(),
        expect: digest.clone(),
        workspace: ws.clone(),
        harness_state: Some(hs.clone()),
        allow_nonempty: false,
    })
    .unwrap();
    assert_eq!(unpacked.digest, digest);
    let m = &unpacked.manifest;
    assert_eq!(m.format, FORMAT);
    assert_eq!(m.harness, "claude_code");
    assert_eq!(m.harness_session_id.as_deref(), Some("s1"));
    assert_eq!(m.transcript.as_deref(), Some("projects/p/s1.jsonl"));
    assert_eq!(
        fs::read_link(ws.join("bin/link")).unwrap(),
        PathBuf::from("run")
    );
    assert!(ws.join("empty").is_dir());
    assert!(ws.join("src/nested/lib.rs").is_file());

    let out = t.path().join("re.packet");
    let packed = pack(&PackOptions {
        workspace: ws,
        harness_state: Some(hs),
        harness: m.harness.clone(),
        harness_session_id: m.harness_session_id.clone(),
        transcript: m.transcript.clone(),
        out: out.clone(),
        skip_dirs: Vec::new(),
    })
    .unwrap();
    assert_eq!(packed.digest, digest);
    assert_eq!(&packed.manifest, m);
    assert!(packed.excluded.is_empty());
    assert_eq!(fs::read(&out).unwrap(), fs::read(&golden).unwrap());
}
