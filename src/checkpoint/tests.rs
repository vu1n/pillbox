use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};

use serde_json::Value;
use tempfile::TempDir;

use super::*;

const SCHEMA: &str = include_str!("../../proto/pillbox/session-packet-1.schema.json");

fn write(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

/// A workspace and a Claude-shaped harness home with one transcript.
fn fixture(root: &Path) -> (PathBuf, PathBuf) {
    let ws = root.join("ws");
    let home = root.join("claude");
    write(&ws.join("README.md"), "hello\n");
    write(&ws.join("src/main.rs"), "fn main() {}\n");
    write(&ws.join("bin/run.sh"), "#!/bin/sh\necho hi\n");
    fs::set_permissions(ws.join("bin/run.sh"), fs::Permissions::from_mode(0o700)).unwrap();
    fs::create_dir_all(ws.join("empty")).unwrap();
    symlink("../README.md", ws.join("src/readme")).unwrap();
    write(&ws.join("auth.json"), "{\"fixture\":true}\n");
    write(&ws.join(".credentials.json"), "{\"secret\":1}\n");
    write(
        &home.join("projects/-workspace-repo/sid-1.jsonl"),
        "{\"type\":\"user\"}\n",
    );
    write(&home.join(".credentials.json"), "{\"claudeAiOauth\":{}}\n");
    write(&home.join("nested/auth.json"), "{\"token\":\"x\"}\n");
    write(&home.join("settings.json"), "{}\n");
    (ws, home)
}

fn pack_opts(ws: &Path, home: Option<&Path>, out: &Path) -> PackOptions {
    PackOptions {
        workspace: ws.to_path_buf(),
        harness_state: home.map(Path::to_path_buf),
        harness: "claude_code".into(),
        harness_session_id: home.map(|_| "sid-1".into()),
        transcript: home.map(|_| "projects/-workspace-repo/sid-1.jsonl".into()),
        out: out.to_path_buf(),
    }
}

fn unpack_opts(packet: &Path, expect: &str, ws: &Path, home: Option<&Path>) -> UnpackOptions {
    UnpackOptions {
        packet: packet.to_path_buf(),
        expect: expect.into(),
        workspace: ws.to_path_buf(),
        harness_state: home.map(Path::to_path_buf),
        allow_nonempty: false,
    }
}

fn digest_of(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn is_empty_or_missing(dir: &Path) -> bool {
    !dir.exists() || fs::read_dir(dir).unwrap().next().is_none()
}

/// Write a hand-built packet: `manifest` as packet.json, then `body`'s entries.
fn craft(
    dir: &Path,
    manifest: &[u8],
    body: impl FnOnce(&mut tar::Writer<Vec<u8>>),
) -> (PathBuf, String) {
    let mut w = tar::Writer::new(Vec::new());
    w.file(
        MANIFEST_NAME,
        false,
        manifest.len() as u64,
        &mut &manifest[..],
    )
    .unwrap();
    body(&mut w);
    let bytes = w.finish().unwrap();
    let path = dir.join("crafted.packet");
    fs::write(&path, &bytes).unwrap();
    (path, digest_of(&bytes))
}

#[test]
fn packing_the_same_tree_twice_gives_the_same_digest() {
    let t = TempDir::new().unwrap();
    let (ws, home) = fixture(t.path());
    let a = pack(&pack_opts(&ws, Some(&home), &t.path().join("a.packet"))).unwrap();
    // A copy elsewhere, written later (different mtimes, inode order), with a
    // different umask-like mode on a non-executable file.
    let copy = t.path().join("copy");
    let (ws2, home2) = fixture(&copy);
    fs::set_permissions(ws2.join("README.md"), fs::Permissions::from_mode(0o600)).unwrap();
    let b = pack(&pack_opts(&ws2, Some(&home2), &t.path().join("b.packet"))).unwrap();
    assert_eq!(a.digest, b.digest);
    assert_eq!(a.bytes, b.bytes);
    let bytes = fs::read(t.path().join("a.packet")).unwrap();
    assert_eq!(bytes, fs::read(t.path().join("b.packet")).unwrap());
    assert_eq!(digest_of(&bytes), a.digest);
    assert_eq!(bytes.len() as u64, a.bytes);
}

#[test]
fn round_trip_restores_both_trees() {
    let t = TempDir::new().unwrap();
    let (ws, home) = fixture(t.path());
    let out = t.path().join("p.packet");
    let packed = pack(&pack_opts(&ws, Some(&home), &out)).unwrap();
    // packet.json is the first entry.
    let mut r = tar::Reader::new(File::open(&out).unwrap());
    assert_eq!(r.next().unwrap().unwrap().path, MANIFEST_NAME);

    let (ws_out, home_out) = (t.path().join("r/ws"), t.path().join("r/home"));
    let got = unpack(&unpack_opts(&out, &packed.digest, &ws_out, Some(&home_out))).unwrap();
    assert_eq!(got.manifest, packed.manifest);
    assert_eq!(got.manifest.harness_session_id.as_deref(), Some("sid-1"));
    for rel in ["README.md", "src/main.rs", "bin/run.sh", "auth.json"] {
        assert_eq!(
            fs::read(ws.join(rel)).unwrap(),
            fs::read(ws_out.join(rel)).unwrap(),
            "{rel}"
        );
    }
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&ws_out.join("bin/run.sh")), 0o755);
    assert_eq!(mode(&ws_out.join("README.md")), 0o644);
    assert!(ws_out.join("empty").is_dir());
    assert_eq!(
        fs::read_link(ws_out.join("src/readme")).unwrap(),
        PathBuf::from("../README.md")
    );
    assert_eq!(
        fs::read(home_out.join(got.manifest.transcript.unwrap())).unwrap(),
        b"{\"type\":\"user\"}\n"
    );
    // No staging directory is left behind.
    let names: Vec<_> = fs::read_dir(&ws_out)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(names
        .iter()
        .all(|n| !n.to_string_lossy().starts_with(".pillbox-unpack-")));
}

