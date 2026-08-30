# Proposal A — Scala 3 / Typelevel

Implements `docs/ARCHITECTURE.md`. Revised after review; library status verified 2026-08-29 (see `LANGUAGE_EVALUATION.md`).

## Stack

| Concern | Choice | Notes |
|---|---|---|
| Language | Scala 3.3 LTS or 3.8; `-Werror -Wall -Wunused:all -Wvalue-discard -Wnonunit-statement -Wsafe-init -language:strictEquality` via sbt-tpolecat | `-Xfatal-warnings` is a deprecated alias of `-Werror`. `-Yexplicit-nulls` is still experimental and Java "flexible types" make it silent at the JDA/SDK boundary — enable it in `core` only |
| Effects / streams | cats-effect 3, fs2 | fs2 for bulk ingest and CR parsing |
| DB | **Doobie** 1.0.0-RC13 + HikariCP, Flyway; **Iron** 3.3 (`iron-doobie`, `iron-circe`) for refined types | pgvector via pgvector-java `PGvector`/`Meta.Advanced.other[PGobject]("vector")` (`::vector` string cast is the documented fallback). Doobie `check` runs in tests against a live DB; nullability inference is documented as weak, and `check` on `vector` columns is unverified. Skunk 1.0.0 (Node/Native capable) is the alternative; iron-skunk is pinned to an older milestone |
| HTTP client | http4s Ember + circe | Scryfall and Voyage only |
| LLM | Anthropic **Java SDK** 2.59, `AnthropicOkHttpClientAsync`, wrapped with `IO.fromCompletableFuture` | Structured outputs are GA (`OutputConfig.builder().format(JsonOutputFormat…)`); `ThinkingConfigParam.ofAdaptive`; `Effort.LOW/HIGH`; `StopReason.REFUSAL` handled. **Schema single-sourcing:** derive with tapir `Schema.derived` + `TapirSchemaToJsonSchema`, plus a ~10-line post-pass adding `additionalProperties:false` (tapir omits it for case classes); a round-trip test pins the tapir schema to the circe decoder. Do not hand-write schemas. `cache_control` on the system prefix |
| Discord | JDA 6.5 | `ListenerAdapter` → `Queue[IO, Event]` via `Dispatcher`; bot = `Stream[IO, Event] => Stream[IO, Unit]`. No maintained Scala-native library exists (AckCord last published 2022). Convert JDA events to domain ADTs immediately so nothing downstream touches non-sealed Java types |
| Config / logging | ciris, log4cats | |
| Tests | munit + munit-cats-effect; testcontainers-scala for Postgres | |
| Build | sbt, three modules | |

## Modules

```
modules/
  core/    domain ADTs, ports (traits), judge(), citation validation, prompts — no IO deps beyond cats-effect
  ingest/  CLI (decline): scryfall sync, CR parser, embed
  bot/     Doobie repos, http4s clients, Anthropic façade, JDA adapter, main
```

`core` compiles in seconds and holds every unit test that matters; `bot` is
the only module with the heavy dependencies.

## Domain model

