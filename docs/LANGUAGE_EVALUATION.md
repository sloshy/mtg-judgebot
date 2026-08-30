# Language evaluation — correctness first

Criteria (in priority order): (1) what the compiler enforces, (2) ergonomics
of expressing the domain in `ARCHITECTURE.md` §5, (3) whether the ecosystem
makes it buildable without inventing infrastructure. Code volume and
build-time are explicitly NOT criteria. All library facts below were verified
against upstream sources on 2026-08-29; items marked [U] are unverified.

## 1. The invariants we want the compiler to hold

| # | Invariant | Where it bites |
|---|---|---|
| I1 | `Resolution`/`Citation`/`JudgeError`/`Source` are closed sums; every consumer handles every case | "Ambiguous can't be silently guessed" |
| I2 | `Card.faces` is non-empty; `RuleId` matches `^\d{3}(\.\d+[a-z]?)?$`; rating ∈ {1,2,3} | Invalid data unconstructible |
| I3 | A `Verdict` cannot be persisted or sent to Discord until citation validation has run | Hallucinated cites never reach users |
| I4 | The synthesis tool loop runs at most one `lookup_rules` round | Bounded cost, no runaway loops |
| I5 | The LLM structured-output schema and the type we decode into are one definition | Schema drift is a compile error, not a 400 |
| I6 | SQL parameter/column types match the schema | Retrieval queries can't silently return wrong shapes |
| I7 | `core` (resolution, context assembly, validation) performs no I/O | Pure logic is testable and can't sneak in an LLM call |
| I8 | Errors on the judge path are values of `JudgeError`, not exceptions | Discord layer must render every failure |
| I9 | No `null`/`undefined` reaches domain code | Java/JS boundary leaks |

## 2. Scorecard

✅ compiler-enforced · 🟡 enforced with a library/pattern, or at test time · ⚠️ discipline only · ❌ not expressible

| | Scala 3 (JVM) | Rust | Haskell | Scala.js | F# | TypeScript | Kotlin | Go |
|---|---|---|---|---|---|---|---|---|
| I1 exhaustive sums | ✅ | ✅ | ✅ | ✅ (Scala side) / ⚠️ through facades | 🟡 warn unless `--warnaserror` | 🟡 `satisfies never` | ✅ | ❌ |
| I2 refined values | 🟡 Iron (literals at compile time, boundary at runtime) | 🟡 nutype + `nonempty` | 🟡 smart ctors; LiquidHaskell (fragile toolchain) | 🟡 Iron | ⚠️ | ⚠️ zod runtime | ⚠️ | ❌ |
| I3 validated-verdict phantom | ✅ opaque/phantom type | ✅ typestate `Verdict<Validated>` | ✅ GADT/phantom `Verdict 'Validated` | ✅ | ✅ phantom | ⚠️ branding | ⚠️ | ❌ |
| I4 one tool round | 🟡 encodable via phantom state | ✅ typestate (idiomatic) | ✅ phantom/indexed | 🟡 | 🟡 | ⚠️ | ⚠️ | ❌ |
| I5 single-sourced schema | 🟡 tapir `Schema.derived` + circe = two derivations; needs round-trip test | ✅ schemars from the serde struct (needs `oneOf→anyOf` transform) | ✅ autodocodec: schema and parser from ONE codec | ⚠️ zod not representable through facade | 🟡 FSharp.Data.JsonSchema [U vs Claude subset] | ✅ zod + `zodOutputFormat` | 🟡 Jackson-derived via SDK | 🟡 |
| I6 typed SQL | 🟡 Doobie `check` at test time vs live DB (nullability "weak"); pgvector via `PGobject` [U w/ check] | ✅ sqlx compile-time, pgvector 0.4.2 supports sqlx 0.9 | 🟡 rel8/squeal typed at compile time; pgvector codec hand-written | 🟡 Skunk codecs; pgvector codec hand-written | 🟡 SqlHydra codegen + pgvector pkg | 🟡 Drizzle inferred types | 🟡 jOOQ codegen | 🟡 sqlc codegen |
| I7 effect tracking | ✅ `IO` / tagless-final | ❌ `async fn` is opaque | ✅ effectful/bluefin (typed effect sets) | ✅ | ❌ | ❌ | ❌ | ❌ |
| I8 typed errors | 🟡 `EitherT` by discipline; `IO` errors are `Throwable` | ✅ `Result` + `#[must_use]` + `unwrap_used` deny | ✅ `Either`/effectful `Error e` | 🟡 | 🟡 `Result` | ⚠️ neverthrow; `throw` escapes | ⚠️ | ⚠️ |
| I9 null-safety | ⚠️ `-Yexplicit-nulls` experimental; Java "flexible types" make JDA/SDK nulls silent | ✅ | ✅ | ⚠️ facades self-declare nullability | ⚠️ | ✅ strict | ✅ | ❌ |
| Official Anthropic SDK | ✅ Java 2.59 (structured outputs GA) | ❌ hand-written reqwest client (~300–500 LOC) | 🟡 `claude` 1.5 (Mercury-maintained, structured outputs) | ⚠️ TS SDK via facade | ✅ C# (beta) | ✅ | ✅ Java | ✅ |
| Discord library | JDA 6.5 | serenity 0.12.5 + poise 0.6.2 | discord-haskell 1.19 (components, threads) | discord.js via facade | Discord.Net / NetCord | discord.js | Kord | discordgo/disgo |
| Ecosystem risk | Low | Low (SDK is ours) | Medium (single-maintainer libs, GHC toolchain) | Medium-high (facades, bundling) | Low | Low | Low | Low |