#[test]
fn credential_files_are_never_packed() {
    let t = TempDir::new().unwrap();
    let (ws, home) = fixture(t.path());
    let out = t.path().join("p.packet");
    let packed = pack(&pack_opts(&ws, Some(&home), &out)).unwrap();
    let paths: Vec<&str> = packed.manifest.entries.iter().map(Entry::path).collect();
    for gone in [
        "workspace/.credentials.json",
        "harness/.credentials.json",
        "harness/nested/auth.json",
    ] {
        assert!(!paths.contains(&gone), "{gone} was packed");
        assert!(packed.excluded.contains(&Excluded {
            path: gone.into(),
            reason: "credential"
        }));
    }
    // A workspace `auth.json` is source, not an agent home's credential.
    assert!(paths.contains(&"workspace/auth.json"));
    let bytes = fs::read(&out).unwrap();
    let hay = String::from_utf8_lossy(&bytes);
    assert!(
        !hay.contains("claudeAiOauth") && !hay.contains("\"secret\"") && !hay.contains("\"token\"")
    );
}

#[test]
fn digest_mismatch_is_refused_before_writing() {
    let t = TempDir::new().unwrap();
    let (ws, home) = fixture(t.path());
    let out = t.path().join("p.packet");
    pack(&pack_opts(&ws, Some(&home), &out)).unwrap();
    let target = t.path().join("r/ws");
    let wrong = format!("sha256:{}", "0".repeat(64));
    let err = unpack(&unpack_opts(
        &out,
        &wrong,
        &target,
        Some(&t.path().join("r/h")),
    ))
    .unwrap_err();
    assert!(
        format!("{err:#}").contains("does not match --expect"),
        "{err:#}"
    );
    assert!(
        !target.exists(),
        "target was created before the digest check"
    );
    // A malformed --expect is a usage error.
    assert!(unpack(&unpack_opts(&out, "sha256:abc", &target, None)).is_err());
}

#[test]
fn tampered_entry_is_refused() {
    let t = TempDir::new().unwrap();
    let (ws, home) = fixture(t.path());
    let out = t.path().join("p.packet");
    pack(&pack_opts(&ws, Some(&home), &out)).unwrap();
    let mut bytes = fs::read(&out).unwrap();
    let at = bytes
        .windows(13)
        .position(|w| w == b"fn main() {}\n")
        .unwrap();
    bytes[at] = b'F';
    fs::write(&out, &bytes).unwrap();
    // The caller's digest matches the tampered bytes; the manifest does not.
    let target = t.path().join("r/ws");
    let err = unpack(&unpack_opts(
        &out,
        &digest_of(&bytes),
        &target,
        Some(&t.path().join("r/h")),
    ))
    .unwrap_err();
    assert!(
        format!("{err:#}").contains("does not match its manifest sha256"),
        "{err:#}"
    );
    assert!(is_empty_or_missing(&target) || only_staging(&target));
}

/// After a refused unpack only the (dropped) staging dir may have existed.
fn only_staging(dir: &Path) -> bool {
    fs::read_dir(dir).unwrap().all(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".pillbox-unpack-")
    })
}

