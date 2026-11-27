//! `pillbox run` as an in-process call. [`start`] resolves the agent and the
//! descriptor defaults, picks the backend and launches one run, returning what
//! started as a typed [`Started`]; it prints nothing to stdout. The CLI is one
//! client of it ([`render_started`] prints the envelope or banner); dispatch and
//! eval are others.

use anyhow::{bail, Result};

use crate::agents::{self, RunOpts};
use crate::errors::PillboxError;
use crate::pillbox::Pillbox;
use crate::sandbox::{Started, SubstrateKind};

/// The agent to run: an explicit `--agent`, else the descriptor cascade
/// (project `pillbox.toml` → `~/.pillbox/global` → built-in `claude`).
pub(crate) fn resolve_agent_spec(
    resolved: &Pillbox,
    override_id: Option<&str>,
) -> Result<&'static agents::AgentSpec> {
    let id = match override_id {
        Some(id) => id.to_string(),
        // Read fresh so an edited `agent =` takes effect without a meta rewrite.
        None => crate::config::resolve_run_config(resolved)
            .agent
            .unwrap_or_else(|| "claude".into()),
    };
    agents::lookup("run", &id)
}

/// Bookmark forks run only on the local libkrun microVM backend.
pub(crate) fn validate_bookmark_backend(opts: &RunOpts, kind: SubstrateKind) -> Result<()> {
    if opts.from_bookmark.is_some() && kind != SubstrateKind::Libkrun {
        return Err(PillboxError::usage(
            "run",
            "bookmark forks require the local libkrun microVM backend",
        )
        .with_next("use a libkrun-enabled Pillbox build with PILLBOX_BACKEND=libkrun")
        .into());
    }
    Ok(())
}

/// The usage errors a run request raises before launch, in the order the CLI
/// reports them. `json` is the CLI's `--json`; in-process callers pass `false`.
pub(crate) fn validate_run_request(
    spec: &agents::AgentSpec,
    opts: &RunOpts,
    json: bool,
) -> Result<()> {
    let structured = spec.integration.supports_execution_request();
    // A profile or reasoning effort needs a structured transport; checked before `--json`.
    if !structured && (opts.profile.is_some() || opts.reasoning_effort.is_some()) {
        return Err(PillboxError::usage(
            "run",
            format!(
                "agent `{}` has no structured profile transport; use a structured agent integration",
                spec.id
            ),
        )
        .into());
    }
    // `--json` needs a persisted session to report; a foreground PTY run has none,
    // so it is refused before launch rather than printing nothing after it. (Not a
    // clap `requires`: server-mode validity depends on the resolved agent.)
    if json && !opts.detach && !structured {
        return Err(PillboxError::usage(
            "run",
            "--json needs a persisted session: add --detach, or run a structured agent",
        )
        .with_next(format!("pillbox run --agent {} --detach --json", spec.id))
        .into());
    }
    Ok(())
}

/// Default the workspace to the project directory when none was given.
pub(crate) fn apply_default_workspace(resolved: &Pillbox, opts: &mut RunOpts) {
    // A bookmark restores into a host path only when a workspace override was supplied.
    // Preserve None through dispatch so the backend can choose its immutable base cache.
    if opts.workspace.is_none() && opts.from_bookmark.is_none() {
        if let crate::pillbox::Scope::Project { source_dir, .. } = &resolved.scope {
            opts.workspace = Some(source_dir.clone());
        }
    }
}

