//! Grader resolution + the rubric marker protocol for `session score`.
//!
//! Splits the verifiable-reward machinery out of the session-lifecycle module:
//! how a `--cmd`/`--rubric` grader compiles to a shell command, and how its exit
//! code + output become a score/feedback/criteria verdict. The *executor* (host
//! vs `--in-sandbox` microVM) stays in the parent — this module is the pure
//! policy and owns the integrity boundary (see [`RUBRIC_MARKER`]).

use std::path::Path;

use anyhow::Result;

use crate::contract::Criterion;
use crate::errors::PillboxError;

/// Default §0 feedback cap — one grade can't bloat a log line.
const FEEDBACK_CAP: usize = 32 * 1024;

/// `\x1e` (RECORD SEPARATOR) prefixed marker line emitted once per criterion:
/// `\x1ePBCRIT\t<exit>\t<name-b64>\t<output-b64>`. The verdict (`<exit>`) is the
/// shell's `$?`, and the name/output are base64 — so a criterion's OWN output
/// (which the graded agent controls) is inert: it can't forge a marker line or
/// flip a verdict. This is the reward-channel integrity boundary — see
/// [`compile_rubric`]/[`parse_rubric_output`] and the forge-resistance test.
const RUBRIC_MARKER: &str = "\u{1e}PBCRIT\t";

/// What to grade with: one command, or a compiled rubric (named criteria).
pub(super) enum GraderSpec {
    Cmd(String),
    Rubric {
        criteria: Vec<(String, String)>,
        /// `grader` label recorded on the §0 event (e.g. `rubric:checks.txt`).
        label: String,
    },
}

impl GraderSpec {
    /// Resolve the mutually-exclusive `--cmd`/`--rubric` flags into a spec,
    /// reading + parsing the rubric file here. clap's ArgGroup guarantees exactly
    /// one is set, so the final arm is unreachable — surfaced, not `unreachable!`.
    pub(super) fn resolve(cmd: Option<&str>, rubric: Option<&Path>) -> Result<Self> {
        match (cmd, rubric) {
            (Some(c), None) => Ok(Self::Cmd(c.to_string())),
            (None, Some(path)) => {
                let text = std::fs::read_to_string(path).map_err(|e| {
                    PillboxError::runtime(
                        "session score",
                        format!("read rubric {}: {e}", path.display()),
                    )
                })?;
                Ok(Self::Rubric {
                    criteria: parse_rubric(&text)?,
                    label: format!("rubric:{}", path.display()),
                })
            }
            _ => Err(PillboxError::usage(
                "session score",
                "provide exactly one of --cmd or --rubric",
            )
            .into()),
        }
    }

    /// The shell command the executor runs (a rubric compiles to one script, so
    /// the executor is identical for both modes).
    pub(super) fn exec_command(&self) -> String {
        match self {
            Self::Cmd(c) => c.clone(),
            Self::Rubric { criteria, .. } => compile_rubric(criteria),
        }
    }

    /// The `grader` label for the `scored` event.
    pub(super) fn label(&self) -> String {
        match self {
            Self::Cmd(c) => c.clone(),
            Self::Rubric { label, .. } => label.clone(),
        }
    }
}

/// The verdict computed from a grader's `(exit_code, raw_output)`.
pub(super) struct GradeResult {
    pub(super) passed: bool,
    pub(super) score: f64,
    pub(super) feedback: String,
    pub(super) criteria: Vec<Criterion>,
}