fn manifest_json(entries: Value) -> Vec<u8> {
    let m: Manifest = serde_json::from_value(serde_json::json!({
        "format": FORMAT, "harness": "claude_code", "entries": entries,
    }))
    .unwrap();
    serde_json::to_vec(&m).unwrap()
}

/// Writes a crafted packet's entries after packet.json.
type Body = Box<dyn FnOnce(&mut tar::Writer<Vec<u8>>)>;

#[test]
fn traversal_and_unlisted_entries_are_refused() {
    let t = TempDir::new().unwrap();
    let target = t.path().join("r");
    let cases: Vec<(Value, Body, &str)> = vec![
        (
            serde_json::json!([{"kind":"dir","path":"workspace/../evil"}]),
            Box::new(|w| w.dir("workspace/../evil").unwrap()),
            "component",
        ),
        (
            serde_json::json!([{"kind":"dir","path":"/etc"}]),
            Box::new(|w| w.dir("/etc").unwrap()),
            "absolute",
        ),
        (
            serde_json::json!([{"kind":"dir","path":"other/x"}]),
            Box::new(|w| w.dir("other/x").unwrap()),
            "not under workspace",
        ),
        (
            serde_json::json!([]),
            Box::new(|w| w.dir("workspace/extra").unwrap()),
            "does not match the manifest",
        ),
        (
            serde_json::json!([{"kind":"dir","path":"workspace/a"}]),
            Box::new(|_| {}),
            "missing from the tar",
        ),
        (
            serde_json::json!([{"kind":"symlink","path":"workspace/l","target":"../../etc"}]),
            Box::new(|w| w.symlink("workspace/l", "../../etc").unwrap()),
            "escapes its tree",
        ),
        (
            serde_json::json!([{"kind":"symlink","path":"workspace/l","target":"/etc/passwd"}]),
            Box::new(|w| w.symlink("workspace/l", "/etc/passwd").unwrap()),
            "absolute",
        ),
        (
            serde_json::json!([{"kind":"file","path":"workspace/a/f","size":0,
                "sha256":"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
                "executable":false}]),
            Box::new(|w| w.file("workspace/a/f", false, 0, &mut &b""[..]).unwrap()),
            "is not a dir entry",
        ),
    ];
    for (entries, body, want) in cases {
        let (packet, digest) = craft(t.path(), &manifest_json(entries.clone()), body);
        let err = unpack(&unpack_opts(&packet, &digest, &target, None)).unwrap_err();
        assert!(format!("{err:#}").contains(want), "{entries}: {err:#}");
        assert!(
            is_empty_or_missing(&target) || only_staging(&target),
            "{entries}"
        );
    }
}