/// Launch one run of `agent` (or the descriptor's default agent) in `resolved`
/// and return what started. Applies the descriptor defaults (name, model,
/// workspace), validates the request against the agent and the selected
/// backend, and runs the `--memory` briefing/capture around the backend call.
/// Prints nothing to stdout; stderr carries only the warnings and notes the
/// run itself raises.
///
/// A structured run whose agent exits non-zero still returns
/// [`Started::StructuredFinished`] with that `exit_code`: the session and its
/// log are real, and the caller decides whether that is an error.
pub(crate) fn start(resolved: &Pillbox, agent: Option<&str>, mut opts: RunOpts) -> Result<Started> {
    let spec = resolve_agent_spec(resolved, agent)?;
    if let Some(meta) = &resolved.meta {
        if opts.name.is_none() {
            opts.name = Some(meta.name.clone());
        }
    }

    validate_run_request(spec, &opts, false)?;

    // Model cascade: `--model` wins, else the descriptor (project `pillbox.toml`
    // → `~/.pillbox/global/pillbox.toml`), else the agent's own default (`None`).
    if opts.model.is_none() {
        opts.model = crate::config::resolve_run_config(resolved).model;
    }

    apply_default_workspace(resolved, &mut opts);

    let kind = SubstrateKind::selected()?;
    validate_bookmark_backend(&opts, kind)?;
    let backend = kind.backend()?;
    if !opts.memory {
        return backend.run(spec, opts, resolved);
    }
    // --memory: brief the agent from kypp at start, capture THIS run's §0 log after. Host-side +
    // best-effort. Detached runs skip the post-capture — the agent outlives this call, so the §0 log
    // isn't complete yet; a scheduled `kypp sweep` (cron) catches them. Project scope = pillbox name.
    let project = opts.name.clone().unwrap_or_else(|| "default".to_string());
    let is_server = spec.integration == crate::agents::Integration::Server;
    let capture_after = !opts.detach;
    let briefed = crate::memory::brief_into_args(&mut opts.args, &project);
    opts.memory_briefed = briefed.clone(); // carried into the backend so a server bring-up can stash it
    let started = std::time::SystemTime::now(); // run-window start: capture only logs written after this
    let result = backend.run(spec, opts, resolved);
    // A FOREGROUND (PTY) agent finalizes its §0 log before `run` returns → capture now. A SERVER agent
    // (opencode) is reparented: the log drains LATER, so capturing here would race an empty log —
    // `session ingest` does the capture instead, from the brief the bring-up stashed (crate::memory).
    if capture_after && !is_server {
        let sessions = crate::session::sessions_root_path(resolved);
        crate::memory::capture_run(&sessions, &project, started);
        // record which briefed claims this run saw, for later credit assignment
        crate::memory::record_brief_usage(&sessions, &project, started, &briefed);
    }
    result
}

/// The stdout lines `pillbox run` prints for `started`: the `--json` start
/// envelope, or the human banner for that lifecycle. Pure, so the shapes are
/// pinned by tests.
fn started_lines(started: &Started, json: bool) -> Vec<String> {
    let Some(session) = started.session() else {
        return Vec::new();
    };
    if json {
        return vec![crate::session::started_json(session)];
    }
    match started {
        Started::Foreground => Vec::new(),
        Started::Detached(session) => vec![
            format!(
                "pillbox: ✓ session `{}` started in background ({})",
                session.id,
                session.substrate.label()
            ),
            format!("  Next: pillbox session attach {}", session.id),
        ],
        Started::ServerReady {
            session,
            pending_prompt,
        } => {
            let model = session
                .server
                .as_ref()
                .map(|s| s.model.as_str())
                .unwrap_or("?");
            vec![
                format!(
                    "pillbox: ✓ {} session `{}` ready ({model}).",
                    session.agent_id, session.id
                ),
                format!(
                    "         pillbox session watch {}    # read the stream",
                    session.id
                ),
                match pending_prompt {
                    Some(p) => format!(
                        "         pillbox session send {} {p:?}  # send your prompt",
                        session.id
                    ),
                    None => format!(
                        "         pillbox session send {} \"…\"  # drive it",
                        session.id
                    ),
                },
            ]
        }
        Started::StructuredFinished {
            session, events, ..
        } => vec![format!(
            "pillbox: ✓ {} session `{}` finished ({events} durable events)",
            session.agent_id, session.id
        )],
        Started::ManagedFinished(session) => vec![
            format!(
                "pillbox: ✓ managed session `{}` finished; result snapshot recorded.",
                session.id
            ),
            format!(
                "         pillbox session pull {}   # rehydrate the result",
                session.id
            ),
        ],
    }
}