/// Turn a grader's exit code + raw output into a verdict. A `--cmd` grade is
/// binary (exit 0 → 1.0); a `--rubric` grade parses per-criterion markers and
/// scores the passed fraction. `passed` is always exit-derived (the rubric script
/// exits nonzero iff any criterion failed), so it can't be forged from output.
pub(super) fn grade_result(spec: &GraderSpec, code: i32, raw: String) -> GradeResult {
    let passed = code == 0;
    match spec {
        GraderSpec::Cmd(_) => GradeResult {
            passed,
            score: if passed { 1.0 } else { 0.0 },
            feedback: cap_tail(raw, FEEDBACK_CAP),
            criteria: Vec::new(),
        },
        GraderSpec::Rubric { .. } => {
            let criteria = parse_rubric_output(&raw);
            if criteria.is_empty() {
                // No markers parsed → the compiled script failed before any
                // criterion (a shell error). Surface the raw output, score 0.
                GradeResult {
                    passed,
                    score: 0.0,
                    feedback: cap_tail(raw, FEEDBACK_CAP),
                    criteria,
                }
            } else {
                let hits = criteria.iter().filter(|c| c.passed).count();
                GradeResult {
                    passed,
                    score: hits as f64 / criteria.len() as f64,
                    feedback: render_rubric_summary(&criteria),
                    criteria,
                }
            }
        }
    }
}

/// Line the grader microVM prints right before it runs the grader, after boot,
/// network setup and the workspace mount succeeded. [`GRADER_STARTED_PRINTF`]
/// emits it (on its own line) from the guest's boot script.
const GRADER_STARTED_MARKER: &str = "\u{1e}PBSTART";

/// The guest shell command that prints [`GRADER_STARTED_MARKER`].
#[cfg_attr(not(feature = "libkrun"), allow(dead_code))]
pub(crate) const GRADER_STARTED_PRINTF: &str = r"printf '\n\036PBSTART\n'";

/// `sitecustomize.py` for an unprivileged (dropped) grader. With
/// `PYTHONSAFEPATH=1` Python no longer puts the cwd or script dir FIRST on
/// `sys.path`, so a worker's `pluggy.py`/`json.py` can't shadow the test
/// runner or the stdlib; this puts the script dir (if any) and the cwd back at
/// the END, so the project still imports from the workspace the way
/// `python3 -m pytest`, a `pytest` console script or `python3 script.py`
/// expects. No `;` or `'` (it rides a `'…'` shell string
/// in an `&&` chain).
const PY_BOOTSTRAP: &str = r#"import os
import sys
_a = sys.argv[0] if sys.argv else ""
_ds = [] if _a in ("", "-c", "-m") else [os.path.dirname(os.path.abspath(_a))]
for _d in _ds + [os.getcwd()]:
    if _d not in sys.path:
        sys.path.append(_d)
"#;

/// Where the guest writes [`PY_BOOTSTRAP`]: root-owned, so `nobody` can't edit it.
const PY_BOOTSTRAP_DIR: &str = "/tmp/.pillbox-py";

/// Guest setup that runs as root right before a grader drops to `nobody`: the
/// grader env (`HOME`, no bytecode, the Python bootstrap). An `&&` chain with
/// no `;`, so the caller's `mount && cd &&` prefix still gates the marker.
#[cfg_attr(not(feature = "libkrun"), allow(dead_code))]
pub(crate) fn dropped_grader_setup() -> String {
    format!(
        "export HOME=/tmp PYTHONDONTWRITEBYTECODE=1 PYTHONSAFEPATH=1 PYTHONPATH={PY_BOOTSTRAP_DIR} \
         && mkdir -p {PY_BOOTSTRAP_DIR} && printf '%s' {} > {PY_BOOTSTRAP_DIR}/sitecustomize.py \
         && chmod 755 {PY_BOOTSTRAP_DIR} && chmod 644 {PY_BOOTSTRAP_DIR}/sitecustomize.py",
        sh_quote(PY_BOOTSTRAP)
    )
}

/// Strip the grader microVM's console output down to what the grader printed.
/// No marker means the VM failed before the grader ran (no boot, no mount, a
/// setup step aborted) — an error, never a failing grade, so an unavailable VM
/// never reads as a worker that failed its tests.
#[cfg_attr(not(feature = "libkrun"), allow(dead_code))]
pub(super) fn grader_output_after_start(code: i32, raw: &str) -> Result<String> {
    let Some(at) = raw.find(GRADER_STARTED_MARKER) else {
        let tail: String = raw
            .lines()
            .rev()
            .take(10)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        return Err(PillboxError::runtime(
            "session score",
            format!("the grader microVM exited ({code}) before the grader started; nothing was graded. VM output:\n{tail}"),
        )
        .into());
    };
    let rest = &raw[at + GRADER_STARTED_MARKER.len()..];
    let rest = rest.strip_prefix('\r').unwrap_or(rest);
    Ok(rest.strip_prefix('\n').unwrap_or(rest).to_string())
}