```scala
opaque type CardId = String;  opaque type RuleId = String;  opaque type CallId = Long

final case class Face(name: String, oracleText: String, manaCost: Option[String], typeLine: String)
final case class Card(id: CardId, name: String, layout: String, faces: NonEmptyList[Face])

enum Source     { case CR, Commander, Tournament, OutOfScope }
enum Confidence { case Low, Medium, High }
enum Rating     { case Incorrect, Partial, Correct }

// Category is generated from data/categories.yaml at build time (sbt sourceGenerators)
// so the YAML is the single source of truth and pattern matches stay exhaustive.
enum Category { case Layers, Replacement, Triggers, Stack, Copy, StateBased, Combat, Mana, Commander /* … */ }

enum Resolution:
  case Resolved(card: Card, via: MatchedVia)
  case Ambiguous(query: String, candidates: NonEmptyList[Card])
  case NotFound(query: String)

enum Citation:
  case Rule(id: RuleId, quote: String)
  case ScryfallRuling(card: CardId, idx: Int, quote: String)
  case PriorCall(id: CallId, quote: String)

final case class Context(cards: List[Card], rules: List[RuleChunk], rulings: List[Ruling],
                         glossary: List[GlossaryEntry], prior: List[RatedCall],
                         notes: List[NightmareNote], history: List[QA])

final case class Verdict(answer: String, confidence: Confidence, citations: List[Citation],
                         category: Category, source: Source, crVersion: String)

enum JudgeError:
  case AmbiguousCards(rs: NonEmptyList[Resolution.Ambiguous])
  case OutOfScope(source: Source)
  case BadCitation(c: Citation)
  case LlmRefused
  case Upstream(t: Throwable)

trait Extractor   { def extract(q: Question, history: List[QA]): IO[Extraction] } // spans + categories + source
trait Resolver    { def resolve(span: String): IO[Resolution] }
trait Retriever   { def retrieve(q: Question, cards: List[Card], e: Extraction): IO[Context] }
trait Synthesizer { def answer(q: Question, ctx: Context): IO[Either[JudgeError, Verdict]] }
trait Embedder    { def embed(texts: List[String]): IO[List[Array[Float]]] }
trait CallStore   { def save(q: Question, v: Verdict, ctx: Context): IO[CallId]; def rate(id: CallId, r: Rating, by: UserId): IO[Unit] }

object Judge:
  def apply(ex: Extractor, rs: Resolver, rt: Retriever, sy: Synthesizer)(q: Question, history: List[QA]): IO[Either[JudgeError, Verdict]] =
    (for
      e    <- EitherT.right(ex.extract(q, history))
      _    <- EitherT.cond[IO](e.source != Source.Tournament && e.source != Source.OutOfScope, (), JudgeError.OutOfScope(e.source))
      res  <- EitherT.right(e.cardSpans.traverse(rs.resolve))
      cards<- EitherT.fromEither[IO](collectResolved(res))     // Left(AmbiguousCards) if any Ambiguous
      ctx  <- EitherT.right(rt.retrieve(q, cards, e))
      v    <- EitherT(sy.answer(q, ctx))
    yield v).value
```

Citation validation lives in `core` as a pure function
`validate(ctx: Context, v: Verdict): Either[JudgeError.BadCitation, Verdict]`
and is applied inside `Synthesizer` with one retry.

## The tool-use round

The synthesizer declares one tool, `lookup_rules(ids: List[String])`. The
façade runs a manual loop bounded to one iteration: if the first response is
`tool_use`, fetch the requested chunks via `Retriever`, append them, and call
again with `tool_choice = none`. Two API calls max per question in `[5]`.

## Where the cost lands

- JDA → fs2 bridge: ~30 lines.
- Anthropic façade + JSON schemas for `Extraction` and `Verdict`: ~120 lines
  (the Java SDK's builders are verbose; keep them behind the façade).
- sbt source generator for `Category`: ~25 lines.
- Cold compile of `bot` ~1 min; incremental in `core` is fast.


## Fencing the Java boundary (the only place guarantees leak)

- Null: `-Yexplicit-nulls` in `core`; adapters wrap every SDK/JDA return in
  `Option` at the edge (neither library ships JSpecify annotations).
- Errors: `IO`'s error channel is `Throwable`; `JudgeError` rides in `EitherT`
  and adapters `attempt` every SDK call into `JudgeError.Upstream`.
- Exhaustiveness: SDK `ContentBlock` unions and JDA events are not sealed;
  use the SDK visitor/`accept` API and map to Scala enums with a catch-all
  that becomes an explicit `Unsupported` case.
- Schema: tapir schema and circe decoder are two derivations; a
  `munit` round-trip test (`decode(encode(sample))` + schema validation)
  is the fence. Tests, not the compiler.

## Risks specific to this stack

- Java SDK from Scala: builders + `Optional` + `JsonValue` are noisy; if the
  façade balloons past ~200 lines, switch to a raw http4s client against
  `/v1/messages` (~120 lines, but then we track API drift ourselves).
- JDA is callback-based and holds its own thread pool; must shut down cleanly
  in the `Resource` finalizer or the JVM won't exit.
- Only one contributor knows the stack; onboarding cost if that changes.
