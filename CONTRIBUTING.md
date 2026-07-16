# Contributing to pillbox

Pillbox is pre-alpha. Small, falsifiable changes are easier to evaluate than
broad platform additions: define the boundary first, add an executable check,
then implement the smallest end-to-end slice.

## Direction

- Cloudflare Durable Objects are the managed session authority; Cloudflare
  Containers run the agent.
- libkrun is the one local agent backend. The Docker agent backend is deprecated
  and gets no new features or parity work.
- Local and managed placements share the §0 event contract. A feature that is
  only observable through a private managed service is incomplete.
- Ghost remains an in-repo experimental tenant until its conductor contract is
  stable. Do not extract it or claim design-only planner features as shipped.

Read [`AGENTS.md`](./AGENTS.md) and the relevant canonical document before
changing a subsystem. Ratified decisions in [`.brief/docs/`](./.brief/docs/) are
constraints, not editable notes.

## Development checks

The platform-independent Rust path:

```sh
cargo fmt --check
cargo test --no-default-features --all-targets
cargo clippy --no-default-features --all-targets -- -D warnings
```

The Cloudflare gateway path (Node.js 22.6+):

```sh
cd cloudflare-spike
npm ci
npm test
npx tsc --noEmit
python3 check-contract-parity.py
../scripts/smoke/cf.sh
```

The libkrun feature path needs the local libkrun toolchain. See
[`docs/libkrun-sandbox.md`](./docs/libkrun-sandbox.md) and CI for the current
macOS installation recipe.

Before committing a change governed by Context Vault:

```sh
brief check
brief pin
```

Record the required conformance or amendment line in `.brief/SIGNOFF`. Do not
bypass the hook or edit an active decision to fit the implementation.

## Pull requests

- Keep one logical change per PR.
- State the contract exercised and paste the verification commands/results.
- Separate what is live-proven, locally tested, implemented-but-unproven, and
  still design-only.
- Update the canonical doc when code and docs disagree.
- Never include secrets, `.env.local`, Cloudflare actor secrets, provider keys,
  or generated run traces.

Security reports belong in the private channel described in
[`SECURITY.md`](./SECURITY.md), not a public issue.
