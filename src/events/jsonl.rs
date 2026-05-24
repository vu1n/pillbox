//! JSONL sink — appends one event line to `<pillbox>/events.jsonl`.
//! The always-on sink: every emit writes here regardless of webhook /
//! OTel configuration. JSON renderer also lives here because the
//! webhook sink reuses it for its POST body (`emit_session_event` in
//! [`super`] hands the rendered payload to both).

use std::fs;

use anyhow::{Context, Result};

use super::{build_attributes, events_path, AttrValue, EventType};
use crate::paths;
use crate::pillbox::Pillbox;
use crate::session::Session;

pub(super) fn sink_emit(pb: &Pillbox, payload: &str) -> Result<()> {
    let path = events_path(pb);
    // Ensure the state dir exists *and* is 0700. Most callers run after
    // a pillbox command that's already touched it, but emission
    // shouldn't depend on a happens-before with init — a fresh isolated
    // test environment or a race against a deleted state dir shouldn't
    // lose the event. Pin the perms here too so `events.jsonl` doesn't
    // end up parented by a 0755 directory if some adversarial code path
    // created the state dir without going through `Pillbox::subdir`.
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        paths::ensure_mode_0700(parent)?;
    }
    // Single `write_all` of `body + "\n"`: stdlib turns this into one
    // `write(2)` syscall on Unix, and `O_APPEND` makes that write
    // atomically positioned at end-of-file. For lines under `PIPE_BUF`
    // (4096 on Linux, typically larger elsewhere) a concurrent
    // `--follow` reader is guaranteed to see whole lines, never a
    // partial mid-line tear.
    let mut line = String::with_capacity(payload.len() + 1);
    line.push_str(payload);
    line.push('\n');
    paths::append_private_file(&path, line.as_bytes())?;
    Ok(())
}

pub(super) fn build_event_json(
    ty: &EventType,
    session_id: &str,
    session: Option<&Session>,
) -> String {
    let map: serde_json::Map<String, serde_json::Value> = build_attributes(ty, session_id, session)
        .into_iter()
        .map(|(k, v)| (k.to_string(), attr_to_json(v)))
        .collect();
    serde_json::Value::Object(map).to_string()
}

fn attr_to_json(v: Option<AttrValue>) -> serde_json::Value {
    match v {
        Some(AttrValue::Str(s)) => serde_json::Value::String(s),
        Some(AttrValue::Int(i)) => serde_json::Value::Number(i.into()),
        None => serde_json::Value::Null,
    }
}
