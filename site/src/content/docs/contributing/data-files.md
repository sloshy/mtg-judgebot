---
title: Data files
description: The YAML files that shape the bot without code changes, and how each is loaded.
sidebar:
  order: 2
---

Four files in the repository are data the bot depends on. Pull requests that improve them
are welcome.

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
  "sheoldred") are left out, so the user gets a "did you mean…?" prompt.
- The resolver also tries an alias with a trailing possessive stripped ("bob's"), and as
  a suffix.

The list is compiled into the binary. After an edit, rebuild and run
`judgebot ingest aliases` with no file, which loads the built-in copy (as `init` and the
scheduled refresh do). Naming a file (`judgebot ingest aliases data/aliases.yaml`) loads
that file as the operator's own list, which opts it out of built-in updates
([Reloading](#reloading)).

## `data/notes.yaml`: nightmare cards

Hand-written Markdown notes keyed by card name. They cover cards whose interactions the
rules text alone explains badly: Blood Moon, Urborg, Humility, and their kind. Whenever a
note's card is recognised in a question, the note is added to what the model reads.

Notes are hints. The Comprehensive Rules still govern, and a note cannot be cited. A note
should cite rule numbers so the model can quote the rules rather than the note. A note can
also settle a card the rules text and rulings leave open, such as Academy Manufactor's
"one of each".

Loading works as for aliases: rebuild, then `judgebot ingest notes` with no file. Naming a
file opts the list out of built-in updates ([Reloading](#reloading)).

## `data/categories.yaml`: the taxonomy

The categories a question is sorted into, each with the CR sections always shown to the
model for it. `crates/core/build.rs` generates the `Category` enum from this file at build
time. Editing it means a recompile, and every `match` over the enum has to be updated.

The extractor must return a first (primary) category. An empty classification fails the
model's output schema. The primary category's sections lead the retrieved material.

### Sections

A category names its CR text by title, never by number. A CR release that inserts a
section moves every later number, and the titles stay. The numbers are looked up in the
loaded CR at each CR load and each refresh. An entry of `sections` is one of:

| Entry | Names |
| --- | --- |
| `Split Cards` | A whole section. |
| `{ part: Zones, section: General }` | A section whose title several parts use. `General` is the first section of every part. |
| `{ section: Keyword Abilities, rule: Lifelink }` | One rule, by its title. |
| `{ section: Commander, number: "5" }` | One rule or sub-rule that has no title, by its number inside the section. Quote it. |

The build rejects a rule number written where a title belongs. Whether a title exists is
known only where a CR is loaded, so that is checked by the load:

- An entry that names nothing is left out of its category. The load that first finds it
  logs it at WARN, and every refresh lists it at INFO. The category's other entries
  still apply.
- A section a new release adds that no category names is logged at WARN. Whether a
  category should include it is a decision for this file.
- The scheduled refresh sends both to `JUDGE_ALERT_WEBHOOK`, at most once each.

A numbered entry (`number: "5"`) is checked only for existing. A release that inserts a
rule before it inside its section moves it without a report, so prefer a title wherever
the rule has one.

After editing the file, run `judgebot ingest rules latest` against a loaded database and
read its log: `taxonomy entries that name nothing in the loaded CR` lists every entry
that did not resolve.

## `eval/gold.yaml`: the gold set

Rules questions, adversarially verified except one, whose reference answer is a human
correction of a reported wrong answer. Each question has:

- The cards it mentions and the categories expected.
- Its `source` (`CR`, `Commander`, `Tournament` or `OutOfScope`), or a list when more
  than one is correct (`[Commander, CR]` for a question about Vintage and Commander).
  The first is the one the gold extraction uses. The sources in a list must agree on
  whether the bot answers.
- The rule ids a correct answer must cite (`decisive_rule_ids`), which recall is scored
  on, and background rule ids a good answer may leave out (`supporting_rule_ids`). An id
  is in one list or the other, and the retrieval gate counts both.
- Alternate ids that state the same fact (`equivalent_rule_ids`, keyed by an id from
  either list).
- A reference answer.

Rule ids **must be quoted** (`'614.12'`). Unquoted, YAML reads `702.10` as a float and
drops the zero, so the loader rejects unquoted ids.

- `judge-eval recall` checks retrieval against it for free.
- `judge-eval answer` runs the full pipeline (about $2.20 for the set).
- `judge-eval grade` has a model grade each answer against its quotes and the reference
  answer (about $0.40 for the set). A grade made against a reference since edited is
  made again.
- `rescore` re-scores citation recall in stored runs after an edit.

Extend it when adding capability, and re-verify rule ids on each CR release.

## Reloading

Each load records where its list came from: `builtin` (`init`, or `aliases` / `notes`
with no file) or `file`, with a digest of the YAML. The scheduled refresh's `lists` step
reloads a built-in list when the binary's copy differs, so an edit merged here reaches
every deployment that uses the built-in lists on its first refresh after the upgrade. A
list loaded from a file is the operator's, and neither the refresh nor `init` touches it.
Loading a file therefore opts that list out of built-in updates, even a file identical to
the built-in copy, until `aliases` or `notes` is run again with no file. The image carries
binaries only, no `data/`, so on a deploy host without a Rust toolchain, mount the file in:
`docker compose run --rm -v ./data:/data:ro refresh aliases /data/aliases.yaml`.

A list loaded by a release before the record existed has none. The refresh adopts it as
built-in only when its table holds exactly what the binary's copy, or an earlier
release's built-in copy, loads. Otherwise it leaves the list alone and logs a warning
until the operator runs `aliases <file>` (keep theirs) or `aliases` with no file (take
the built-in copy), and likewise for `notes`.

The earlier copies are `data/legacy/`, listed in `LEGACY` in
`crates/bot/src/ingest/aliases.rs` and `notes.rs`. **A change to a list's entries (not
only its comments) copies the file as it was before the change into `data/legacy/` and
appends it to `LEGACY`**, so a database still holding that release's rows is recognised.
