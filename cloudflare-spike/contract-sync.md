# Keeping the TS and Rust §0 contracts in sync

`contract.rs` is **canonical**. The TS contract is **generated** from it, through a
JSON Schema, so the two can't silently drift.

```
src/contract.rs                 (canonical, hand-written Rust)
      │  schemars  ─ `cargo test --features contract-schema` (golden-file gate)
      ▼
cloudflare-spike/contract.schema.json     (canonical JSON Schema, committed)
      │  json-schema-to-typescript ─ `npm run gen:contract`
      ▼
cloudflare-spike/src/contract.generated.ts   (generated, committed, do NOT edit)
      │  hand-written extension (spec-ahead + the open catch-all)
      ▼
cloudflare-spike/src/contract.ts          (what the spike imports)
```

## Why schemars (not typeshare)

typeshare 1.x **cannot** represent our `Payload`: it's an *internally*-tagged serde
enum (`#[serde(tag = "type")]` → `{"type":"tool_call","toolCallId":…}`), and
typeshare only supports *adjacently*-tagged enums (`{"type":…,"content":{…}}`). It
also rejects `u64`. schemars honors the internally-tagged shape (a `oneOf` of
`$ref`-plus-`const`-discriminant branches) and `u64`, and — because every optional
field pairs `#[serde(default, skip_serializing_if=…)]` — emits them as
**not-required**, so they land optional in TS. That's the most faithful capture.

## The two gates

| Gate | Runs | Catches |
|---|---|---|
| `contract_schema_is_current` (`src/contract.rs`) | `cargo test --features contract-schema` | `contract.rs` changed without regenerating `contract.schema.json` |
| `check:contract` (`package.json`) | `npm run check:contract` | `contract.schema.json` changed without regenerating `contract.generated.ts` |

Regenerate after a deliberate contract change:

```sh
# Rust → schema
UPDATE_SCHEMA=1 cargo test --features contract-schema contract_schema_is_current
# schema → TS
cd cloudflare-spike && npm run gen:contract
```

## The hand-written extension layer (`contract.ts`)

The generated file is the Rust-backed core. `contract.ts` re-exports it and adds the
two things the generator *can't* produce:

1. **The open forward-compat catch-all.** `contract.rs`'s `#[serde(other)] Unknown`
   renders as a literal `{type:"unknown"}` branch in the schema — not the
   absorb-any-tag `{type:string;[k]:unknown}` the wire actually needs. Swap it here.
2. **Spec-ahead fields/payloads** the spike uses but `contract.rs` hasn't built yet
   (`docs/session-event-log.md`): the `v`/`causationId`/`class`/`idempotencyKey`
   envelope fields and the `driver_changed` payload. When one graduates into
   `contract.rs`, delete it here — it starts coming from the generated core.

Template (verify the generated export names first):

```ts
import type { Event as GenEvent, Payload as GenPayload, Actor } from "./contract.generated.js";
export type { Actor };

export interface Event extends Omit<GenEvent, "payload"> {
  payload: Payload;
  v?: number;            // spec-ahead — session-event-log.md §Envelope
  causationId?: number;
  class?: "content" | "signal";
  idempotencyKey?: string;
}

export type Payload =
  | Exclude<GenPayload, { type: "unknown" }>   // drop the bogus literal variant
  | { type: "driver_changed"; from?: Actor; to?: Actor; mode: "granted" | "requested" | "stolen" | "released" }
  | { type: string; [k: string]: unknown };    // the real open catch-all
```

## Cutover status (remaining, needs an env with npm)

The Rust half is landed + verified. To finish, in a checkout with `npm install`:

1. `cd cloudflare-spike && npm install` (adds `json-schema-to-typescript`; refreshes the lockfile).
2. `npm run gen:contract` → produces `src/contract.generated.ts`; confirm the export names match the template above.
3. Restructure `src/contract.ts` to the extension layer (template above); `tsc -noEmit` to confirm the spike still type-checks.
4. Delete `check-contract-parity.py` and switch `scripts/smoke/cf.sh` to run `cargo test --features contract-schema contract_schema_is_current` + `npm run check:contract` instead.

Until step 4, `check-contract-parity.py` stays as the interim `contract.rs ↔ contract.ts` gate.