/// Combine a grader's stdout+stderr into one feedback string (uncapped) — the raw
/// gradient the executor hands to [`grade_result`]. Kept RAW until after rubric
/// markers are parsed (capping tail-only would drop early criteria).
pub(super) fn combine_streams(stdout: &[u8], stderr: &[u8]) -> String {
    let mut s = String::from_utf8_lossy(stdout).into_owned();
    let err = String::from_utf8_lossy(stderr);
    if !err.trim().is_empty() {
        if !s.is_empty() {
            s.push('\n');
        }
        s.push_str(&err);
    }
    s
}

/// Keep the TAIL of `s` within `cap` bytes (pytest/cargo-test put the failure
/// summary last), on a char boundary. The single capping primitive — applied to
/// a whole-grade blob and to each rubric criterion's output.
fn cap_tail(mut s: String, cap: usize) -> String {
    if s.len() > cap {
        let mut cut = s.len() - cap;
        while !s.is_char_boundary(cut) {
            cut += 1;
        }
        s = format!("…[truncated {cut} leading bytes]\n{}", &s[cut..]);
    }
    s
}

/// Parse a rubric file into `(name, command)` criteria: each non-blank, non-`#`
/// line is `NAME :: COMMAND`, split on the FIRST ` :: ` (space-padded, matching
/// the documented form). So a name may contain `::` and a command a later ` :: `;
/// only the first separator binds. Names are base64-framed downstream, so any
/// name content is safe.
fn parse_rubric(text: &str) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name, command) = line.split_once(" :: ").ok_or_else(|| {
            PillboxError::usage(
                "session score",
                format!("rubric line {} is not `NAME :: COMMAND`: {line}", i + 1),
            )
        })?;
        let (name, command) = (name.trim(), command.trim());
        if name.is_empty() || command.is_empty() {
            return Err(PillboxError::usage(
                "session score",
                format!("rubric line {} has an empty name or command", i + 1),
            )
            .into());
        }
        out.push((name.to_string(), command.to_string()));
    }
    if out.is_empty() {
        return Err(PillboxError::usage("session score", "rubric file has no criteria").into());
    }
    Ok(out)
}

/// Single-quote `s` for `sh` (wrap in `'…'`, escaping embedded quotes as `'\''`).
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Compile criteria into ONE shell script. Per criterion: capture combined output
/// into `$__pb_o` and the REAL exit into `$__pb_rc` (`$?`), then emit a single
/// marker line with the exit + base64(name) + base64(output). The command's
/// output never reaches the script's stdout directly (it's captured, then
/// base64-re-emitted), so a malicious criterion can't forge a marker or flip its
/// own verdict — the exit is the shell's, not anything the agent can print. Exits
/// nonzero iff any criterion failed, so `passed = code == 0` still holds. Runs
/// through the unchanged executor, so `--in-sandbox`/`--grader-egress` are free.
fn compile_rubric(criteria: &[(String, String)]) -> String {
    use base64::Engine as _;
    let mut script = String::from("__pb_fail=0\n");
    for (name, command) in criteria {
        let name_b64 = base64::engine::general_purpose::STANDARD.encode(name);
        script.push_str(&format!(
            "__pb_o=$(sh -c {} 2>&1); __pb_rc=$?\n\
             printf '\\036PBCRIT\\t%s\\t%s\\t%s\\n' \"$__pb_rc\" '{name_b64}' \
             \"$(printf %s \"$__pb_o\" | base64 | tr -d '\\n')\"\n\
             [ \"$__pb_rc\" -eq 0 ] || __pb_fail=1\n",
            sh_quote(command),
        ));
    }
    script.push_str("exit \"$__pb_fail\"\n");
    script
}

