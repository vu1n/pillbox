//! `pillbox bookmark …` handlers.

use anyhow::Result;

use crate::bookmarks::{self, Bookmark};
use crate::cli::BookmarkAction;
use crate::errors::PillboxError;
use crate::pillbox::Pillbox;
use crate::workspace::SnapshotHandle;

pub(crate) fn dispatch(resolved: &Pillbox, action: BookmarkAction) -> Result<()> {
    match action {
        BookmarkAction::List { json } => list(resolved, json),
        BookmarkAction::Show { name, json } => show(resolved, &name, json),
        BookmarkAction::Set { name, snapshot } => set(resolved, &name, snapshot.as_deref()),
        BookmarkAction::Rm { name } => rm(resolved, &name),
    }
}

fn list(resolved: &Pillbox, json: bool) -> Result<()> {
    let bookmarks = bookmarks::list(resolved)?;
    if json {
        println!("{}", list_json(resolved, &bookmarks));
        return Ok(());
    }
    if bookmarks.is_empty() {
        println!("(no bookmarks for `{}`)", resolved.display_name());
        println!();
        println!("Create one with: pillbox bookmark set main latest");
        return Ok(());
    }
    println!("Bookmarks for `{}`:", resolved.display_name());
    for b in bookmarks {
        println!(
            "  {:<24}  {}  updated {}",
            b.name,
            short(&b.snapshot),
            b.updated_at
        );
    }
    Ok(())
}

fn show(resolved: &Pillbox, name: &str, json: bool) -> Result<()> {
    let bookmark = bookmarks::get(resolved, name)?.ok_or_else(|| {
        PillboxError::runtime("bookmark show", format!("bookmark `{name}` not found"))
            .with_next("pillbox bookmark list")
    })?;
    if json {
        println!("{}", show_json(&bookmark));
        return Ok(());
    }
    println!("Bookmark {}", bookmark.name);
    println!("  snapshot:   {}", bookmark.snapshot);
    println!("  created_at: {}", bookmark.created_at);
    println!("  updated_at: {}", bookmark.updated_at);
    Ok(())
}

fn set(resolved: &Pillbox, name: &str, snapshot: Option<&str>) -> Result<()> {
    let bookmark = bookmarks::set(resolved, name, snapshot)?;
    println!(
        "pillbox: ✓ bookmark `{}` -> {}",
        bookmark.name,
        short(&bookmark.snapshot)
    );
    Ok(())
}

fn rm(resolved: &Pillbox, name: &str) -> Result<()> {
    match bookmarks::delete(resolved, name)? {
        true => println!("pillbox: ✓ bookmark `{name}` removed"),
        false => println!("(no bookmark named `{name}`)"),
    }
    Ok(())
}

fn bookmark_value(bookmark: &Bookmark) -> serde_json::Value {
    let mut o = serde_json::Map::new();
    o.insert(
        "name".into(),
        serde_json::Value::String(bookmark.name.clone()),
    );
    o.insert(
        "snapshot".into(),
        serde_json::Value::String(bookmark.snapshot.clone()),
    );
    o.insert(
        "short".into(),
        serde_json::Value::String(short(&bookmark.snapshot)),
    );
    o.insert(
        "created_at".into(),
        serde_json::Value::String(bookmark.created_at.clone()),
    );
    o.insert(
        "updated_at".into(),
        serde_json::Value::String(bookmark.updated_at.clone()),
    );
    serde_json::Value::Object(o)
}

fn list_json(resolved: &Pillbox, bookmarks: &[Bookmark]) -> String {
    let arr = bookmarks.iter().map(bookmark_value).collect();
    crate::paths::json_v1(vec![
        (
            "pillbox",
            serde_json::Value::String(resolved.display_name().into()),
        ),
        ("bookmarks", serde_json::Value::Array(arr)),
    ])
}

fn show_json(bookmark: &Bookmark) -> String {
    crate::paths::json_v1(vec![("bookmark", bookmark_value(bookmark))])
}

fn short(snapshot: &str) -> String {
    SnapshotHandle::new(snapshot).short().to_string()
}