/// Print what `pillbox run` started.
// The CLI renders the started session; backends return it and never print.
pub(crate) fn render_started(started: &Started, json: bool) -> Result<()> {
    for line in started_lines(started, json) {
        println!("{line}");
    }
    // A structured run that exited non-zero is reported, then fails the command.
    if let Started::StructuredFinished {
        session, exit_code, ..
    } = started
    {
        if *exit_code != 0 {
            bail!("{} structured run exited {}", session.agent_id, exit_code);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{ServerSession, Session};

    /// The `run --json` envelope, byte for byte as the backends printed it
    /// before they returned [`Started`] instead.
    const FIXTURE_ENVELOPE: &str = r#"{"session":{"agent_id":"claude","attached_pid":null,"backend":"libkrun","base_snapshot":null,"expires_at":null,"id":"abc123def456","label":"test","placement":"local","pty_pid":0,"result_snapshot":null,"sandbox_id":"","started_at":"2026-05-23T13:37:00Z"},"version":1}"#;

    fn usage_reason(err: anyhow::Error) -> String {
        err.downcast::<PillboxError>().unwrap().to_string()
    }

    const NO_PROFILE_TRANSPORT: &str =
        "agent `claude` has no structured profile transport; use a structured agent integration";
    const JSON_NEEDS_SESSION: &str =
        "--json needs a persisted session: add --detach, or run a structured agent";

    #[test]
    fn run_request_reports_the_profile_error_before_the_json_error() {
        let claude = agents::lookup("run", "claude").unwrap();
        // `run --agent claude --profile fast --json` (no --detach): both checks fail;
        // the profile one is reported, as on base.
        let opts = RunOpts {
            profile: Some("fast".into()),
            ..RunOpts::default()
        };
        let err = usage_reason(validate_run_request(claude, &opts, true).unwrap_err());
        assert!(err.contains(NO_PROFILE_TRANSPORT), "{err}");
        // In-process callers (json=false) still get the profile check.
        let err = usage_reason(validate_run_request(claude, &opts, false).unwrap_err());
        assert!(err.contains(NO_PROFILE_TRANSPORT), "{err}");
    }

    #[test]
    fn run_request_refuses_json_without_a_persisted_session() {
        let claude = agents::lookup("run", "claude").unwrap();
        let foreground = RunOpts::default();
        let err = usage_reason(validate_run_request(claude, &foreground, true).unwrap_err());
        assert!(err.contains(JSON_NEEDS_SESSION), "{err}");
        assert!(validate_run_request(claude, &foreground, false).is_ok());
        let detached = RunOpts {
            detach: true,
            ..RunOpts::default()
        };
        assert!(validate_run_request(claude, &detached, true).is_ok());
        // A structured agent persists a session even in the foreground.
        let opencode = agents::lookup("run", "opencode").unwrap();
        assert!(validate_run_request(opencode, &foreground, true).is_ok());
    }

    fn libkrun_fixture() -> Session {
        Session::test_fixture()
    }

    #[test]
    fn json_envelope_is_the_same_for_every_lifecycle() {
        let session = Session::test_fixture();
        for started in [
            Started::Detached(session.clone()),
            Started::ServerReady {
                session: session.clone(),
                pending_prompt: Some("hi".into()),
            },
            Started::StructuredFinished {
                session: session.clone(),
                events: 3,
                exit_code: 0,
            },
            Started::ManagedFinished(session.clone()),
        ] {
            assert_eq!(started_lines(&started, true), vec![FIXTURE_ENVELOPE]);
        }
        assert!(started_lines(&Started::Foreground, true).is_empty());
    }

    #[test]
    fn human_banners_match_the_backend_banners() {
        // One detached banner for every backend, naming the session's backend.
        assert_eq!(
            started_lines(&Started::Detached(libkrun_fixture()), false),
            vec![
                "pillbox: ✓ session `abc123def456` started in background (libkrun)",
                "  Next: pillbox session attach abc123def456",
            ]
        );
        let mut server = libkrun_fixture();
        server.agent_id = "opencode".into();
        server.server = Some(ServerSession {
            agent_session_id: "ses_1".into(),
            model: "m/x".into(),
            temperature: None,
        });
        assert_eq!(
            started_lines(
                &Started::ServerReady {
                    session: server.clone(),
                    pending_prompt: Some("fix \"it\"".into()),
                },
                false
            ),
            vec![
                "pillbox: ✓ opencode session `abc123def456` ready (m/x).",
                "         pillbox session watch abc123def456    # read the stream",
                "         pillbox session send abc123def456 \"fix \\\"it\\\"\"  # send your prompt",
            ]
        );
        assert_eq!(
            started_lines(
                &Started::ServerReady {
                    session: server,
                    pending_prompt: None,
                },
                false
            )[2],
            "         pillbox session send abc123def456 \"…\"  # drive it"
        );
        assert_eq!(
            started_lines(
                &Started::StructuredFinished {
                    session: libkrun_fixture(),
                    events: 7,
                    exit_code: 0,
                },
                false
            ),
            vec!["pillbox: ✓ claude session `abc123def456` finished (7 durable events)"]
        );
        assert_eq!(
            started_lines(&Started::ManagedFinished(libkrun_fixture()), false),
            vec![
                "pillbox: ✓ managed session `abc123def456` finished; result snapshot recorded.",
                "         pillbox session pull abc123def456   # rehydrate the result",
            ]
        );
    }

    #[test]
    fn structured_non_zero_exit_renders_then_fails_with_the_old_message() {
        let started = Started::StructuredFinished {
            session: libkrun_fixture(),
            events: 2,
            exit_code: 3,
        };
        let error = render_started(&started, true).expect_err("non-zero exit fails the command");
        assert_eq!(error.to_string(), "claude structured run exited 3");
        let ok = Started::StructuredFinished {
            session: libkrun_fixture(),
            events: 2,
            exit_code: 0,
        };
        assert!(render_started(&ok, true).is_ok());
    }
}