/// Parse a compiled rubric's marker lines into per-criterion verdicts. Each
/// [`RUBRIC_MARKER`] line is `<exit>\t<name-b64>\t<output-b64>`; `passed` = exit 0,
/// feedback = base64-decoded output (tail-capped so N criteria can't bloat the §0
/// line). Non-marker lines are ignored — only our framing is trusted (the forge
/// boundary). A missing `base64` tool in the grader env degrades feedback to empty
/// but leaves the exit-derived verdict intact.
fn parse_rubric_output(raw: &str) -> Vec<Criterion> {
    use base64::Engine as _;
    const PER_CRIT_CAP: usize = 8 * 1024;
    let b64 = base64::engine::general_purpose::STANDARD;
    let decode = |s: &str| {
        b64.decode(s.trim())
            .ok()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
    };
    let mut crits = Vec::new();
    for line in raw.lines() {
        let Some(rest) = line.strip_prefix(RUBRIC_MARKER) else {
            continue;
        };
        let mut fields = rest.splitn(3, '\t');
        let (Some(exit), Some(name_b64)) = (fields.next(), fields.next()) else {
            continue;
        };
        let feedback = fields.next().and_then(decode).unwrap_or_default();
        crits.push(Criterion {
            // A name we emitted is always valid base64; fall back to the raw field
            // only if somehow not, so a criterion never silently vanishes.
            name: decode(name_b64).unwrap_or_else(|| name_b64.to_string()),
            passed: exit.trim() == "0",
            feedback: cap_tail(feedback.trim().to_string(), PER_CRIT_CAP),
        });
    }
    crits
}

/// The grader tool a sandboxed grade could not find, if any. Only the program
/// the grader command itself names counts: a bare `--cmd`/criterion program the
/// shell could not find (exit 127 and the shell's `PROG: not found`), or the
/// module of a leading `python3 -m MOD` that is not installed (runpy's
/// `…python3: No module named MOD`). In the grader microVM that means the runner
/// image lacks the tool, so every worker would "fail" the same way;
/// [`refuse_missing_tool`] turns it into an error instead. A program missing
/// further down (`sh check.sh` calling a `./bin/tool` the worker never built) is
/// the worker's defect and stays a failed grade.
fn missing_tool(spec: &GraderSpec, code: i32, raw: &str) -> Option<String> {
    match spec {
        GraderSpec::Cmd(cmd) => lead_tool_missing(cmd, code, raw),
        GraderSpec::Rubric { criteria, .. } => {
            use base64::Engine as _;
            let b64 = base64::engine::general_purpose::STANDARD;
            let markers = raw.lines().filter_map(|l| l.strip_prefix(RUBRIC_MARKER));
            markers.zip(criteria).find_map(|(line, (name, command))| {
                let mut f = line.splitn(3, '\t');
                let exit = f.next()?.trim().parse::<i32>().ok()?;
                let out = f
                    .nth(1)
                    .and_then(|o| b64.decode(o.trim()).ok())
                    .map(|b| String::from_utf8_lossy(&b).into_owned())
                    .unwrap_or_default();
                lead_tool_missing(command, exit, &out).map(|m| format!("criterion `{name}`: {m}"))
            })
        }
    }
}

/// [`missing_tool`] for one shell command, its exit code and its output. Setup
/// builtins at the front of a top-level list are skipped (`cd backend &&
/// pytest` names pytest), and only the first real command counts: a later one
/// may never have run, so a nested `git: not found` from an earlier step must
/// not be pinned on it. The split ignores quoting, which at worst misses a
/// tool, never invents one.
fn lead_tool_missing(command: &str, exit: i32, out: &str) -> Option<String> {
    for c in command.split([';', '|', '&']).map(str::trim) {
        let prog = c
            .split_whitespace()
            .find(|w| !is_env_assignment(w) && !matches!(*w, "env" | "exec"));
        match prog {
            None => continue,
            Some(p) if SETUP_BUILTINS.contains(&p) => continue,
            Some(_) => return segment_tool_missing(c, exit, out),
        }
    }
    None
}

