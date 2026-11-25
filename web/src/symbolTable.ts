// Magic's card symbols, from `GET https://api.scryfall.com/symbology`
// (fetched 2026-08-31). Regenerate by re-running that request: `symbol` with
// its braces stripped is the key, the last path segment of `svg_uri` is the
// file, and `english` is the alt text.
//
// The Discord side derives its emoji names from the same bodies
// (crates/core/src/symbol.rs), so the two surfaces show the same set.

/** One card symbol: the Scryfall SVG basename, and what to read aloud. */
export interface SymbolInfo {
  file: string;
  english: string;
  /**
   * The SVG is drawn in flat black with no coloured disc behind it, so it
   * disappears against a dark background and has to be inverted there.
   * Checked against the live SVGs' fill attributes on 2026-08-31.
   */
  flat?: true;
}

/**
 * Keyed by the symbol body exactly as Scryfall writes it. Indexing may miss,
 * hence `| undefined`: the text these are looked up from is written by an LLM.
 */
export const SYMBOLS: Readonly<Record<string, SymbolInfo | undefined>> = {
  "T": { file: "T", english: "tap this permanent" },
  "Q": { file: "Q", english: "untap this permanent" },
  "E": { file: "E", english: "an energy counter", flat: true },
  "P": { file: "P", english: "modal budget pawprint", flat: true },
  "PW": { file: "PW", english: "planeswalker", flat: true },
  "CHAOS": { file: "CHAOS", english: "chaos", flat: true },
  "A": { file: "A", english: "an acorn counter" },
  "TK": { file: "TK", english: "a ticket counter", flat: true },
  "X": { file: "X", english: "X generic mana" },
  "Y": { file: "Y", english: "Y generic mana" },
  "Z": { file: "Z", english: "Z generic mana" },
  "0": { file: "0", english: "zero mana" },
  "\u00bd": { file: "HALF", english: "one-half generic mana" },
  "1": { file: "1", english: "one generic mana" },
  "2": { file: "2", english: "two generic mana" },
  "3": { file: "3", english: "three generic mana" },
  "4": { file: "4", english: "four generic mana" },
  "5": { file: "5", english: "five generic mana" },
  "6": { file: "6", english: "six generic mana" },
  "7": { file: "7", english: "seven generic mana" },
  "8": { file: "8", english: "eight generic mana" },
  "9": { file: "9", english: "nine generic mana" },
  "10": { file: "10", english: "ten generic mana" },
  "11": { file: "11", english: "eleven generic mana" },
  "12": { file: "12", english: "twelve generic mana" },
  "13": { file: "13", english: "thirteen generic mana" },
  "14": { file: "14", english: "fourteen generic mana" },
  "15": { file: "15", english: "fifteen generic mana" },
  "16": { file: "16", english: "sixteen generic mana" },
  "17": { file: "17", english: "seventeen generic mana" },
  "18": { file: "18", english: "eighteen generic mana" },
  "19": { file: "19", english: "nineteen generic mana" },
  "20": { file: "20", english: "twenty generic mana" },
  "100": { file: "100", english: "one hundred generic mana" },
  "1000000": { file: "1000000", english: "one million generic mana" },
  "\u221e": { file: "INFINITY", english: "infinite generic mana" },
  "W/U": { file: "WU", english: "one white or blue mana" },
  "W/B": { file: "WB", english: "one white or black mana" },
  "B/R": { file: "BR", english: "one black or red mana" },
  "B/G": { file: "BG", english: "one black or green mana" },
  "U/B": { file: "UB", english: "one blue or black mana" },
  "U/R": { file: "UR", english: "one blue or red mana" },
  "R/G": { file: "RG", english: "one red or green mana" },
  "R/W": { file: "RW", english: "one red or white mana" },
  "G/W": { file: "GW", english: "one green or white mana" },
  "G/U": { file: "GU", english: "one green or blue mana" },
  "B/G/P": { file: "BGP", english: "one black mana, one green mana, or 2 life" },
  "B/R/P": { file: "BRP", english: "one black mana, one red mana, or 2 life" },
  "G/U/P": { file: "GUP", english: "one green mana, one blue mana, or 2 life" },
  "G/W/P": { file: "GWP", english: "one green mana, one white mana, or 2 life" },
  "R/G/P": { file: "RGP", english: "one red mana, one green mana, or 2 life" },
  "R/W/P": { file: "RWP", english: "one red mana, one white mana, or 2 life" },
  "U/B/P": { file: "UBP", english: "one blue mana, one black mana, or 2 life" },
  "U/R/P": { file: "URP", english: "one blue mana, one red mana, or 2 life" },
  "W/B/P": { file: "WBP", english: "one white mana, one black mana, or 2 life" },
  "W/U/P": { file: "WUP", english: "one white mana, one blue mana, or 2 life" },
  "C/W": { file: "CW", english: "one colorless mana or one white mana" },
  "C/U": { file: "CU", english: "one colorless mana or one blue mana" },
  "C/B": { file: "CB", english: "one colorless mana or one black mana" },
  "C/R": { file: "CR", english: "one colorless mana or one red mana" },
  "C/G": { file: "CG", english: "one colorless mana or one green mana" },
  "2/W": { file: "2W", english: "two generic mana or one white mana" },
  "2/U": { file: "2U", english: "two generic mana or one blue mana" },
  "2/B": { file: "2B", english: "two generic mana or one black mana" },
  "2/R": { file: "2R", english: "two generic mana or one red mana" },
  "2/G": { file: "2G", english: "two generic mana or one green mana" },
  "H": { file: "H", english: "one colored mana or two life" },
  "W/P": { file: "WP", english: "one white mana or two life" },
  "U/P": { file: "UP", english: "one blue mana or two life" },
  "B/P": { file: "BP", english: "one black mana or two life" },
  "R/P": { file: "RP", english: "one red mana or two life" },
  "G/P": { file: "GP", english: "one green mana or two life" },
  "C/P": { file: "CP", english: "one colorless mana or two life" },
  "HW": { file: "HW", english: "one-half white mana" },
  "HR": { file: "HR", english: "one-half red mana" },
  "W": { file: "W", english: "one white mana" },
  "U": { file: "U", english: "one blue mana" },
  "B": { file: "B", english: "one black mana" },
  "R": { file: "R", english: "one red mana" },
  "G": { file: "G", english: "one green mana" },
  "C": { file: "C", english: "one colorless mana" },
  "S": { file: "S", english: "one snow mana" },
  "L": { file: "L", english: "one mana from a legendary source", flat: true },
  "D": { file: "D", english: "one potential land drop", flat: true },
};