Disqualified: OCaml 5 (no viable Discord lib; effects untyped), Gleam (no
typeclasses/effects, untyped FFI), Elixir (no user annotations/exhaustiveness
yet), Swift (community SDK only, untyped SQL), Unison/Idris/Lean (no ecosystem).

## 3. Findings that changed the picture

- **Scala.js on Node is a net correctness loss.** Every pure-Scala library
  cross-builds fine, so the JS target buys nothing there; the two things you
  would import (discord.js, `@anthropic-ai/sdk`) enter as unchecked facades
  — literal unions lose exhaustiveness, nullability is self-declared, errors
  arrive as `js.JavaScriptException`, and `zodOutputFormat` can't be
  represented faithfully (ScalablyTyped #535 is the same failure shape). On
  the JVM those boundaries are compiled, checked Java types. Bundling for
  Node is also unsettled (scalajs-bundler dormant since 2023).
- **Scala's weaknesses cluster at the Java boundary** (I5, I8, I9). They can
  be quarantined: `-Yexplicit-nulls` in `core` only; a thin `llm`/`discord`
  adapter layer that converts to domain ADTs immediately; a round-trip test
  pinning tapir schema ↔ circe decoder.
- **Rust has no effect tracking, and that is its only structural gap.**
  Everything else (I1–I6, I8, I9) is enforced, and I3/I4 are more idiomatic
  in Rust (typestate) than anywhere else. The Anthropic client is ours to
  maintain; the wire surface is one endpoint.
- **Haskell is the only language that scores ✅ on I5, I7, I8 and I9
  simultaneously.** autodocodec's schema-from-the-parser is the strongest
  single-sourcing story found; effectful gives per-port effect sets
  (`Retriever :> es` cannot call `Anthropic`). Its risk is ecosystem
  concentration (discord-haskell and `claude` are small-team) and toolchain
  friction; the pgvector codec is a few lines.

## 4. Ranking on the stated criteria

1. **Haskell** — most invariants enforced, effect tracking of the kind you
   already prefer, best schema single-sourcing. Costs: ecosystem depth and
   GHC toolchain; LiquidHaskell not worth its version-pinning.
2. **Rust** — strongest on value-level invariants and the only compile-time
   checked SQL; loses only on effect tracking. Costs: own the API client.
3. **Scala 3 (JVM)** — strong core; every gap is a Java-interop leak that
   must be fenced by convention and tests rather than the compiler.
   Best-known to the author; best official-SDK story of the three.
4. F# — ecosystem-safe, guarantees roughly Kotlin-plus-DUs.
5. Scala.js — dominated by JVM Scala.
6. TypeScript, Kotlin, Go — below the bar for "compiler over test suite".

The gap between 1–3 is much smaller than the gap from 3 to 4+. All three are
viable; none is a mistake.

## 5. Decision (2026-08-29)

**Rust selected.** Rationale: enforces I1–I6, I8, I9 at compile time; the
effect-tracking gap (I7) is accepted and fenced by keeping `core` free of
I/O crates. Owning the Anthropic client is accepted. See `proposals/rust.md`.

## 6. Original recommendation (kept for the record)

If compiler enforcement is the deciding criterion and familiarity is not:
**Haskell**, with Rust as the runner-up if the Haskell ecosystem risk is
unacceptable. If familiarity with Scala/Typelevel is allowed to count as
"ergonomics", **Scala 3 on the JVM** remains a defensible choice provided the
Java boundary is fenced as described in `proposals/scala.md`. Scala.js is not
recommended.

Proposals: `proposals/scala.md`, `proposals/rust.md`, `proposals/typescript.md`
(kept for contrast). A `haskell.md` can be written to the same depth on request.
