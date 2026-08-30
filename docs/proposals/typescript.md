# Proposal B — TypeScript (strict)

Implements `docs/ARCHITECTURE.md`. Written for direct comparison with
`scala.md`; same architecture, same domain model, different plumbing.

## Stack

| Concern | Choice | Notes |
|---|---|---|
| Language | TypeScript 5.x, `strict`, `noUncheckedIndexedAccess`, `exactOptionalPropertyTypes` | Node 22, ESM |
| Runtime style | `async`/`await`; **neverthrow** `Result<T, JudgeError>` for the pipeline | No effect system; errors are values, not exceptions, on the judge path |
| Validation | **zod** | One schema per domain type; used for LLM output, DB rows, YAML, and env |
| DB | **Drizzle ORM** + `postgres` driver; drizzle-kit migrations | Drizzle ships a `vector` column type and `cosineDistance` helper — pgvector is first-class, no custom codec |
| HTTP client | native `fetch` | Scryfall + Voyage; `voyageai` npm SDK is also fine |
| LLM | `@anthropic-ai/sdk` | `client.messages.parse` + `zodOutputFormat(schema)` gives typed structured output directly; `betaZodTool` for `lookup_rules`. Adaptive thinking; effort `low`/`high`; `cache_control` on system prefix; check `stop_reason === "refusal"` |
| Discord | **discord.js v14** | Slash commands, buttons for ratings and "did you mean", threads for follow-ups — all native |
| Config / logging | zod-parsed `process.env`, pino | |
| Tests | vitest; testcontainers for Postgres | |
| Build / tooling | pnpm, tsx (dev), tsc (check), biome (lint+fmt) | Single package, no monorepo |

## Layout

```
src/
  domain/      types.ts (zod schemas + inferred types), errors.ts, judge.ts (pure pipeline)
  ports/       extractor.ts, resolver.ts, retriever.ts, synthesizer.ts, embedder.ts, callStore.ts (interfaces)
  adapters/    anthropic/, drizzle/, scryfall.ts, voyage.ts, discord/
  ingest/      cli.ts, crParser.ts, scryfallSync.ts, embed.ts
  bot.ts       composition root
data/          aliases.yaml, categories.yaml, nightmare/*.md
eval/          gold.yaml, run.ts
```

## Domain model

```ts
import { z } from "zod";
import { Result } from "neverthrow";

export const Category = z.enum(loadCategoryIds("data/categories.yaml")); // literal tuple at module load; exhaustive `switch` via `satisfies never`
export const Source = z.enum(["CR", "Commander", "Tournament", "OutOfScope"]);
export const Confidence = z.enum(["Low", "Medium", "High"]);

export const Face = z.object({ name: z.string(), oracleText: z.string(), manaCost: z.string().optional(), typeLine: z.string() });
export const Card = z.object({ id: z.string().brand<"CardId">(), name: z.string(), layout: z.string(), faces: Face.array().nonempty() });

export type Resolution =
  | { kind: "resolved"; card: Card; via: MatchedVia }
  | { kind: "ambiguous"; query: string; candidates: [Card, ...Card[]] }
  | { kind: "notFound"; query: string };

export const Citation = z.discriminatedUnion("kind", [
  z.object({ kind: z.literal("rule"),    id: z.string(), quote: z.string() }),
  z.object({ kind: z.literal("ruling"),  card: z.string(), idx: z.number().int(), quote: z.string() }),
  z.object({ kind: z.literal("prior"),   id: z.number().int(), quote: z.string() }),
]);

export const Verdict = z.object({
  answer: z.string(), confidence: Confidence, citations: Citation.array(),
  category: Category, source: Source, crVersion: z.string(),
});
export type Verdict = z.infer<typeof Verdict>;

export type JudgeError =
  | { kind: "ambiguousCards"; resolutions: Extract<Resolution, { kind: "ambiguous" }>[] }
  | { kind: "outOfScope"; source: Source }
  | { kind: "badCitation"; citation: Citation }
  | { kind: "llmRefused" }
  | { kind: "upstream"; cause: unknown };

export interface Synthesizer { answer(q: Question, ctx: Context): Promise<Result<Verdict, JudgeError>> }
// …other ports analogous

export const judge = (deps: Deps) => async (q: Question, history: QA[]): Promise<Result<Verdict, JudgeError>> => {
  const e = await deps.extractor.extract(q, history);
  if (e.source === "Tournament" || e.source === "OutOfScope") return err({ kind: "outOfScope", source: e.source });
  const res = await Promise.all(e.cardSpans.map(deps.resolver.resolve));
  const cards = collectResolved(res);            // Result<Card[], JudgeError>
  if (cards.isErr()) return cards;
  const ctx = await deps.retriever.retrieve(q, cards.value, e);
  return deps.synthesizer.answer(q, ctx);
};
```

The same `Verdict` zod schema is passed to `zodOutputFormat` — the LLM output
contract and the domain type are literally one definition. Citation validation
is a pure function `validate(ctx, verdict): Result<Verdict, JudgeError>`.

## The tool-use round

`betaZodTool({ name: "lookup_rules", inputSchema: z.object({ ids: z.string().array() }), run })`
with `client.beta.messages.toolRunner(...)` and `max_iterations: 2`; or a
manual loop identical to the Scala version. The `run` function calls the
`Retriever`.

## Where the cost lands

- Almost nowhere: every library here is a first-class fit (Drizzle→pgvector,
  SDK→zod, discord.js→buttons/threads). No adapters to write.
- Discipline cost instead: TypeScript won't force exhaustiveness or
  totality; `satisfies never` checks and biome's `noSwitchDeclarations` /
  `useExhaustiveSwitchCases` must be turned on and kept on.
- Runtime type safety only where zod is applied; the DB boundary and LLM
  boundary are covered, internal code is trusted.



## Risks specific to this stack

- `Result` discipline erodes easily; a thrown exception inside an adapter
  bypasses the typed error channel. Mitigate with `ResultAsync.fromPromise` at
  every adapter boundary.
- Nullability/optional-property mistakes are the main bug class; the strict
  flags above are non-negotiable.
- Less pleasant modeling of invariants than Scala (no opaque types beyond
  branding, no `NonEmptyList` in the type system without a library).
