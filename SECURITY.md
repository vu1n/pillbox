# Security policy

Pillbox is pre-alpha software. We take credential-handling and
sandbox-isolation bugs seriously and will fix them with the same
priority as functional regressions.

## Threat model

See [docs/security.md](./docs/security.md) for the full posture
— what pillbox defends against, what it does not, where data lives,
and the security boundaries of local libkrun and experimental
Cloudflare-managed sessions.

## Reporting a vulnerability

If you've found a credential leak, sandbox escape, perms bug, or
similar — please disclose it privately, not in a public issue.

Open a private vulnerability report at
https://github.com/vu1n/pillbox/security/advisories/new — GitHub
takes it from there. If you need a fallback channel (your account
can't open advisories, you're disclosing on behalf of someone
without one), ping the maintainers via a public GitHub issue
asking for a private channel and we'll get you one. Do **not**
include the vulnerability detail in the public issue.

What helps us triage fast:

- A self-contained repro (a tiny script, a corrupt input, a flag
  combination, …).
- The pillbox version (`pillbox version`) and host OS.
- Whether the issue requires local libkrun, the experimental managed
  placement, or both.
- If you tried it: what you observed vs. what you expected.

We aim to acknowledge within 72 hours and ship a fix within 14 days
for confirmed issues. We'll keep you in the loop on the fix and
credit you in the changelog unless you'd rather we didn't.

## In-scope

- Anything under this repository (`src/`, `tests/`, helper scripts,
  packaging metadata).
- Anything pillbox writes to disk: layout, permissions, encryption
  posture, blob protocol shape.

## Out of scope

- Upstream issues in dependencies — please report directly to those
  projects. Pillbox will pull in fixed releases when they ship.
- The agent CLIs themselves (`claude`, `codex`, …). Pillbox sandboxes
  them but doesn't own their code.
- Social-engineering, phishing, or supply-chain attacks against
  Anthropic / OpenAI / GitHub itself.
- Issues that require physical access to an unlocked machine — those
  are the host's disk-encryption story, not pillbox's.

## Past disclosures

None yet. We'll list confirmed reports here once any land.
