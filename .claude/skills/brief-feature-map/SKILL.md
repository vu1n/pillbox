---
name: brief-feature-map
description: Seed or extend a repo's brief feature map — one `type: features` doc with an anchor per feature (user-facing or internal), its `paths:` globs, and a Gotchas section. Invoke for "map the features", "add a feature to the map", "seed the feature map".
disable-model-invocation: true
---

# brief feature map — what each part is, where it lives, what bites

A feature map lets an agent go from a diff to the features it touches
(`brief features <files>`) and read their Gotchas before changing code. It is
descriptive, not a decision: editing it needs no ratification, and its `paths` never
gate code. Its one hard rule, enforced by `brief check`, is that every glob matches at
least one tracked file.

## 1. Find the features

Start with the top 5–10 areas, not everything. Take them from routes, commands, menus,
packages, and existing specs, and include **internal areas** (a store, a scheduler, an
auth layer) — a lesson about internals needs a home too. A feature is something a person
would name in a sentence ("search", "the gate"), not a directory.

## 2. Write `.brief/docs/features.md`

````markdown
---
id: features
type: features
title: <project> feature map
---

# <project> feature map

<!-- brief:anchor search -->
## Search

```yaml
paths:
  - "src/search/**"
  - "cli/commands/search.ts"
```

One paragraph: what it does, from its user's point of view (for an internal area, its
caller's).

### Gotchas
- Traps that waste or invalidate work here, each one concrete and checkable.
````

- **Anchor id** — short, kebab-case, stable. Others will reference
  `doc://<project>/features@latest#<id>`, so never rename one casually.
- **`paths:`** — derive them from the code that *implements* the feature, not the
  directory it happens to sit in. Shared files can appear under several features.
- **Gotchas** — only what you verified in the code or saw fail. Leave the section out
  rather than pad it; it grows as agents hit traps.
- Don't restate decisions. If a gotcha is really a ratified constraint, link its
  `doc://` ref instead.

## 3. Check it

```sh
brief check           # blocks any glob that matches no tracked file
brief doctor          # warns on a feature with no paths
brief features src/search/index.ts   # spot-check a file maps where you expect
```

Commit the map with the change that needed it.
