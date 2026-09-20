// The light Markdown an answer is written in: `**bold**` card names,
// `` `702.19b` `` rule numbers and the odd `*emphasis*`. The synthesis prompt
// asks for exactly that and no more (crates/bot/src/prompts/synth_system.md),
// because Discord renders it. This renders the same three marks on the page
// so an answer does not show its asterisks and backticks.
//
// Nothing else is interpreted: no links, no images, no HTML. Every piece is a
// JSX child, so the text stays untrusted text. A mark with no partner on the
// same line is left exactly as written.

import type { JSX } from "solid-js";

import Symbols from "./Symbols";

/** One run of an answer. */
export type Span =
  | { kind: "text"; text: string }
  | { kind: "strong"; text: string }
  | { kind: "em"; text: string }
  | { kind: "code"; text: string };

const MARKS = [
  { open: "**", kind: "strong" },
  { open: "`", kind: "code" },
  { open: "*", kind: "em" },
] as const;

/** A character a star must not touch for it to mean emphasis. */
const JOINS = /[\p{L}\p{N}/+*\\-]/u;
/** Emphasised text starts on a letter or digit and ends on one or on closing punctuation. */
const WORDS = /^[\p{L}\p{N}](?:.*[\p{L}\p{N}.!?)"'’”])?$/u;

// Whether `*inner*` at `open`..`close` is emphasis. Magic writes a variable
// power and toughness with stars (star-slash-star, with or without a +1) and
// people write 2*3*4, so a star only counts when it wraps words and stands
// clear of the text around it. Anything else keeps its stars. (Line comments
// on purpose: a star next to a slash would end a block comment.)
function emphasis(text: string, open: number, inner: string, close: number): boolean {
  const before = open > 0 ? text.charAt(open - 1) : "";
  const after = text.charAt(close + 1);
  return WORDS.test(inner) && !JOINS.test(before) && !JOINS.test(after);
}

/**
 * Split `text` into marked and plain runs. One pass, left to right; a mark
 * opens only when its partner closes it later on the same line around
 * non-empty text that does not start or end with a space, and a single star
 * must also pass [`emphasis`]. Marks do not nest: what is inside one is text.
 */
export function spans(text: string): Span[] {
  const out: Span[] = [];
  let plain = "";
  let i = 0;
  outer: while (i < text.length) {
    for (const mark of MARKS) {
      if (!text.startsWith(mark.open, i)) continue;
      const from = i + mark.open.length;
      const close = text.indexOf(mark.open, from);
      const inner = close < 0 ? "" : text.slice(from, close);
      const ok =
        inner.length > 0 &&
        !inner.includes("\n") &&
        inner.trim() === inner &&
        (mark.kind !== "em" || emphasis(text, i, inner, close));
      if (!ok) continue;
      if (plain) out.push({ kind: "text", text: plain });
      plain = "";
      out.push({ kind: mark.kind, text: inner });
      i = close + mark.open.length;
      continue outer;
    }
    plain += text[i];
    i += 1;
  }
  if (plain) out.push({ kind: "text", text: plain });
  return out;
}

/** `text` with its light Markdown and its card symbols drawn. */
export default function Markdown(props: { text: string }): JSX.Element {
  return (
    <>
      {spans(props.text).map((s) => {
        switch (s.kind) {
          case "strong":
            return (
              <strong>
                <Symbols text={s.text} />
              </strong>
            );
          case "em":
            return (
              <em>
                <Symbols text={s.text} />
              </em>
            );
          case "code":
            // A rule number or a literal: no symbols inside.
            return <code>{s.text}</code>;
          default:
            return <Symbols text={s.text} />;
        }
      })}
    </>
  );
}
