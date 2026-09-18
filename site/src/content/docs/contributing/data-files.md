---
title: Data files
description: The YAML files that shape the bot without code changes, and how each is loaded.
sidebar:
  order: 2
---

Four files in the repository are data the bot depends on. Two load into the database,
one is compiled into the binary (and loaded), and one drives the evaluation. Pull requests
that improve them are the easiest way to make the bot better.

## `data/aliases.yaml`: nicknames

A flat mapping of nickname to canonical card name: `bob: Dark Confidant`,
`goyf: Tarmogoyf`, `t3feri: Teferi, Time Raveler`. Aliases are lowercased on load and
must match a current card name or face name case-insensitively. Nicknames that could mean
several cards (the Tron lands, "karn", "emrakul", "sheoldred") are left out on purpose.
Ambiguity goes to the user as a "did you mean…?" prompt, never to a guess. Load with
`judge-ingest aliases data/aliases.yaml`. The resolver also tries an alias with a trailing
possessive stripped ("bob's") and as a suffix.

## `data/notes.yaml`: nightmare cards

Hand-written Markdown notes keyed by card name, for cards whose interactions the rules
text alone explains badly: Blood Moon, Urborg, Humility, and their kind. A note is
injected into the model's context whenever its card resolves. Notes are hints. The
Comprehensive Rules still govern, and a note should cite rule numbers so the model can
quote the rules rather than the note. Load with `judge-ingest notes data/notes.yaml`.

## `data/categories.yaml`: the taxonomy

The question categories the extractor classifies into, each with the CR sections that are
always injected for it. `crates/core/build.rs` generates the `Category` enum from this file
at build time, so editing it is a recompile and every `match` over the enum has to follow.
The first category in the extractor's output is required (an empty classification fails
the model's schema), and its sections lead the retrieved material.

## `eval/gold.yaml`: the gold set

Adversarially verified rules questions with the cards they mention, the categories
expected, the rule ids a correct answer must cite (`expected_rule_ids`), alternate ids that
state the same fact (`equivalent_rule_ids`, keyed by an expected id), and a reference
answer. Rule ids **must be quoted** (`'614.12'`). Unquoted, YAML reads `702.10` as a float
and drops the zero, and the loader rejects it.

- `judge-eval recall` checks retrieval against it for free.
- `judge-eval answer` runs the full pipeline (about $2.50 for the set).
- `rescore` re-grades stored runs after an edit.

Extend it when adding capability, and re-verify rule ids on each CR release.

## Reloading

`aliases` and `notes` are not part of the nightly `refresh`. Run their `judge-ingest`
commands when the files change. The image carries binaries only, no `data/`. On a
deploy host without a Rust toolchain, mount the file in:
`docker compose run --rm -v ./data:/data:ro refresh aliases /data/aliases.yaml`.
