//! Minimal interactive prompts for the `pillbox new -i` wizard — a line-with-default
//! and a numbered single-select, dependency-free (stdin + stdout, no TUI framework;
//! crossterm stays reserved for the pty front-end). Callers gate every prompt on
//! [`interactive`] so a piped/redirected run never blocks waiting on stdin.

use std::io::{self, IsTerminal, Write};

/// True iff BOTH stdin and stdout are terminals — the gate for any prompt. A run
/// with redirected input/output must not block; the caller falls back to defaults.
pub(crate) fn interactive() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

/// Prompt for one line; empty input returns `default` (shown in `[..]` when non-empty).
pub(crate) fn line(label: &str, default: &str) -> io::Result<String> {
    if default.is_empty() {
        print!("  {label}: ");
    } else {
        print!("  {label} [{default}]: ");
    }
    io::stdout().flush()?;
    let mut s = String::new();
    io::stdin().read_line(&mut s)?;
    let s = s.trim();
    Ok(if s.is_empty() {
        default.to_string()
    } else {
        s.to_string()
    })
}

/// Numbered single-select; empty input picks `options[default_idx]`, an out-of-range
/// entry re-prompts. Returns the chosen option verbatim (the caller handles any
/// sentinel like a trailing "custom…" entry).
pub(crate) fn select(label: &str, options: &[&str], default_idx: usize) -> io::Result<String> {
    println!("  {label}:");
    for (i, o) in options.iter().enumerate() {
        println!("    {}) {o}", i + 1);
    }
    loop {
        print!("    [{}]: ", default_idx + 1);
        io::stdout().flush()?;
        let mut s = String::new();
        io::stdin().read_line(&mut s)?;
        let s = s.trim();
        if s.is_empty() {
            return Ok(options[default_idx].to_string());
        }
        match s.parse::<usize>() {
            Ok(n) if (1..=options.len()).contains(&n) => return Ok(options[n - 1].to_string()),
            _ => println!("    (enter 1–{})", options.len()),
        }
    }
}
