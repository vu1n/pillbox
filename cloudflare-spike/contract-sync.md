# The §0 contract pipeline: Rust → JSON Schema → TypeScript

`src/contract.rs` is the only hand-written definition of the §0 session event
contract (`Event`, `Actor`, `Payload` and every type they reach). Everything else
is generated from it and committed:

```
src/contract.rs                        hand-written; serde attrs define the wire
      │  schemars (feature `contract-schema`), test `contract_schema_is_current`
      ▼
cloudflare-spike/contract.schema.json  generated, committed, do not edit
      │  json-schema-to-typescript (`npm run gen:contract`)
      ▼
cloudflare-spike/src/contract.ts       generated, committed, do not edit
```

TS code imports the generated types (`Payload`, and enums such as `ToolStatus` /
`AttentionReason`) from `./contract.js`. If a consumer needs a shape `tsc` rejects,
change `contract.rs` and regenerate; never hand-edit or wrap `contract.ts`.

## Changing the contract

```sh
# 1. edit src/contract.rs, then Rust → schema
UPDATE_SCHEMA=1 cargo test --no-default-features --features contract-schema contract_schema_is_current
# 2. schema → TS
cd cloudflare-spike && npm run gen:contract
```

Commit `contract.rs`, `contract.schema.json` and `src/contract.ts` together.

## The two gates (both in CI)

| Gate | Command | Fails when |
|---|---|---|
| Rust → schema | `cargo test --no-default-features --features contract-schema contract_schema_is_current` (ubuntu leg of the `test` job) | `contract.rs` changed without regenerating `contract.schema.json` |
| schema → TS | `npm run check:contract` in `cloudflare-spike/` (the `cloudflare` job and `scripts/smoke/cf.sh`) | `src/contract.ts` differs from what json2ts generates from the schema (compared in memory; no file or git index is touched) |

## Generator choices

Both npm scripts run `scripts/gen-contract.mjs`, which holds the json2ts options.


- **schemars, not typeshare.** `Payload` is an internally-tagged serde enum
  (`#[serde(tag = "type")]`), which typeshare cannot represent, and typeshare rejects
  `u64`. schemars follows the serde attrs: camelCase fields, a `type` const per
  variant, and `default`/`skip_serializing_if` fields as not-required (optional in TS).
- **Tag in its own `allOf` branch.** schemars renders a struct variant as the struct's
  `$ref` with a sibling `type` property, and json2ts drops one of the two. The
  `tag_beside_ref_to_all_of` transform in `src/contract.rs` emits
  `allOf: [{$ref}, {type: const}]` instead, which json2ts renders as
  `MessageStart & {type: "message_start"}`, a union `tsc` narrows on `type`.
- **`additionalProperties: false`.** The schema leaves `additionalProperties` unset, so
  json2ts would add `[k: string]: unknown` to every interface and turn off
  excess-property checks when TS builds a payload. Rust never emits extra fields, so
  the generated interfaces are closed.
- **`unknownAny`.** `serde_json::Value` fields (`ToolCall.input`, `Custom.payload`)
  become `unknown`, not `any`.
- **`{type: "unknown"}` stays.** `Payload::Unknown` is `#[serde(other)]`: Rust reads
  any unmodelled tag as `Unknown` and re-serializes it as `{"type":"unknown"}`, so the
  generated branch is what Rust actually emits.

## What the TS types do not check

The types carry shapes only. Rust's hand-written decode checks (for example
a reported `EffectiveRuntimeLimits` must name at least one limit, a rule
only its hand-written `Deserialize` holds) and integer ranges (`u64` becomes
`number`) are enforced when Rust reads the event, not by `tsc`.

The `Payload` union is closed: a TS producer can emit only what `contract.rs`
declares, so a TS reader of TS-produced evidence sees no other tag.

Payloads and envelope fields that `docs/session-event-log.md` specs but
`contract.rs` does not define (`driver_changed`; `v`, `causationId`, `class`,
`idempotencyKey`) are in neither contract. They reach TS only by landing in
`contract.rs` first.