/// Shell builtins a reward may run before its verifier; never the missing tool.
const SETUP_BUILTINS: &[&str] = &[
    "cd", "pushd", "export", "set", "unset", "umask", "source", ".", "true", ":",
];

/// [`lead_tool_missing`] for one simple command.
fn segment_tool_missing(command: &str, exit: i32, out: &str) -> Option<String> {
    let mut words = command
        .split_whitespace()
        .skip_while(|w| matches!(*w, "env" | "exec") || is_env_assignment(w));
    let prog = words.next()?;
    if exit == 127 && !prog.contains('/') {
        let not_found = [
            format!(": {prog}: not found"),
            format!(": {prog}: command not found"),
        ];
        if out
            .lines()
            .any(|l| not_found.iter().any(|n| l.trim_end().ends_with(n.as_str())))
        {
            return Some(format!("`{prog}` (exit 127)"));
        }
    }
    if !prog
        .rsplit('/')
        .next()
        .unwrap_or(prog)
        .starts_with("python")
    {
        return None;
    }
    let module = loop {
        match words.next()? {
            "-m" => break words.next()?,
            w if w.starts_with("-m") => break &w[2..],
            _ => {}
        }
    };
    // Only a module a runner image provides: `python3 -m myproject.check`
    // missing is the worker deleting its own package, a failed grade.
    let top = module.split('.').next().unwrap_or(module);
    if !RUNNER_TOOL_MODULES.contains(&top) {
        return None;
    }
    let expected = format!(": No module named {module}");
    out.lines()
        .map(str::trim)
        .any(|l| {
            l.ends_with(&expected) && {
                let exe = &l[..l.len() - expected.len()];
                exe.rsplit('/').next().unwrap_or(exe).starts_with("python")
            }
        })
        .then(|| format!("python module `{module}`"))
}

/// Python test and lint tools a grader runs with `python3 -m` that come from the
/// runner image, never from the worker's tree.
const RUNNER_TOOL_MODULES: &[&str] = &[
    "pytest",
    "coverage",
    "hypothesis",
    "nose2",
    "tox",
    "nox",
    "mypy",
    "ruff",
    "black",
    "flake8",
    "pylint",
    "pyright",
    "behave",
    "pip",
];

