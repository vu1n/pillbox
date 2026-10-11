# `grok_bot` as a `pillbox.text/2` harness: assessment and blockers

STATUS: blocked, no driver. Researched 2026-10-11 against the SpaceXAI (xAI)
docs listed under [Sources](#sources). This is an assessment, not a canonical
subsystem doc; the text contract itself is in [commands.md](./commands.md)
("Sealed local text execution") and
[huddles-codex-execution.md](./huddles-codex-execution.md).

## Finding

"Grok Bot" is a hosted agent product, not a harness Pillbox can run. A Grok
Bot is a named, persistent AI teammate that does its work on a cloud computer
in Cursor's cloud (browser, filesystem, terminal, connectors, computer use).
People drive it by messaging it from the desktop or mobile apps. There is no
CLI, SDK or API that sends one prompt and returns one final text. The xAI API
reference has no Bot endpoints. The only documented programmatic entry point
is a routine's webhook trigger, which starts a run and returns `200` without
the result or a callback.

So `grok_bot` cannot satisfy the `pillbox.text/2` driver contract, and this
change adds no driver, agent registration, runner-image bundle or event
parser for it. Registering it as a Pillbox agent would mean faking it. A
request that names `grok_bot` is still refused by `TextRequestV2::validate`
as an unknown harness, the right answer for a harness Pillbox does not have.

Anything that could run headlessly here would be a Grok model behind some
other harness, not a Grok Bot: either Grok Build (`grok -p`, xAI's official
CLI) or a direct client for the xAI Responses API. This repo registers
neither. Calling either of them `grok_bot` would misreport what ran in
`resolved.harness`.

## What Grok Bot is (primary sources)

| Question | Answer | Source |
|---|---|---|
| Real CLI or API | None for driving a Bot. Desktop and mobile apps are "thin clients for chat, review, and approvals". The xAI REST API covers inference, collections and management only. | [overview], [teams], [api-ref] |
| Headless one-prompt mode | None. The closest is a routine webhook: `POST` with a bearer key and an optional JSON body. "A response of 200 means Grok Bot accepted the call and started a run. It does not mean the Bot has finished the instruction. Check the Bot's chat for the result." | [webhook], [routines] |
| Where it runs | On a persistent cloud computer in Cursor's cloud, shared by every Bot on the account. Not installable in a runner image or a microVM. | [overview], [computer] |
| Auth | A paid Cursor plan or a linked SuperGrok subscription, signed in through the app. A webhook key is per routine. No API-key path. | [overview], [webhook] |
| Output format | Chat messages, voice memos and drafts in the app. No machine-readable stream. | [overview] |
| Usage or served model | A turn does not return token counts or a cost. There is no customer-facing model picker. Account usage analytics show the model that served each request, and billing follows that model. That is a dashboard after the fact, not a per-turn payload. | [overview], [security] |
| Version | Not exposed. The product updates itself in the cloud. | [overview], [computer] |
| Can all tools be denied | No. Each Bot has a browser, terminal, files and connectors. Auto Review and approvals gate risky actions but do not remove the tools. Local-computer settings "do not prevent the Bot from using its cloud computer." A routine test "does real work. It can change files and use connected plugins." Network allowlists are Enterprise-only and default to allow-all. | [computer], [approvals], [webhook], [security] |
| State between turns | A Bot keeps memory, files, browser sessions and preferences across sessions by design. | [overview], [bots] |

## Blockers

Each one alone rules out a faithful driver. Requirement numbers refer to the
driver requirements in the task (resolve, tool-free, one bounded final text,
`resolved`, `usage`, fail closed).

1. **No placement inside the microVM.** `pillbox.text/2` accepts only
   `placement: "local_microvm"`. A Grok Bot runs on a Cursor-hosted cloud
   computer, so no part of the turn executes in the owned libkrun VM. There is
   nothing to put in the runner image and no `harness_version` to observe from
   it (requirements 1 and 4).
2. **Not tool-free, and it cannot be proven.** `tool_policy` must be
   `deny_all`, proven by a test. A Grok Bot's browser, terminal, filesystem,
   connectors and computer use are part of the product. Approvals and Auto
   Review decide whether a given action runs; they do not remove tools, and
   neither is enforceable by Pillbox. The vault cannot fence the egress of a
   machine Pillbox does not own (requirement 2). The task says to report this
   rather than weaken `deny_all`.
3. **No one-prompt, one-final-text call.** A webhook `POST` is accepted with
   HTTP 200 when a run starts. The routines page does not document a response
   body that carries the answer, a completion signal, or a callback. The
   result is in the Bot's chat (requirement 3).
4. **Not a sealed, stateless turn.** Huddles owns the conversation and sends
   one sealed rendered input. A Bot folds every turn into its own memory and
   shares a computer, logins and files with the account's other Bots. The
   output depends on hidden state Pillbox cannot pin or record, and the input
   leaks into a store Pillbox does not control.
5. **No credential path through the vault.** Access is a signed-in Cursor or
   SuperGrok account plus per-routine webhook keys. There is no
   `pillbox:*:default` credential the vault can release for one invocation
   with a bounded egress host list (requirements 1 and 6).
6. **No per-turn usage, served model, or version.** A turn returns no token
   counts and no cost, so `usage` would be absent, which is how text/2 treats
   a harness that reports nothing. `resolved.served_model` is a different
   shape: the record always includes it, and it is null unless the turn names
   the model. Account usage analytics show the serving model later, including
   failovers, and there is no customer-facing model picker, so a requested
   `agent.model` cannot be checked or reported at `resolve`. No
   `harness_version` is exposed to observe (requirements 1, 4 and 5).
7. **Idempotency.** text/2 promises at-most-once dispatch per invocation ID.
   A webhook `POST` carries no idempotency key, and a retried `POST` can start
   a second run.

## Design if xAI ships a Bot API

A real driver needs, at minimum, an official endpoint that runs one prompt
against a Bot and returns its final answer, plus a way to run that turn with
no tools and no memory writes. With that, the driver would be:

- **Registration.** No runner-image bundle, because nothing runs in the guest.
  Placement would have to be a new value. `pillbox.text/2` accepts only
  `local_microvm` today, and the managed tier is a separate single-controller
  runtime (`docs/managed-tier.md`), so this is a contract change, not a driver
  added beside Codex.
- **Resolve.** Map the requested model to the Bot's served model through an
  xAI models call, reject unservable models with `runtime_rejected`, and take
  the credential from a vault provider for the API host only.
- **Turn.** One HTTPS call through the vault with the exact rendered input.
  Fail with `runtime_protocol_error` at `turn` if the answer is empty, over
  `max_final_text_bytes`, or shows any tool call.
- **`resolved`.** `harness: "grok_bot"`, `harness_version` from the API
  response, and `served_model` set to the model that response names, or null.
  The key is always present.
- **`usage`.** A `TurnUsage::from_grok_bot_*` constructor in
  `src/execution/usage.rs`, only if the API reports per-turn tokens or cost.

Until then, the honest Grok options belong to other harnesses.

## Decisions for the owner

- **Keep `grok_bot` unregistered** (this change's position) until xAI
  documents a programmatic, tool-free Bot surface.
- **A Grok model turn belongs on a Grok Build harness, not on `grok_bot`.**
  Grok Build is xAI's official headless CLI (`grok -p`, `--output-format json`,
  `--tools`, `--disallowed-tools`, `--no-memory`, `--disable-web-search`; see
  the CLI reference under Sources). This repo does not register that harness.
  Whether another change adds it is outside this assessment.
- **Optional: a direct xAI Responses API harness.** A small in-guest client
  for `POST https://api.x.ai/v1/responses` with no `tools`, keyed by an xAI
  vault provider. `api.x.ai` is already in the libkrun standard egress list.
  It would be a new harness named for what it is (for example `xai_api`), not
  `grok_bot`, and it needs an owner decision because it is not a bundled
  third-party harness.

## Sources

Fetched 2026-10-11. All are official SpaceXAI (formerly xAI) or Cursor
documentation; Grok Bot runs in Cursor's cloud and Cursor hosts part of its
help.

- [overview]: Grok Bot overview, <https://docs.x.ai/grok-bot/overview> (updated 2026-10-07)
- [bots]: Create and manage Bots, <https://docs.x.ai/grok-bot/bots> (updated 2026-10-06)
- [computer]: Use the computer and apps, <https://docs.x.ai/grok-bot/computer-and-apps>
- [approvals]: Approvals, security, and privacy, <https://docs.x.ai/grok-bot/approvals-security-and-privacy>
- [security]: Grok Bot security, <https://docs.x.ai/grok-bot/security>
- [teams]: Grok Bot for teams and enterprises, <https://docs.x.ai/grok-bot/teams-and-enterprises>
- [routines]: Skills and routines, <https://docs.x.ai/grok-bot/skills-routines-and-automations>
- [webhook]: Cursor help, Routines (webhook trigger), <https://cursor.com/help/grok-bot/routines>
- [api-ref]: REST API reference, <https://docs.x.ai/developers/rest-api-reference/inference> (updated 2026-10-08)
- Grok Build headless mode, for contrast: <https://docs.x.ai/build/cli/headless-scripting>, <https://docs.x.ai/build/cli/reference>

[overview]: https://docs.x.ai/grok-bot/overview
[bots]: https://docs.x.ai/grok-bot/bots
[computer]: https://docs.x.ai/grok-bot/computer-and-apps
[approvals]: https://docs.x.ai/grok-bot/approvals-security-and-privacy
[security]: https://docs.x.ai/grok-bot/security
[teams]: https://docs.x.ai/grok-bot/teams-and-enterprises
[routines]: https://docs.x.ai/grok-bot/skills-routines-and-automations
[webhook]: https://cursor.com/help/grok-bot/routines
[api-ref]: https://docs.x.ai/developers/rest-api-reference/inference
