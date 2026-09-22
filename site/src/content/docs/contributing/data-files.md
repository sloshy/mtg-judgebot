---
title: Data files
description: The YAML files that shape the bot without code changes, and how each is loaded.
sidebar:
  order: 2
---

Four files in the repository are data the bot depends on. Pull requests that improve them
are the easiest way to make the bot better.

| File | What it is | How it is used |
| --- | --- | --- |
| `data/aliases.yaml` | Card nicknames | Loaded into the database |
| `data/notes.yaml` | Notes on hard cards | Loaded into the database |
| `data/categories.yaml` | Question categories | Compiled into the binary, and loaded |
| `eval/gold.yaml` | Reference questions | Drives the evaluation |

## `data/aliases.yaml`: nicknames

A flat mapping of nickname to canonical card name: `bob: Dark Confidant`,
`goyf: Tarmogoyf`, `t3feri: Teferi, Time Raveler`.

- Aliases are lowercased on load. Each must match a current card name or face name,
  ignoring case.
- Nicknames that could mean several cards (the Tron lands, "karn", "emrakul",
  "sheoldred") are left out on purpose. Ambiguity goes to the user as a "did you mean…?"
  prompt, never to a guess.
- The resolver also tries an alias with a trailing possessive stripped ("bob's"), and as
  a suffix.

Load with `judge-ingest aliases data/aliases.yaml`. With no file, `aliases` loads the copy
compiled into the binary. That is what `init` and a container use, so an edit reaches an
image only through a rebuild, or by mounting the file and naming it.

## `data/notes.yaml`: nightmare cards

Hand-written Markdown notes keyed by card name. They cover cards whose interactions the
rules text alone explains badly: Blood Moon, Urborg, Humility, and their kind. Whenever a
note's card is recognised in a question, the note is added to what the model reads.

Notes are hints. The Comprehensive Rules still govern. A note should cite rule numbers so
the model can quote the rules rather than the note. Load with
`judge-ingest notes data/notes.yaml`, or `judge-ingest notes` for the built-in copy.

## `data/categories.yaml`: the taxonomy

The categories a question is sorted into, each with the CR sections always shown to the
model for it. `crates/core/build.rs` generates the `Category` enum from this file at build
time. Editing it means a recompile, and every `match` over the enum has to be updated.

The extractor must return a first (primary) category. An empty classification fails the
model's output schema. The primary category's sections lead the retrieved material.

## `eval/gold.yaml`: the gold set

Adversarially verified rules questions. Each question has:

- The cards it mentions and the categories expected.
- The rule ids a correct answer must cite (`expected_rule_ids`).
- Alternate ids that state the same fact (`equivalent_rule_ids`, keyed by an expected id).
- A reference answer.

Rule ids **must be quoted** (`'614.12'`). Unquoted, YAML reads `702.10` as a float and
drops the zero, so the loader rejects unquoted ids.

- `judge-eval recall` checks retrieval against it for free.
- `judge-eval answer` runs the full pipeline (about $2.50 for the set).
- `rescore` re-grades stored runs after an edit.

Extend it when adding capability, and re-verify rule ids on each CR release.

## Reloading

`aliases` and `notes` are not part of the nightly `refresh`. Run their `judge-ingest`
commands when the files change. The image carries binaries only, no `data/`, so on a
deploy host without a Rust toolchain, mount the file in:
`docker compose run --rm -v ./data:/data:ro refresh aliases /data/aliases.yaml`.