fn is_env_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(k, _)| {
        !k.is_empty()
            && !k.starts_with(|c: char| c.is_ascii_digit())
            && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// Error out of a sandboxed grade whose tool is missing from the runner image
/// (see [`missing_tool`]): that is the grader's environment failing, not the
/// worker, so it must read as `errored`, never as a failed grade.
pub(super) fn refuse_missing_tool(spec: &GraderSpec, code: i32, raw: &str) -> Result<()> {
    match missing_tool(spec, code, raw) {
        None => Ok(()),
        Some(what) => Err(PillboxError::runtime(
            "session score",
            format!(
                "the grader microVM could not find {what}; the runner image lacks a tool \
                 this grader needs, so nothing was graded"
            ),
        )
        .with_next(
            "install the tool in a runner image built FROM the pillbox runner and set \
             PILLBOX_RUNNER_IMAGE (see docs/dispatch.md, Grading)",
        )
        .into()),
    }
}

/// A short `feedback`-field summary of a rubric grade — one `✓/✗ name` line per
/// criterion. The structured detail lives in `criteria[]`.
fn render_rubric_summary(criteria: &[Criterion]) -> String {
    criteria
        .iter()
        .map(|c| format!("{} {}", if c.passed { "✓" } else { "✗" }, c.name))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combine_streams_joins_nonempty() {
        assert_eq!(combine_streams(b"out", b"err"), "out\nerr");
        assert_eq!(combine_streams(b"only-out", b""), "only-out");
        assert_eq!(combine_streams(b"", b"only-err"), "only-err");
    }

    #[test]
    fn cap_tail_keeps_the_tail() {
        let big = "x".repeat(40 * 1024) + "TAIL-VERDICT";
        let f = cap_tail(big, 32 * 1024);
        assert!(f.starts_with("…[truncated"), "{}", &f[..40]);
        assert!(f.ends_with("TAIL-VERDICT"), "tail kept");
        assert!(f.len() < 34 * 1024, "capped near 32K, got {}", f.len());
    }

    #[test]
    fn parse_rubric_reads_named_criteria_and_skips_noise() {
        let r =
            parse_rubric("# header\n\nAll tests pass :: pytest -q\nhas fn :: grep -q def f.py\n")
                .unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(
            r[0],
            ("All tests pass".to_string(), "pytest -q".to_string())
        );
        assert_eq!(r[1].1, "grep -q def f.py");
    }

    #[test]
    fn python_bootstrap_stops_workspace_shadowing_but_keeps_project_imports() {
        let py = |args: &[&str], dir: &Path, env: bool, boot: &Path| {
            let mut c = std::process::Command::new("python3");
            c.args(args).current_dir(dir);
            if env {
                c.env("PYTHONSAFEPATH", "1").env("PYTHONPATH", boot);
            }
            c.output().map(|o| o.status.code().unwrap_or(-1))
        };
        let ver = std::process::Command::new("python3")
            .args(["-c", "import sys; print(sys.version_info >= (3, 11))"])
            .output();
        if !matches!(&ver, Ok(o) if String::from_utf8_lossy(&o.stdout).trim() == "True") {
            eprintln!("skip: needs python3 >= 3.11 (PYTHONSAFEPATH)");
            return;
        }
        let boot = tempfile::tempdir().unwrap();
        std::fs::write(boot.path().join("sitecustomize.py"), PY_BOOTSTRAP).unwrap();
        let ws = tempfile::tempdir().unwrap();
        // A worker shadow of a stdlib module the "runner" imports.
        std::fs::write(ws.path().join("json.py"), "raise SystemExit(0)\n").unwrap();
        std::fs::create_dir_all(ws.path().join("mypkg")).unwrap();
        std::fs::write(ws.path().join("mypkg/__init__.py"), "X = 1\n").unwrap();
        std::fs::create_dir_all(ws.path().join("tools")).unwrap();
        std::fs::write(ws.path().join("tools/helper.py"), "Y = 2\n").unwrap();
        std::fs::write(ws.path().join("tools/run.py"), "import helper\n").unwrap();
        let shadow = ["-m", "json.tool", "/nonexistent-input"];
        assert_eq!(
            py(&shadow, ws.path(), false, boot.path()).unwrap(),
            0,
            "the shadow works without the bootstrap"
        );
        assert_ne!(
            py(&shadow, ws.path(), true, boot.path()).unwrap(),
            0,
            "the real json.tool ran"
        );
        assert_eq!(
            py(&["-c", "import mypkg"], ws.path(), true, boot.path()).unwrap(),
            0
        );
        assert_eq!(
            py(&["tools/run.py"], ws.path(), true, boot.path()).unwrap(),
            0,
            "script dir still importable"
        );
        // A console script (`pytest`) lives outside the tree: the project
        // still imports from the cwd.
        let bin = tempfile::tempdir().unwrap();
        let script = bin.path().join("console-script");
        std::fs::write(&script, "import mypkg\n").unwrap();
        assert_eq!(
            py(&[script.to_str().unwrap()], ws.path(), true, boot.path()).unwrap(),
            0,
            "cwd importable under a console script"
        );
        let setup = dropped_grader_setup();
        assert!(!setup.contains(';'), "{setup}");
        assert!(setup.contains("PYTHONSAFEPATH=1"), "{setup}");
    }

    #[test]
    fn grader_started_printf_emits_the_marker_on_its_own_line() {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(GRADER_STARTED_PRINTF)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            format!("\n{GRADER_STARTED_MARKER}\n")
        );
    }

    #[test]
    fn grader_output_after_start_drops_boot_noise() {
        let raw = format!("kernel: booted\n\n{GRADER_STARTED_MARKER}\r\n3 passed\n");
        assert_eq!(grader_output_after_start(0, &raw).unwrap(), "3 passed\n");
    }

    #[test]
    fn missing_start_marker_is_an_error_not_a_failed_grade() {
        let err = grader_output_after_start(1, "mount: virtiofs: no such device\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("before the grader started"), "{err}");
        assert!(err.contains("no such device"), "{err}");
    }

    fn run_rubric(criteria: &[(&str, &str)]) -> (GraderSpec, i32, String) {
        let criteria: Vec<(String, String)> = criteria
            .iter()
            .map(|(n, c)| (n.to_string(), c.to_string()))
            .collect();
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(compile_rubric(&criteria))
            .output()
            .unwrap();
        let spec = GraderSpec::Rubric {
            criteria,
            label: "rubric:t".into(),
        };
        (
            spec,
            out.status.code().unwrap(),
            combine_streams(&out.stdout, &out.stderr),
        )
    }

    #[test]
    fn missing_grader_tool_is_refused_not_graded() {
        let cmd = GraderSpec::Cmd("cargo test".into());
        assert!(missing_tool(&cmd, 127, "sh: cargo: not found").is_some());
        assert!(missing_tool(&cmd, 1, "1 test failed").is_none());
        assert!(missing_tool(&cmd, 0, "").is_none());
        let env = GraderSpec::Cmd("RUST_LOG=1 cargo test".into());
        assert!(missing_tool(&env, 127, "bash: line 1: cargo: command not found").is_some());
        let pytest = GraderSpec::Cmd("python3 -m pytest -q".into());
        let py = missing_tool(&pytest, 1, "/usr/bin/python3: No module named pytest\n").unwrap();
        assert!(py.contains("pytest"), "{py}");
        // A test's own ImportError is a failed grade, not a missing tool.
        assert!(
            missing_tool(&pytest, 1, "E   ModuleNotFoundError: No module named 'foo'").is_none()
        );
        assert!(refuse_missing_tool(&cmd, 127, "sh: 1: cargo: not found").is_err());
        assert!(refuse_missing_tool(&cmd, 1, "assert 1 == 2").is_ok());

        // A program missing inside the grader (the worker never built it) is
        // the worker's failure, not the runner image's.
        let nested = GraderSpec::Cmd("sh .pillbox-grader/check.sh".into());
        let out = ".pillbox-grader/check.sh: 1: ./bin/tool: not found";
        assert!(missing_tool(&nested, 127, out).is_none());
        let own = GraderSpec::Cmd("python3 -m myproject.check".into());
        let out = "/usr/bin/python3: No module named myproject";
        assert!(
            missing_tool(&own, 1, out).is_none(),
            "the worker's own package"
        );
        let listed = GraderSpec::Cmd("cd backend && python3 -m pytest -q".into());
        let out = "/usr/bin/python3: No module named pytest";
        assert!(
            missing_tool(&listed, 1, out).is_some(),
            "a tool after a setup step"
        );
        let listed = GraderSpec::Cmd("cd crate && cargo test".into());
        assert!(missing_tool(&listed, 127, "sh: 1: cargo: not found").is_some());
        let unrun = GraderSpec::Cmd("sh -c 'PATH=/nope git' && git status".into());
        assert!(
            missing_tool(&unrun, 127, "sh: 1: git: not found").is_none(),
            "a later command may never have run"
        );
        let worker_module = GraderSpec::Cmd("sh run.sh".into());
        assert!(missing_tool(&worker_module, 1, "python3: No module named app").is_none());

        let (spec, code, raw) =
            run_rubric(&[("fmt", "true"), ("tests", "pb-no-such-tool-xyz --check")]);
        let what = missing_tool(&spec, code, &raw).unwrap();
        assert!(what.contains("tests"), "{what}");
        let (spec, code, raw) = run_rubric(&[("tests", "exit 1")]);
        assert!(missing_tool(&spec, code, &raw).is_none());
        let (spec, code, raw) = run_rubric(&[("built", "sh -c ./pb-not-built-xyz")]);
        assert!(missing_tool(&spec, code, &raw).is_none(), "{raw}");
    }

    #[test]
    fn parse_rubric_rejects_malformed_and_empty() {
        assert!(parse_rubric("no separator here").is_err());
        assert!(parse_rubric(" :: command with no name").is_err());
        assert!(parse_rubric("name with no command :: ").is_err());
        assert!(parse_rubric("# only comments\n\n").is_err());
    }

    #[test]
    fn parse_rubric_allows_colons_in_name_and_command() {
        // Split on the FIRST ` :: ` only: a name may contain `::`, a command a
        // later ` :: `. (Names are base64-framed, so any content is safe.)
        let r = parse_rubric("Mod::check :: python -c 'a :: b'").unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, "Mod::check");
        assert_eq!(r[0].1, "python -c 'a :: b'");
    }

    #[test]
    fn compile_then_parse_rubric_roundtrips_verdicts() {
        // Compile a 2-criterion rubric (one passes, one fails), run it through a
        // real shell, and confirm the marker output parses back to per-criterion
        // verdicts — the end-to-end contract without a session/VM.
        let script = compile_rubric(&[
            ("says hi".into(), "echo hi".into()),
            ("fails".into(), "echo boom; exit 3".into()),
        ]);
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&script)
            // Pin the subprocess cwd: other tests mutate the process-global cwd
            // (set_current_dir into a tempdir that's later removed). An unpinned
            // `sh` can fork while cwd points at such a dir mid-removal, so its
            // getcwd fails ("cannot access parent directories"). CARGO_MANIFEST_DIR
            // is the crate root — always present — so this test is race-immune.
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .unwrap();
        // The script exits nonzero iff any criterion failed.
        assert_ne!(out.status.code(), Some(0));
        let crits = parse_rubric_output(&combine_streams(&out.stdout, &out.stderr));
        assert_eq!(crits.len(), 2);
        assert_eq!(crits[0].name, "says hi");
        assert!(crits[0].passed);
        assert_eq!(crits[0].feedback, "hi");
        assert_eq!(crits[1].name, "fails");
        assert!(!crits[1].passed);
        assert!(crits[1].feedback.contains("boom"));
    }

    #[test]
    fn rubric_verdict_cannot_be_forged_by_criterion_output() {
        // The reward-channel integrity boundary: a criterion that PRINTS a fake
        // passing marker but actually exits nonzero must still score as failed —
        // the graded agent controls output, not the verdict.
        let forge = "printf '\\036PBCRIT\\t0\\tZm9yZ2Vk\\tZm9yZ2Vk\\n'; exit 1";
        let script = compile_rubric(&[("real check".into(), forge.into())]);
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&script)
            .current_dir(env!("CARGO_MANIFEST_DIR")) // stable cwd; see compile_then_parse_rubric_roundtrips_verdicts
            .output()
            .unwrap();
        let crits = parse_rubric_output(&combine_streams(&out.stdout, &out.stderr));
        // Exactly one criterion (the forged marker is inert — base64'd as output),
        // and it failed (the real exit was 1).
        assert_eq!(crits.len(), 1, "forged marker injected a phantom criterion");
        assert_eq!(crits[0].name, "real check");
        assert!(!crits[0].passed, "forged marker flipped the verdict");
    }

    #[test]
    fn sh_quote_escapes_embedded_single_quote() {
        assert_eq!(sh_quote("plain"), "'plain'");
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn grade_result_cmd_is_binary_rubric_is_fractional() {
        assert_eq!(
            grade_result(&GraderSpec::Cmd("x".into()), 0, "ok".into()).score,
            1.0
        );
        assert_eq!(
            grade_result(&GraderSpec::Cmd("x".into()), 1, "no".into()).score,
            0.0
        );
    }
}