#[test]
fn non_canonical_manifest_with_null_is_refused() {
    let t = TempDir::new().unwrap();
    let raw = format!(
        r#"{{"format":"{FORMAT}","harness":"claude_code","harness_session_id":null,"entries":[]}}"#
    );
    let (packet, digest) = craft(t.path(), raw.as_bytes(), |_| {});
    let err = unpack(&unpack_opts(&packet, &digest, &t.path().join("r"), None)).unwrap_err();
    assert!(format!("{err:#}").contains("canonical"), "{err:#}");
    let extra = format!(r#"{{"format":"{FORMAT}","harness":"claude_code","entries":[],"x":1}}"#);
    let (packet, digest) = craft(t.path(), extra.as_bytes(), |_| {});
    assert!(unpack(&unpack_opts(&packet, &digest, &t.path().join("r"), None)).is_err());
}

#[test]
fn escaping_symlinks_are_refused_at_pack() {
    for (name, target) in [
        ("up", "../../outside"),
        ("abs", "/etc/passwd"),
        ("cross", "../../harness/x"),
    ] {
        let t = TempDir::new().unwrap();
        let ws = t.path().join("ws");
        fs::create_dir_all(ws.join("sub")).unwrap();
        symlink(target, ws.join("sub").join(name)).unwrap();
        let err = pack(&pack_opts(&ws, None, &t.path().join("p"))).unwrap_err();
        assert!(
            format!("{err:#}").contains("symlink target"),
            "{name}: {err:#}"
        );
        assert!(!t.path().join("p").exists());
    }
    // `d -> .` is inside, but `e -> d/..` climbs out through it.
    let t = TempDir::new().unwrap();
    let ws = t.path().join("ws");
    fs::create_dir_all(&ws).unwrap();
    symlink(".", ws.join("d")).unwrap();
    symlink("d/..", ws.join("e")).unwrap();
    let err = pack(&pack_opts(&ws, None, &t.path().join("p"))).unwrap_err();
    assert!(
        format!("{err:#}").contains("passes through another symlink"),
        "{err:#}"
    );
}

#[test]
fn non_empty_target_is_refused_unless_allowed() {
    let t = TempDir::new().unwrap();
    let (ws, _) = fixture(t.path());
    let out = t.path().join("p.packet");
    let packed = pack(&pack_opts(&ws, None, &out)).unwrap();
    let target = t.path().join("r");
    write(&target.join("README.md"), "old\n");
    write(&target.join("keep.txt"), "kept\n");
    let err = unpack(&unpack_opts(&out, &packed.digest, &target, None)).unwrap_err();
    assert!(format!("{err:#}").contains("not empty"), "{err:#}");
    assert_eq!(
        fs::read_to_string(target.join("README.md")).unwrap(),
        "old\n"
    );

    let mut opts = unpack_opts(&out, &packed.digest, &target, None);
    opts.allow_nonempty = true;
    unpack(&opts).unwrap();
    assert_eq!(
        fs::read_to_string(target.join("README.md")).unwrap(),
        "hello\n"
    );
    assert_eq!(
        fs::read_to_string(target.join("keep.txt")).unwrap(),
        "kept\n"
    );
}

#[test]
fn harness_state_without_a_target_is_refused() {
    let t = TempDir::new().unwrap();
    let (ws, home) = fixture(t.path());
    let out = t.path().join("p.packet");
    let packed = pack(&pack_opts(&ws, Some(&home), &out)).unwrap();
    let target = t.path().join("r");
    assert!(unpack(&unpack_opts(&out, &packed.digest, &target, None)).is_err());
    assert!(!target.exists());
}

#[test]
fn transcript_must_be_a_packed_file() {
    let t = TempDir::new().unwrap();
    let (ws, home) = fixture(t.path());
    let mut opts = pack_opts(&ws, Some(&home), &t.path().join("p"));
    opts.transcript = Some(".credentials.json".into());
    assert!(pack(&opts).is_err());
    opts.transcript = Some("../ws/README.md".into());
    assert!(pack(&opts).is_err());
}

// ---- the manifest against the checked-in JSON Schema ----

/// Validate `v` against `schema` for the keywords the packet schema uses.
/// `pattern` is not evaluated (no regex engine in the tree); `validate()`
/// enforces those rules in code and the tests above exercise them.
fn check(root: &Value, schema: &Value, v: &Value, at: &str) -> Result<(), String> {
    let s = schema.as_object().unwrap();
    if let Some(r) = s.get("$ref") {
        let name = r.as_str().unwrap().strip_prefix("#/$defs/").unwrap();
        return check(root, &root["$defs"][name], v, at);
    }
    if let Some(c) = s.get("const") {
        if c != v {
            return Err(format!("{at}: {v} is not {c}"));
        }
    }
    if let Some(t) = s.get("type").and_then(Value::as_str) {
        let ok = match t {
            "object" => v.is_object(),
            "array" => v.is_array(),
            "string" => v.is_string(),
            "integer" => v.is_u64() || v.is_i64(),
            "boolean" => v.is_boolean(),
            other => return Err(format!("{at}: unexpected type {other}")),
        };
        if !ok {
            return Err(format!("{at}: {v} is not {t}"));
        }
    }
    if let Some(n) = v.as_str() {
        let len = n.chars().count() as u64;
        if s.get("minLength")
            .and_then(Value::as_u64)
            .is_some_and(|m| len < m)
            || s.get("maxLength")
                .and_then(Value::as_u64)
                .is_some_and(|m| len > m)
        {
            return Err(format!("{at}: length {len} out of bounds"));
        }
    }
    if let Some(n) = v.as_u64() {
        if s.get("minimum")
            .and_then(Value::as_u64)
            .is_some_and(|m| n < m)
            || s.get("maximum")
                .and_then(Value::as_u64)
                .is_some_and(|m| n > m)
        {
            return Err(format!("{at}: {n} out of bounds"));
        }
    }
    if let Some(obj) = v.as_object() {
        let props = s.get("properties").and_then(Value::as_object);
        for req in s
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if !obj.contains_key(req.as_str().unwrap()) {
                return Err(format!("{at}: missing {req}"));
            }
        }
        for (k, val) in obj {
            match props.and_then(|p| p.get(k)) {
                Some(sub) => check(root, sub, val, &format!("{at}.{k}"))?,
                None if s.get("additionalProperties") == Some(&Value::Bool(false)) => {
                    return Err(format!("{at}: unknown field {k}"))
                }
                None => {}
            }
        }
    }
    if let Some(items) = v.as_array() {
        if s.get("maxItems")
            .and_then(Value::as_u64)
            .is_some_and(|m| items.len() as u64 > m)
        {
            return Err(format!("{at}: too many items"));
        }
        if let Some(item) = s.get("items") {
            for (i, x) in items.iter().enumerate() {
                check(root, item, x, &format!("{at}[{i}]"))?;
            }
        }
    }
    if let Some(one) = s.get("oneOf").and_then(Value::as_array) {
        let hits = one.iter().filter(|o| check(root, o, v, at).is_ok()).count();
        if hits != 1 {
            return Err(format!("{at}: {v} matches {hits} oneOf branches"));
        }
    }
    Ok(())
}

