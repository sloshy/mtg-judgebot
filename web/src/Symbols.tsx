// Renders Magic's card symbols (`{W}`, `{2/U}`, `{T}`) inside a run of text as
// Scryfall's SVGs, the way the Discord side renders them as custom emoji.
//
// The SVGs are hotlinked from Scryfall's CDN, which is what the `svg_uri` in
// their symbology response is for; nothing is bundled or served from here.
// That does mean every answer fetches a handful of images from a third party,
// with `referrerpolicy="no-referrer"` so the page URL does not go with them.
// A symbol not in SYMBOLS — or one whose image fails to load — stays as the
// literal `{W}` Scryfall writes, so a stale table degrades instead of breaking.
//
// `split` and `lookup` mirror `Rendered::substitute` and `SymbolTable::tag` in
// crates/bot/src/discord/mana.rs: the same spellings resolve on both surfaces,
// and the same unresolvable text is left alone.

import { Show, createSignal } from "solid-js";

import { SYMBOLS, SymbolInfo } from "./symbols";

/** Where Scryfall serves the symbol SVGs. */
const SYMBOL_CDN = "https://svgs.scryfall.io/card-symbols";

/**
 * Longest symbol body worth looking at; `{1000000}` is the longest real one.
 * It also bounds the scan: a stray `{` can only look this far ahead for a `}`,
 * so text full of braces stays linear. Mirrors MAX_BODY_CHARS in
 * crates/core/src/symbol.rs.
 */
const MAX_BODY = 12;

/** One piece of a split run of text. */
type Piece =
  | { kind: "text"; text: string }
  | { kind: "symbol"; body: string; info: SymbolInfo };

/**
 * Split `text` into plain runs and known symbols. Anything in braces that is
 * not a symbol we have is left in the text exactly as written, including the
 * braces, and an unclosed brace never swallows the rest of the string.
 */
export function split(text: string): Piece[] {
  const pieces: Piece[] = [];
  let plain = "";
  let i = 0;
  while (i < text.length) {
    const open = text.indexOf("{", i);
    if (open < 0) {
      plain += text.slice(i);
      break;
    }
    plain += text.slice(i, open);
    const close = text.indexOf("}", open + 1);
    const body =
      close < 0 || close - open - 1 > MAX_BODY ? null : text.slice(open + 1, close);
    const info = body === null ? undefined : lookup(body);
    if (body === null || info === undefined) {
      // Not a symbol: keep the brace and carry on just past it, so `{{W}`
      // still finds the `{W}`.
      plain += "{";
      i = open + 1;
      continue;
    }
    if (plain) pieces.push({ kind: "text", text: plain });
    plain = "";
    pieces.push({ kind: "symbol", body, info });
    i = close + 1;
  }
  if (plain) pieces.push({ kind: "text", text: plain });
  return pieces;
}

/**
 * `W/U` and `w/u` and `WU` all name the same symbol. Slashes are dropped and
 * the rest uppercased, which is exactly what `judge_core::symbol::emoji_name`
 * does before lowercasing, so both surfaces accept the same set of spellings.
 */
const normalize = (body: string) => body.toUpperCase().replaceAll("/", "");

/**
 * SYMBOLS re-keyed by [`normalize`], on a null prototype so a body like
 * `constructor` cannot reach `Object.prototype` and hand back a function.
 * (`toUpperCase` happens to block that today; this does not depend on it.)
 */
const BY_NORMALIZED: Record<string, SymbolInfo | undefined> = Object.assign(
  Object.create(null) as Record<string, SymbolInfo | undefined>,
  Object.fromEntries(
    Object.entries(SYMBOLS).map(([body, info]) => [normalize(body), info]),
  ),
);

/**
 * The symbol for a body, tolerating the case and the order the writer put a
 * two-colour hybrid in. `{2/W}` and `{W/P}` have a fixed order and are never
 * flipped; no two-colour hybrid exists in both orders in Scryfall's set, so
 * flipping can never turn one real symbol into a different one — the same
 * reasoning as `judge_core::symbol::flipped`.
 */
function lookup(body: string): SymbolInfo | undefined {
  const direct = BY_NORMALIZED[normalize(body)];
  if (direct) return direct;
  const parts = body.toUpperCase().split("/");
  const colour = (s: string) => /^[WUBRGC]$/.test(s);
  if (parts.length === 2 && parts.every(colour)) {
    return BY_NORMALIZED[`${parts[1]}${parts[0]}`];
  }
  return undefined;
}

/**
 * One symbol, falling back to its literal `{W}` if the image will not load
 * (a stale table, or Scryfall's CDN being unreachable). The fallback is a
 * signal rather than DOM surgery on the <img>: replacing the node by hand
 * detaches something Solid still owns, and the orphaned text then survives
 * every later update of the surrounding text.
 */
function Symbol(props: { body: string; info: SymbolInfo }) {
  const [failed, setFailed] = createSignal(false);
  const literal = () => `{${props.body}}`;
  // <Show>, not a ternary: a component body runs once, so a ternary there
  // would be evaluated before the image ever had a chance to fail.
  return (
    <Show when={failed()} fallback={
      <img
        class={props.info.flat ? "mana mana-flat" : "mana"}
        src={`${SYMBOL_CDN}/${props.info.file}.svg`}
        // `alt` is what a screen reader announces, so it gets the English
        // Scryfall supplies ("one white mana"); the machine form is the tooltip.
        alt={props.info.english}
        title={literal()}
        decoding="async"
        referrerpolicy="no-referrer"
        onError={() => setFailed(true)}
      />
    }>
      {literal()}
    </Show>
  );
}

/** `text` with its card symbols drawn. Safe for untrusted text: every piece
 * is rendered as a JSX child or an attribute, never as HTML. */
export default function Symbols(props: { text: string }) {
  // A plain map, not <For>: the pieces are wholly derived from `props.text`
  // and none of them is independently reactive, so a keyed list would rebuild
  // every piece on each change and pay for a reactive node per symbol.
  return (
    <>
      {split(props.text).map((piece) =>
        piece.kind === "symbol" ? (
          <Symbol body={piece.body} info={piece.info} />
        ) : (
          piece.text
        ),
      )}
    </>
  );
}