/// Every keyword in the schema is one `check` evaluates or a documented
/// exception, every object schema is closed, and nothing admits `null`.
fn audit(schema: &Value, at: &str) {
    const EVALUATED: &[&str] = &[
        "$ref",
        "const",
        "type",
        "minLength",
        "maxLength",
        "minimum",
        "maximum",
        "required",
        "properties",
        "additionalProperties",
        "maxItems",
        "items",
        "oneOf",
    ];
    const ANNOTATIONS: &[&str] = &["$schema", "$id", "$defs", "title", "description", "pattern"];
    let obj = schema.as_object().unwrap();
    for k in obj.keys() {
        assert!(
            EVALUATED.contains(&k.as_str()) || ANNOTATIONS.contains(&k.as_str()),
            "{at}: {k}"
        );
    }
    if obj.get("type") == Some(&Value::from("object")) {
        assert_eq!(
            obj.get("additionalProperties"),
            Some(&Value::Bool(false)),
            "{at} is open"
        );
    }
    assert_ne!(
        obj.get("type"),
        Some(&Value::from("null")),
        "{at} admits null"
    );
    assert!(
        !obj.get("type").is_some_and(Value::is_array),
        "{at}: union type"
    );
    for (k, v) in obj {
        match k.as_str() {
            "properties" | "$defs" => v
                .as_object()
                .unwrap()
                .iter()
                .for_each(|(n, s)| audit(s, &format!("{at}.{n}"))),
            "items" => audit(v, &format!("{at}[]")),
            "oneOf" => v.as_array().unwrap().iter().for_each(|s| audit(s, at)),
            _ => {}
        }
    }
}

#[test]
fn packet_json_validates_against_the_checked_in_schema() {
    let schema: Value = serde_json::from_str(SCHEMA).unwrap();
    audit(&schema, "#");
    assert_eq!(schema["properties"]["format"]["const"], FORMAT);
    assert_eq!(schema["properties"]["entries"]["maxItems"], MAX_ENTRIES);
    assert_eq!(
        schema["$defs"]["file"]["properties"]["size"]["maximum"],
        MAX_PACKET_BYTES
    );

    let t = TempDir::new().unwrap();
    let (ws, home) = fixture(t.path());
    let out = t.path().join("p.packet");
    pack(&pack_opts(&ws, Some(&home), &out)).unwrap();
    let mut r = tar::Reader::new(File::open(&out).unwrap());
    r.next().unwrap().unwrap();
    let mut raw = Vec::new();
    r.body(&mut raw).unwrap();
    let manifest: Value = serde_json::from_slice(&raw).unwrap();
    check(&schema, &schema, &manifest, "packet.json").unwrap();
    // Every entry kind is exercised, and optionals are present here.
    for kind in ["dir", "file", "symlink"] {
        assert!(manifest["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == kind));
    }
    assert!(manifest.get("transcript").is_some());

    // Without harness state the optionals are omitted, not null.
    let bare = pack(&pack_opts(&ws, None, &t.path().join("q"))).unwrap();
    let bare = serde_json::to_value(&bare.manifest).unwrap();
    assert!(bare.get("harness_session_id").is_none() && bare.get("transcript").is_none());
    check(&schema, &schema, &bare, "bare").unwrap();

    // The validator itself rejects what the schema forbids.
    let mut bad = manifest.clone();
    bad["harness_session_id"] = Value::Null;
    assert!(check(&schema, &schema, &bad, "null").is_err());
    let mut bad = manifest.clone();
    bad["entries"][0]["mtime"] = Value::from(0);
    assert!(check(&schema, &schema, &bad, "open").is_err());
}
