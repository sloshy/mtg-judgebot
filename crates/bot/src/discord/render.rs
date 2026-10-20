//! Pure rendering: a validated verdict or a [`JudgeError`] → the text of a
//! Discord reply. No serenity types here, so every branch is unit-testable
//! and Discord's size limits are enforced by construction (`fit`).
//!
//! Anything that can carry Magic's card symbols — the answer body and the
//! citation quotes — goes through [`mana`], which turns `{W}` into the bot's
//! custom emoji and returns a [`Rendered`] whose tags truncation cannot split.

use std::fmt::Write as _;

use std::time::Duration;

use judge_core::{
    Ambiguous, Card, CardId, Citation, Context, DiscordOperator, EXAMPLE_PREFIX, Earlier,
    FeedbackCategory, Freshness, JudgeError, Locale, RuleChunk, RuleId, Ruling, Score, SourceOffer,
    Validated, Verdict, source,
};
use nonempty::NonEmpty;

use super::catalog::{self, License, Messages};
use super::cooldown::UserLimit;
use super::mana::{Rendered, SymbolTable};
use crate::feed::Feed;

/// Discord's limit on message `content`, in characters.
pub const CONTENT_LIMIT: usize = 2000;
/// Discord's limit on an embed description, in characters.
pub const EMBED_DESCRIPTION_LIMIT: usize = 4096;
/// Discord's limit on a button label, in characters.
pub const BUTTON_LABEL_LIMIT: usize = 80;
/// Most "did you mean…?" choices offered (one action row holds five buttons).
pub const MAX_CHOICES: usize = 5;
/// Appended to text that had to be cut to fit.
pub const TRUNCATION_MARKER: &str = " […]";
/// Longest quote shown per citation line, in source characters.
pub const QUOTE_LIMIT: usize = 240;
/// Most of [`EMBED_DESCRIPTION_LIMIT`] one citation line may take once its
/// symbols are emoji. As with [`HEADER_LIMIT`], the source-character cap does
/// not bound the sent width: 240 characters of `{T}` expand past half the
/// embed, and one Oracle-text quote would then crowd out every other citation.
/// A quarter of the budget leaves room for at least four lines.
pub const CITATION_LINE_LIMIT: usize = EMBED_DESCRIPTION_LIMIT / 4;
/// Longest question restated in the reply header, in source characters.
pub const QUESTION_LIMIT: usize = 300;
/// Most of [`CONTENT_LIMIT`] the restated question may take *after* its symbols
/// have become emoji. [`QUESTION_LIMIT`] alone cannot bound this: a tag is
/// about 29 characters where `{W}` is three, so 300 characters of symbols would
/// expand past the whole message budget and leave no room for the ruling.
pub const HEADER_LIMIT: usize = CONTENT_LIMIT / 4;
/// HTML mirror of the Comprehensive Rules with one anchor per rule
/// (verified 2026-08-30: ids look like `R70219b`, see [`rule_anchor`]).
pub const CR_MIRROR_URL: &str = "https://yawgatog.com/resources/magic-rules/";

/// `/help`, as the two messages it is sent in: how to use the bot and what
/// the data is ([`data`]); then what is stored, the public feed (unless
/// `feed` is off), and what `/license` says — where this instance's code is,
/// at which commit, under which licence, so the AGPL's network clause is met
/// on the bot's own surface, and who runs it. Two because together they pass
/// [`CONTENT_LIMIT`]; a test holds each under it at its longest. Ephemeral,
/// so neither clutters a channel.
#[must_use]
pub fn help(
    locale: Locale,
    offer: &SourceOffer,
    operator: &DiscordOperator,
    freshness: Option<&Freshness>,
    feed: Feed,
) -> [String; 2] {
    let m = catalog::messages(locale);
    let mut privacy = vec![m.help_stored];
    if feed.is_on() {
        privacy.push(m.help_feed);
    }
    [
        format!(
            "{}\n\n{}\n\n{}",
            m.help_body,
            m.help_disclaimer,
            data(locale, freshness)
        ),
        format!(
            "{}\n\n{}",
            privacy.join("\n\n"),
            license(locale, offer, operator)
        ),
    ]
}

/// What the data is, as `/help` lists it: the CR release and the last
/// refresh ([`Freshness::lines`]), or a line saying it could not be read.
#[must_use]
pub fn data(locale: Locale, freshness: Option<&Freshness>) -> String {
    let m = catalog::messages(locale);
    match freshness {
        Some(f) => {
            let mut out = m.data_heading.to_owned();
            for line in f.lines(locale) {
                let _ = write!(out, "\n- {line}");
            }
            out
        }
        None => m.data_unknown.to_owned(),
    }
}

/// Who runs this instance. The username is in a code span: `_` is legal in
/// one and would otherwise italicise. The support address follows when the
/// operator set one too.
#[must_use]
pub fn contact(locale: Locale, operator: &DiscordOperator) -> String {
    let email = operator.operator().email().map(ToString::to_string);
    (catalog::messages(locale).contact)(operator.username().as_ref(), email.as_deref())
}

/// `/license`: the source offer alone. Links are wrapped in `<…>` so Discord
/// does not unfurl them into embeds; the commit links into the repository
/// when the build knew it. [`contact`] follows it.
#[must_use]
pub fn license(locale: Locale, offer: &SourceOffer, operator: &DiscordOperator) -> String {
    let m = catalog::messages(locale);
    let revision = match offer.commit_url() {
        Some(url) => format!("[{}](<{url}>)", offer.revision(locale)),
        None => offer.revision(locale),
    };
    let repository = offer.repository().to_string();
    format!(
        "{} {}\n\n{}",
        m.license_heading,
        (m.license)(&License {
            program: source::PROGRAM,
            copyright: source::COPYRIGHT,
            license_name: source::LICENSE_NAME,
            license_url: source::LICENSE_URL,
            repository: &repository,
            revision: &revision,
        }),
        contact(locale, operator),
    )
}

/// A rendered answer: message content plus the embed holding the citations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    /// Message content: the answer text, at most [`CONTENT_LIMIT`] chars.
    pub content: String,
    /// Embed description: one `[702.19b] “…”` line per citation, at most
    /// [`EMBED_DESCRIPTION_LIMIT`] chars.
    pub citations: String,
    /// Embed footer: the resolved cards (if any), confidence and CR version.
    pub footer: String,
}

/// Render a validated verdict. The content restates the question (see
/// [`with_header`]); citation lines link into `ctx`'s cards where it has them.
/// Card symbols in the answer, in the restated question and in the quotes
/// become emoji from `symbols` — [`SymbolTable::empty`] leaves them as `{W}`.
#[must_use]
pub fn answer(
    locale: Locale,
    v: &Verdict<Validated>,
    ctx: Option<&Context>,
    asker: u64,
    question: &str,
    symbols: &SymbolTable,
) -> Answer {
    let lines: Vec<Rendered> = v
        .citations()
        .iter()
        .map(|c| citation_line(locale, c, ctx, symbols))
        .collect();
    Answer {
        content: with_header(locale, asker, question, v.answer(), symbols),
        citations: fit_lines(locale, &lines, EMBED_DESCRIPTION_LIMIT),
        footer: footer(locale, v),
    }
}

/// `<@asker> asked: <question>`, the question collapsed to one line and cut to
/// [`QUESTION_LIMIT`] chars. The `<@…>` mention renders in Discord; whether it
/// *pings* is decided by the message's allowed-mentions, not here.
#[must_use]
pub fn header(locale: Locale, asker: u64, question: &str) -> String {
    format!(
        "{PROMPT_MENTION}{asker}{PROMPT_CLOSE}{}{}",
        catalog::messages(locale).asked,
        fit(&collapse_whitespace(question), QUESTION_LIMIT)
    )
}

/// [`header`], a blank line, then `body`, with card symbols in both drawn as
/// emoji from `symbols`. The header is bounded first (to [`HEADER_LIMIT`]) and
/// the body gets whatever is left of [`CONTENT_LIMIT`], so an over-long body is
/// cut, never the header — *and* a question made entirely of symbols cannot
/// crowd the answer out of its own message. Every reply the bot sends composes
/// through here, so that budget rule has one definition. The one exception is
/// a [`PickPrompt`], which must restate the question verbatim and whose
/// question length `/judge` bounds instead.
#[must_use]
pub fn with_header(
    locale: Locale,
    asker: u64,
    question: &str,
    body: &str,
    symbols: &SymbolTable,
) -> String {
    let head = Rendered::substitute(&header(locale, asker, question), symbols).fit(HEADER_LIMIT);
    let room = CONTENT_LIMIT
        .saturating_sub(head.chars().count())
        .saturating_sub(SEPARATOR.chars().count());
    let body = Rendered::substitute(body, symbols).fit(room);
    let mut out = format!("{head}{SEPARATOR}{body}");
    let kept = out.trim_end().len();
    out.truncate(kept);
    out
}

/// Between the restated question and the answer.
const SEPARATOR: &str = "\n\n";

/// Anchor of a rule on [`CR_MIRROR_URL`]: `R` plus the id with its dots
/// removed. Observed on the live page 2026-08-30: `100.1` → `id=R1001`,
/// `702.19b` → `id=R70219b`, `613.1a` → `id=R6131a`, `702` → `id=R702`,
/// `704.5aa` → `id=R7045aa` (3318 such ids, one per rule).
#[must_use]
pub fn rule_anchor(id: &RuleId) -> String {
    format!("R{}", id.as_ref().replace('.', ""))
}

/// Deep link to a rule: [`CR_MIRROR_URL`] plus `#` and [`rule_anchor`].
#[must_use]
pub fn rule_url(id: &RuleId) -> String {
    format!("{CR_MIRROR_URL}#{}", rule_anchor(id))
}

/// Scryfall page for an oracle id. The `/card/<uuid>` form 404s; this search
/// form 303-redirects to the card's search result (verified 2026-08-30).
#[must_use]
pub fn scryfall_url(card: CardId) -> String {
    format!("https://scryfall.com/search?q=oracleid%3A{card}")
}

/// `Confidence: High · CR 2026-08-19`, under a `Cards: Dark Confidant · Blood
/// Moon` line when the question named any: the reader's check that a nickname
/// or a misspelling was taken to mean the card they had in mind. Names are
/// separated by ` · `, not commas, because names contain commas.
#[must_use]
pub fn footer(locale: Locale, v: &Verdict<Validated>) -> String {
    let m = catalog::messages(locale);
    let meta = (m.footer_meta)(v.confidence(), &cr_date(v.cr_version().as_ref()));
    match cards_line(m, v) {
        Some(cards) => format!("{cards}\n{meta}"),
        None => meta,
    }
}

/// Longest `Cards: …` footer line, in characters (Discord allows 2048 for the
/// whole footer; a question naming many cards should not make it a wall).
pub const CARDS_LINE_LIMIT: usize = 300;

/// `Cards: A · B`, or `None` when the question named no card.
fn cards_line(m: &Messages, v: &Verdict<Validated>) -> Option<String> {
    if v.cards().is_empty() {
        return None;
    }
    let names: Vec<&str> = v.cards().iter().map(|c| c.name.as_str()).collect();
    Some(fit(&(m.cards_line)(&names.join(" · ")), CARDS_LINE_LIMIT))
}

/// One compact citation line: `[702.19b](…rule url…) “quote”`. Rule and card
/// references are markdown links (embed descriptions render those; plain
/// message content does not, so these lines must stay in the embed). Card
/// names come from `ctx` when it holds the card; the link works either way,
/// since the URL only needs the oracle id the citation itself carries.
///
/// The quote is capped twice: [`QUOTE_LIMIT`] on the source text, which is
/// what "240 characters of quote" means to a reader, and
/// [`CITATION_LINE_LIMIT`] once its symbols are emoji, which is what Discord
/// counts. [`fit_lines`] then fits the lines to the embed.
#[must_use]
pub fn citation_line(
    locale: Locale,
    c: &Citation,
    ctx: Option<&Context>,
    symbols: &SymbolTable,
) -> Rendered {
    let m = catalog::messages(locale);
    let quote = fit(&collapse_whitespace(c.quote()), QUOTE_LIMIT);
    let card_name = |id: CardId| ctx.and_then(|x| x.card(id)).map(|card| card.name.clone());
    let open = m.quote_open;
    let prefix = match c {
        Citation::Rule { id, .. } => format!("[{id}]({}) {open}", rule_url(id)),
        Citation::ScryfallRuling { card, ruling, .. } => {
            // The date tells two rulings of one card apart; both link to the
            // same Scryfall page.
            let date = ctx
                .and_then(|x| x.ruling(*card, ruling))
                .map(|r| format!(" ({})", r.published_at))
                .unwrap_or_default();
            let label = (m.ruling_label)(&date, card_name(*card).as_deref());
            format!("[{label}]({}) {open}", scryfall_url(*card))
        }
        Citation::PriorCall { .. } => format!("[{}] {open}", m.prior_call_label),
        Citation::OracleText { card, face, .. } => {
            let face = match face {
                0 => None,
                n => Some(usize::try_from(*n).map_or(usize::MAX, |n| n.saturating_add(1))),
            };
            let label = (m.oracle_label)(face, card_name(*card).as_deref());
            format!("[{label}]({}) {open}", scryfall_url(*card))
        }
    };
    // The label and the URL are counted too, so CITATION_LINE_LIMIT bounds the
    // whole line rather than just the quote inside it.
    let room = CITATION_LINE_LIMIT
        .saturating_sub(prefix.chars().count())
        .saturating_sub(m.quote_close.chars().count());
    let mut line = Rendered::plain(prefix);
    line.append(Rendered::substitute(&quote, symbols).truncate(room));
    line.push_str(m.quote_close);
    line
}

/// The "did you mean…?" for the *first* ambiguous span when no buttons can
/// be offered ([`PickPrompt::new`] refused the question): the choices as a
/// list and how to ask again with one named.
#[must_use]
pub fn did_you_mean(locale: Locale, spans: &NonEmpty<Ambiguous>) -> String {
    let m = catalog::messages(locale);
    let first = spans.first();
    let mut content = (m.did_you_mean_head)(&display_span(&first.query));
    for (i, card) in first.candidates.iter().take(MAX_CHOICES).enumerate() {
        // Writing to a String cannot fail.
        let _ = write!(content, "\n{}. {}", i + 1, card.name);
    }
    let hidden = first.candidates.len().saturating_sub(MAX_CHOICES);
    if hidden > 0 {
        let _ = write!(content, "\n{}", (m.did_you_mean_more)(hidden));
    }
    let _ = write!(content, "\n{}", m.did_you_mean_tail);
    fit(&content, CONTENT_LIMIT)
}

/// A "did you mean…?" message whose content carries the whole question, so a
/// click on one of its buttons is answered from the message alone, by any
/// process, after any restart ([`super::ids::Pick`] carries the rest).
///
/// The content is `<@asker> asked: `, the question exactly as asked (not
/// collapsed, not cut, no emoji substituted), a blank line, then a body: a
/// lead line, the choices as a numbered list (the same cards, in the same
/// order, as the buttons), and up to two notes, each on a line of its own:
///
/// ```text
/// <@42> asked: does bob  die?
///
/// I'm not sure which card you mean by **bob**. Did you mean…?
/// 1. Dark Confidant
/// 2. Bob, the Burger
/// ```
///
/// Every body line is collapsed to one line ([`display_span`],
/// [`display_name`]) and none is empty, so the body holds no blank line and
/// the question is everything up to the *last* one, whatever the question
/// holds. [`Self::parse`] checks that by rendering what it read and
/// comparing: content it cannot reproduce exactly is refused, so a parse
/// never recovers a question the bot did not write. The fields are private,
/// so every value came from [`Self::new`] or [`Self::parse`] and fits the
/// message limit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PickPrompt {
    /// The language the prompt is written in, carried in its buttons' ids.
    locale: Locale,
    asker: u64,
    question: String,
    /// The ambiguous span as shown, in [`display_span`] form.
    span: String,
    /// The candidates listed, 1 to [`MAX_CHOICES`], in [`display_name`] form.
    choices: Vec<String>,
    /// More candidates than buttons.
    more: bool,
    /// Ambiguous spans after this one.
    later: usize,
}

const PROMPT_MENTION: &str = "<@";
const PROMPT_CLOSE: &str = ">";
/// Digits in the largest `u64` (a user id, a count).
const U64_DIGITS: usize = 20;
/// `\n5. `: a list line's newline and number ([`MAX_CHOICES`] has one digit).
const PICK_ITEM_HEAD: usize = "\n5. ".len();
const _: () = assert!(MAX_CHOICES < 10);

/// Longest ambiguous span a [`PickPrompt`] shows, in characters.
pub const PICK_SPAN_LIMIT: usize = 100;
/// Longest card name a [`PickPrompt`] lists, in characters. Most names are
/// far shorter; the few past it are cut with [`TRUNCATION_MARKER`], and the
/// button still carries the name (to [`BUTTON_LABEL_LIMIT`]).
pub const PICK_NAME_LIMIT: usize = 60;
/// Most characters a prompt in `m` spends before the question. Fixed text is
/// counted in bytes, which is never less than characters.
const fn prefix_max(m: &Messages) -> usize {
    PROMPT_MENTION.len() + U64_DIGITS + PROMPT_CLOSE.len() + m.asked.len()
}

/// Most characters a [`PickPrompt`] in `m` spends after the question, blank
/// line included.
const fn pick_body_max(m: &Messages) -> usize {
    SEPARATOR.len()
        + m.pick_head.len()
        + PICK_SPAN_LIMIT
        + m.pick_ask.len()
        + MAX_CHOICES * (PICK_ITEM_HEAD + PICK_NAME_LIMIT)
        + 1
        + m.pick_more.len()
        + 1
        + max(
            m.pick_later_one.len(),
            m.pick_later_head.len() + U64_DIGITS + m.pick_later_tail.len(),
        )
}

const fn max(a: usize, b: usize) -> usize {
    if a > b { a } else { b }
}

/// The longest question a [`PickPrompt`] in `m` always holds.
const fn pick_question_limit(m: &Messages) -> usize {
    CONTENT_LIMIT - prefix_max(m) - pick_body_max(m)
}

/// The longest question a [`RetryPrompt`] in `m` always holds.
const fn retry_question_limit(m: &Messages) -> usize {
    CONTENT_LIMIT - prefix_max(m) - SEPARATOR.len() - m.retry_body.len()
}

/// The least over every locale's catalog of the pick (`retry` false) or
/// retry question limit. (A `const fn` cannot call through a function
/// pointer, hence the flag.)
const fn least(retry: bool) -> usize {
    let mut least = usize::MAX;
    let mut rest: &[Locale] = &Locale::ALL;
    while let [locale, tail @ ..] = rest {
        let m = catalog::messages(*locale);
        let n = if retry {
            retry_question_limit(m)
        } else {
            pick_question_limit(m)
        };
        if n < least {
            least = n;
        }
        rest = tail;
    }
    least
}

/// The longest question, in characters, that a [`PickPrompt`] in any locale
/// always holds within [`CONTENT_LIMIT`]: `/judge`'s `max_length` is held to
/// it.
pub const PICK_QUESTION_LIMIT: usize = least(false);

/// A span as a [`PickPrompt`] shows it: whitespace collapsed to single
/// spaces (so never a line break) and cut to [`PICK_SPAN_LIMIT`].
#[must_use]
pub fn display_span(span: &str) -> String {
    fit(&collapse_whitespace(span), PICK_SPAN_LIMIT)
}

/// A card name as a [`PickPrompt`] lists it: whitespace collapsed and cut to
/// [`PICK_NAME_LIMIT`].
#[must_use]
pub fn display_name(name: &str) -> String {
    fit(&collapse_whitespace(name), PICK_NAME_LIMIT)
}

impl PickPrompt {
    /// The prompt for `spans`' first span, or `None` when the message would
    /// not fit [`CONTENT_LIMIT`] (counted as characters and as UTF-16 units,
    /// whichever Discord counts). A question within [`PICK_QUESTION_LIMIT`]
    /// characters fits unless it is heavy in characters outside the Basic
    /// Multilingual Plane; the caller then offers no buttons
    /// ([`did_you_mean`]) rather than cut the question.
    #[must_use]
    pub fn new(
        locale: Locale,
        asker: u64,
        question: &str,
        spans: &NonEmpty<Ambiguous>,
    ) -> Option<Self> {
        let first = spans.first();
        let p = Self {
            locale,
            asker,
            question: question.to_owned(),
            span: display_span(&first.query),
            choices: first
                .candidates
                .iter()
                .take(MAX_CHOICES)
                .map(|c| display_name(&c.name))
                .collect(),
            more: first.candidates.len() > MAX_CHOICES,
            later: spans.len().saturating_sub(1),
        };
        p.fits().then_some(p)
    }

    /// The asker's Discord user id.
    #[must_use]
    pub const fn asker(&self) -> u64 {
        self.asker
    }

    /// The language it is written in.
    #[must_use]
    pub const fn locale(&self) -> Locale {
        self.locale
    }

    /// The question, byte for byte as asked.
    #[must_use]
    pub fn question(&self) -> &str {
        &self.question
    }

    /// Whether the list offers the card named `name` (compared as listed,
    /// [`display_name`]): a pick must be for a card its prompt offered.
    #[must_use]
    pub fn lists(&self, name: &str) -> bool {
        let shown = display_name(name);
        self.choices.contains(&shown)
    }

    /// The message content.
    #[must_use]
    pub fn render(&self) -> String {
        let m = catalog::messages(self.locale);
        let mut out = format!(
            "{PROMPT_MENTION}{}{PROMPT_CLOSE}{}{}{SEPARATOR}{}{}{}",
            self.asker, m.asked, self.question, m.pick_head, self.span, m.pick_ask
        );
        for (i, name) in self.choices.iter().enumerate() {
            // Writing to a String cannot fail.
            let _ = write!(out, "\n{}. {name}", i + 1);
        }
        if self.more {
            let _ = write!(out, "\n{}", m.pick_more);
        }
        match self.later {
            0 => {}
            1 => {
                let _ = write!(out, "\n{}", m.pick_later_one);
            }
            n => {
                let _ = write!(out, "\n{}{n}{}", m.pick_later_head, m.pick_later_tail);
            }
        }
        out
    }

    fn fits(&self) -> bool {
        let c = self.render();
        c.chars().count() <= CONTENT_LIMIT && c.encode_utf16().count() <= CONTENT_LIMIT
    }

    /// Read a prompt in `locale` (its buttons say which) back from a
    /// message's content: `None` unless rendering what was read reproduces
    /// `content` exactly (and it fits the limit).
    #[must_use]
    pub fn parse(locale: Locale, content: &str) -> Option<Self> {
        let m = catalog::messages(locale);
        let rest = content.strip_prefix(PROMPT_MENTION)?;
        let (asker, rest) = rest.split_once(PROMPT_CLOSE)?;
        let rest = rest.strip_prefix(m.asked)?;
        if asker.is_empty() || !asker.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let asker: u64 = asker.parse().ok()?;
        let (question, body) = rest.rsplit_once(SEPARATOR)?;
        let mut lines = body.split('\n').peekable();
        let span = lines
            .next()?
            .strip_prefix(m.pick_head)?
            .strip_suffix(m.pick_ask)?;
        let mut choices = Vec::new();
        while let Some(name) = lines.peek().and_then(|l| {
            let number = choices.len().saturating_add(1);
            (number <= MAX_CHOICES)
                .then(|| l.strip_prefix(&format!("{number}. ")))
                .flatten()
        }) {
            choices.push(name.to_owned());
            lines.next();
        }
        let more = lines.next_if_eq(&m.pick_more).is_some();
        let later = match lines.next() {
            None => 0,
            Some(t) if t == m.pick_later_one => 1,
            Some(t) => {
                let n = t
                    .strip_prefix(m.pick_later_head)?
                    .strip_suffix(m.pick_later_tail)?;
                if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                n.parse().ok()?
            }
        };
        if lines.next().is_some() || choices.is_empty() {
            return None;
        }
        let p = Self {
            locale,
            asker,
            question: question.to_owned(),
            span: span.to_owned(),
            choices,
            more,
            later,
        };
        let displayed =
            p.span == display_span(&p.span) && p.choices.iter().all(|c| *c == display_name(c));
        (displayed && p.fits() && p.render() == content).then_some(p)
    }
}

/// The longest question, in characters, that a [`RetryPrompt`] in any
/// locale always holds within [`CONTENT_LIMIT`].
pub const RETRY_QUESTION_LIMIT: usize = least(true);

/// A `/judge` answered with the maintenance reply, carrying the whole
/// question so its Retry button ([`super::ids::Retry`]) asks it again from
/// the message alone, like a "did you mean…?" ([`PickPrompt`]):
///
/// ```text
/// <@42> asked: does bob  die?
///
/// Judgebot is being upgraded and will be back in a minute or two. Press **Retry** to ask this again.
/// ```
///
/// [`Self::parse`] renders what it read and compares, so a parse never
/// recovers a question the bot did not write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetryPrompt {
    /// The language it is written in, carried in its button's id.
    locale: Locale,
    asker: u64,
    question: String,
}

impl RetryPrompt {
    /// The reply for `question`, or `None` when it would not fit
    /// [`CONTENT_LIMIT`] (the caller then sends the maintenance reply alone).
    #[must_use]
    pub fn new(locale: Locale, asker: u64, question: &str) -> Option<Self> {
        let p = Self {
            locale,
            asker,
            question: question.to_owned(),
        };
        p.fits().then_some(p)
    }

    /// The asker's Discord user id.
    #[must_use]
    pub const fn asker(&self) -> u64 {
        self.asker
    }

    /// The question, byte for byte as asked.
    #[must_use]
    pub fn question(&self) -> &str {
        &self.question
    }

    /// The message content.
    #[must_use]
    pub fn render(&self) -> String {
        let m = catalog::messages(self.locale);
        format!(
            "{PROMPT_MENTION}{}{PROMPT_CLOSE}{}{}{SEPARATOR}{}",
            self.asker, m.asked, self.question, m.retry_body
        )
    }

    /// The language it is written in.
    #[must_use]
    pub const fn locale(&self) -> Locale {
        self.locale
    }

    fn fits(&self) -> bool {
        let c = self.render();
        c.chars().count() <= CONTENT_LIMIT && c.encode_utf16().count() <= CONTENT_LIMIT
    }

    /// Read a reply in `locale` back from a message's content: `None`
    /// unless rendering what was read reproduces `content` exactly.
    #[must_use]
    pub fn parse(locale: Locale, content: &str) -> Option<Self> {
        let m = catalog::messages(locale);
        let rest = content.strip_prefix(PROMPT_MENTION)?;
        let (asker, rest) = rest.split_once(PROMPT_CLOSE)?;
        let rest = rest.strip_prefix(m.asked)?;
        if asker.is_empty() || !asker.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let asker: u64 = asker.parse().ok()?;
        let question = rest.strip_suffix(m.retry_body)?.strip_suffix(SEPARATOR)?;
        let p = Self {
            locale,
            asker,
            question: question.to_owned(),
        };
        (p.fits() && p.render() == content).then_some(p)
    }
}

/// The message for a failed `judge()`. Exhaustive over [`JudgeError`], so a
/// new variant must be rendered before this compiles. `AmbiguousCards` gives
/// the buttonless "did you mean…?" ([`did_you_mean`]); the Discord adapter
/// offers buttons through [`PickPrompt`] instead whenever it can.
#[must_use]
pub fn error(locale: Locale, e: &JudgeError) -> String {
    let m = catalog::messages(locale);
    match e {
        JudgeError::AmbiguousCards(spans) => did_you_mean(locale, spans),
        JudgeError::CardsNotFound(names) => {
            let listed: Vec<String> = names.iter().map(|n| format!("**{n}**")).collect();
            fit(&(m.cards_not_found)(&listed.join(m.or)), CONTENT_LIMIT)
        }
        JudgeError::OutOfScope(_) => m.out_of_scope.to_owned(),
        JudgeError::Upstream(err) if is_spend_cap(err) => m.spend_cap.to_owned(),
        JudgeError::BadCitation(_)
        | JudgeError::MalformedCitation(_)
        | JudgeError::EmptyVerdict(_)
        | JudgeError::UncitedRules(_)
        | JudgeError::ToolMisuse(_)
        | JudgeError::LlmRefused
        | JudgeError::Upstream(_) => m.failed.to_owned(),
    }
}

fn is_spend_cap(err: &anyhow::Error) -> bool {
    err.downcast_ref::<judge_llm::LlmError>()
        .is_some_and(|c| matches!(c, judge_llm::LlmError::SpendCapExceeded { .. }))
}

/// Label of a rating button.
#[must_use]
pub fn rating_label(locale: Locale, score: Score) -> &'static str {
    (catalog::messages(locale).rating_label)(score)
}

/// Ephemeral acknowledgement of a rating, naming its reason's category
/// when it has one, and saying the reason is public when `public`
/// (`JUDGE_PUBLIC_FEEDBACK`). An *Incorrect* one also names who reads the reasons,
/// and, when the instance's source is on GitHub, links that repository's
/// wrong-ruling issue form.
#[must_use]
pub fn rated(
    locale: Locale,
    score: Score,
    category: Option<FeedbackCategory>,
    is_judge: bool,
    offer: &SourceOffer,
    operator: &DiscordOperator,
    public: bool,
) -> String {
    let m = catalog::messages(locale);
    let rated = if public && category.is_some() {
        m.rated_public
    } else {
        m.rated
    };
    let mut out = rated(
        rating_label(locale, score),
        category.map(m.category_label),
        is_judge,
    );
    if matches!(score, Score::Incorrect) {
        let _ = write!(
            out,
            "\n\n{}",
            (m.report_wrong)(operator.username().as_ref())
        );
        if let Some(url) = report_url(offer) {
            out.push_str(&(m.report_link)(&url));
        }
    }
    out
}

/// "You already rated this…": the rater's `earlier` rating and the `new`
/// one they clicked, above Replace and Keep.
#[must_use]
pub fn replace_prompt(locale: Locale, earlier: Earlier, new: Score) -> String {
    let m = catalog::messages(locale);
    (m.replace_prompt)(
        rating_label(locale, earlier.score),
        earlier.category.map(m.category_label),
        rating_label(locale, new),
    )
}

/// After Keep: the rating the rater has on record now.
#[must_use]
pub fn on_record(locale: Locale, now: Earlier) -> String {
    let m = catalog::messages(locale);
    (m.on_record)(
        rating_label(locale, now.score),
        now.category.map(m.category_label),
    )
}

/// The wrong-ruling issue form of the repository this instance's source is
/// in, when that is a GitHub repository (the form is a GitHub feature, and a
/// fork carries the template with the rest of the tree).
#[must_use]
pub fn report_url(offer: &SourceOffer) -> Option<String> {
    let repo = offer.repository().to_string();
    let repo = repo.trim_end_matches('/');
    let path = repo.strip_prefix("https://github.com/")?;
    // `owner/name` and nothing else: not a file, a tree or a query.
    let mut parts = path.split('/');
    let (Some(owner), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
        return None;
    };
    let name = name.strip_suffix(".git").unwrap_or(name);
    // GitHub's own alphabet for both, which also keeps the masked link whole.
    let plain = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    if !plain(owner) || !plain(name) {
        return None;
    }
    Some(format!(
        "https://github.com/{owner}/{name}/issues/new?template=wrong_answer.yml"
    ))
}

/// Refusal of a `/judge` past the per-user allowance.
#[must_use]
pub fn cooling_down(locale: Locale, limit: UserLimit, wait: Duration) -> String {
    (catalog::messages(locale).cooling_down)(limit.max.get(), limit.window, wait)
}

/// Longest `/card` name accepted, in characters: a bound on what reaches the
/// resolver's trigram query, far above any real card name.
pub const LOOKUP_INPUT_LIMIT: usize = 200;
/// Discord's limit on an embed title, in characters.
pub const EMBED_TITLE_LIMIT: usize = 256;
/// An embed for a lookup: a linked title over a description.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lookup {
    /// Embed title, at most [`EMBED_TITLE_LIMIT`].
    pub title: String,
    /// Where the title links.
    pub url: String,
    /// Embed description, at most [`EMBED_DESCRIPTION_LIMIT`].
    pub description: String,
    /// Embed footer, if any.
    pub footer: Option<String>,
}

/// `/card`: every face's cost, type line and Oracle text, then the rulings,
/// newest first, as many whole ones as fit.
#[must_use]
pub fn card(locale: Locale, card: &Card, rulings: &[Ruling], symbols: &SymbolTable) -> Lookup {
    let m = catalog::messages(locale);
    let mut lines: Vec<Rendered> = Vec::new();
    let many = card.faces.len() > 1;
    for face in &card.faces {
        let mut head = Rendered::default();
        if many {
            head.push_str(&format!("**{}**", face.name));
        }
        if !face.mana_cost.is_empty() {
            if many {
                head.push_str("  ");
            }
            head.append(Rendered::substitute(&face.mana_cost, symbols));
        }
        if !head.is_empty() {
            lines.push(head);
        }
        lines.push(Rendered::plain(format!("*{}*", face.type_line)));
        if !face.oracle_text.is_empty() {
            lines.push(Rendered::substitute(&face.oracle_text, symbols));
        }
        lines.push(Rendered::default());
    }
    if rulings.is_empty() {
        lines.push(Rendered::plain(m.no_rulings));
    } else {
        lines.push(Rendered::plain((m.rulings_heading)(rulings.len())));
        for r in rulings {
            let mut line = Rendered::plain(format!("`{}` ", r.published_at));
            line.append(Rendered::substitute(&collapse_whitespace(&r.text), symbols));
            lines.push(line);
        }
    }
    Lookup {
        title: fit(&card.name, EMBED_TITLE_LIMIT),
        url: scryfall_url(card.id),
        description: fit_lines(locale, &lines, EMBED_DESCRIPTION_LIMIT),
        footer: None,
    }
}

/// `/card` when the name could mean several cards.
#[must_use]
pub fn card_ambiguous(locale: Locale, name: &str, candidates: &[&str]) -> String {
    let m = catalog::messages(locale);
    let mut out = (m.card_ambiguous_head)(&fit(name, QUESTION_LIMIT));
    for c in candidates.iter().take(MAX_CHOICES * 2) {
        let _ = write!(out, "\n- {c}");
    }
    let _ = write!(out, "\n{}", m.card_ambiguous_tail);
    fit(&out, CONTENT_LIMIT)
}

/// `/card` when nothing matched.
#[must_use]
pub fn card_not_found(locale: Locale, name: &str) -> String {
    fit(
        &(catalog::messages(locale).card_not_found)(&fit(name, QUESTION_LIMIT)),
        CONTENT_LIMIT,
    )
}

/// `/rule`: the chunks a rule number names, each with its examples quoted
/// under the line they belong to, as many whole ones as fit. `None` when the
/// number names nothing in the loaded CR.
#[must_use]
pub fn rules(
    locale: Locale,
    asked: &RuleId,
    chunks: &[RuleChunk],
    symbols: &SymbolTable,
) -> Option<Lookup> {
    let m = catalog::messages(locale);
    let first = chunks.first()?;
    let mut lines: Vec<Rendered> = Vec::new();
    for c in chunks {
        let mut line = Rendered::default();
        for (i, l) in c.text().trim().lines().enumerate() {
            if l.starts_with(EXAMPLE_PREFIX) {
                line.push_str("\n> ");
                line.append(Rendered::substitute(&collapse_whitespace(l), symbols));
            } else {
                if i > 0 {
                    line.push_str("\n");
                }
                line.append(Rendered::substitute(l, symbols));
            }
        }
        lines.push(line);
    }
    let heading = (m.rule_title)(
        asked.as_ref(),
        Some(first.heading.as_str()).filter(|h| !h.is_empty()),
    );
    // A few rules (205.3, 608.2) are longer than an embed on their own. Cut,
    // they would stop mid-sentence with nothing to say why, so say where the
    // rest is.
    let budget = EMBED_DESCRIPTION_LIMIT.saturating_sub(m.long_rule_note.chars().count());
    let cut = lines.first().is_some_and(|l| l.len() > budget);
    let mut description = fit_lines(locale, &lines, budget);
    if cut {
        description.push_str(m.long_rule_note);
    }
    Some(Lookup {
        title: fit(&heading, EMBED_TITLE_LIMIT),
        url: rule_url(asked),
        description,
        footer: Some((m.cr_label)(&cr_date(first.cr_version.as_ref()))),
    })
}

/// `/rule` when the number names nothing.
#[must_use]
pub fn rule_not_found(locale: Locale, id: &RuleId) -> String {
    (catalog::messages(locale).rule_not_found)(id.as_ref())
}

/// Shown on the "did you mean…?" message while the pick is being judged.
#[must_use]
pub fn working(locale: Locale, card: &str) -> String {
    fit(&(catalog::messages(locale).working)(card), CONTENT_LIMIT)
}

/// A button label: the card name, cut to [`BUTTON_LABEL_LIMIT`] chars.
#[must_use]
pub fn button_label(name: &str) -> String {
    let name = name.trim();
    if name.chars().count() <= BUTTON_LABEL_LIMIT {
        return name.to_owned();
    }
    let mut s: String = name.chars().take(BUTTON_LABEL_LIMIT - 1).collect();
    s.push('…');
    s
}

/// `text`, cut to at most `limit` chars with [`TRUNCATION_MARKER`] at the end
/// if anything was dropped. Never panics; a `limit` smaller than the marker
/// simply hard-cuts.
#[must_use]
pub fn fit(text: &str, limit: usize) -> String {
    let text = text.trim_end();
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let marker = TRUNCATION_MARKER.chars().count();
    if limit < marker {
        return text.chars().take(limit).collect();
    }
    let mut out: String = text.chars().take(limit - marker).collect();
    let kept = out.trim_end().len();
    out.truncate(kept);
    out.push_str(TRUNCATION_MARKER);
    out
}

/// The least held back for the "… and N more" note while lines remain;
/// [`fit_lines`] holds back more when the locale's note is longer.
const NOTE_RESERVE: usize = 24;

/// Join `lines` with newlines, dropping whole trailing lines (rather than
/// cutting one in half) so the result fits `limit`; says how many were dropped.
/// Lengths are counted as Discord counts them, so an emoji tag costs its full
/// `<:mana_w:123…>` width here even though it shows as one symbol.
#[must_use]
pub fn fit_lines(locale: Locale, lines: &[Rendered], limit: usize) -> String {
    let more = catalog::messages(locale).more_lines;
    // At most every line is dropped, and the note never shrinks as the
    // count grows, so this holds room for whatever count it ends up saying.
    let note_reserve = NOTE_RESERVE.max(more(lines.len()).chars().count().saturating_add(1));
    let mut out = Rendered::default();
    let mut shown = 0usize;
    for line in lines {
        let candidate_len = out
            .len()
            .saturating_add(usize::from(!out.is_empty()))
            .saturating_add(line.len());
        // Leave room for the "… and N more" note if this is not the last line.
        let reserve = if shown.saturating_add(1) < lines.len() {
            note_reserve
        } else {
            0
        };
        if candidate_len.saturating_add(reserve) > limit {
            break;
        }
        if !out.is_empty() {
            out.push_str("\n");
        }
        out.append(line.clone());
        shown = shown.saturating_add(1);
    }
    let dropped = lines.len().saturating_sub(shown);
    if dropped > 0 {
        if out.is_empty() {
            // Even the first line did not fit: cut it rather than show nothing.
            return lines.first().map(|l| l.fit(limit)).unwrap_or_default();
        }
        out.push_str("\n");
        out.push_str(&more(dropped));
    }
    out.fit(limit)
}

/// `20260819` → `2026-08-19` (the `CrVersion` invariant guarantees eight ASCII digits).
fn cr_date(v: &str) -> String {
    match (v.get(0..4), v.get(4..6), v.get(6..8)) {
        (Some(y), Some(m), Some(d)) if v.len() == 8 => format!("{y}-{m}-{d}"),
        _ => v.to_owned(),
    }
}

fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use judge_core::Confidence;

    const FAILED: &str = catalog::EN.failed;
    const OUT_OF_SCOPE: &str = catalog::EN.out_of_scope;
    const SPEND_CAP: &str = catalog::EN.spend_cap;
    const PICK_LATER_ONE: &str = catalog::EN.pick_later_one;
    const PICK_MORE: &str = catalog::EN.pick_more;
    use judge_core::{
        AnswerableSource, Card, CardId, Category, Context, CrVersion, EmptyVerdict, Face, Layout,
        RuleChunk, RuleId, Source, ruling_key,
    };

    /// Most tests predate the emoji and assert the literal `{W}` behaviour.
    fn no_symbols() -> SymbolTable {
        SymbolTable::empty()
    }
    use uuid::Uuid;

    type Res = Result<(), Box<dyn std::error::Error>>;

    fn rule(body: &str) -> Result<RuleChunk, Box<dyn std::error::Error>> {
        Ok(RuleChunk {
            id: RuleId::try_new("702.19b".to_owned())?,
            parent_id: None,
            subsection: RuleId::try_new("702".to_owned())?,
            heading: "Trample".into(),
            body: body.into(),
            examples: vec![],
            cr_version: CrVersion::try_new("20260819".to_owned())?,
        })
    }

    fn validated(
        answer: &str,
        n_citations: usize,
    ) -> Result<Verdict<Validated>, Box<dyn std::error::Error>> {
        let body = "The controller of an attacking creature with trample first assigns the combat damage to the \
                    creature(s) blocking it.";
        let ctx = Context {
            rules: vec![rule(body)?],
            ..Context::default()
        };
        let id = RuleId::try_new("702.19b".to_owned())?;
        let citations = vec![
            Citation::Rule {
                id,
                quote: judge_core::Quote::try_new(body)?
            };
            n_citations
        ];
        let v = Verdict::new(answer.into(), Confidence::High, citations, Category::Combat);
        Ok(v.validate(&ctx, AnswerableSource::Cr)?)
    }

    #[test]
    fn footer_names_the_resolved_cards_above_the_confidence() -> Res {
        let body = "The controller of an attacking creature with trample first assigns the combat damage to the \
                    creature(s) blocking it.";
        let ctx = Context {
            rules: vec![rule(body)?],
            cards: vec![card(1, "Ragavan, Nimble Pilferer"), card(2, "Blood Moon")],
            ..Context::default()
        };
        let v = Verdict::new(
            LONG_ENOUGH.into(),
            Confidence::High,
            vec![Citation::Rule {
                id: RuleId::try_new("702.19b".to_owned())?,
                quote: judge_core::Quote::try_new(body)?,
            }],
            Category::Combat,
        )
        .validate(&ctx, AnswerableSource::Cr)?;
        assert_eq!(
            footer(Locale::En, &v),
            "Cards: Ragavan, Nimble Pilferer · Blood Moon\nConfidence: High · CR 2026-08-19"
        );
        let many = Context {
            cards: (0..40)
                .map(|n| card(n, "Emeritus of Conflict // Lightning Bolt"))
                .collect(),
            ..ctx
        };
        let v = Verdict::new(
            LONG_ENOUGH.into(),
            Confidence::High,
            vec![Citation::Rule {
                id: RuleId::try_new("702.19b".to_owned())?,
                quote: judge_core::Quote::try_new(body)?,
            }],
            Category::Combat,
        )
        .validate(&many, AnswerableSource::Cr)?;
        let f = footer(Locale::En, &v);
        let first = f.lines().next().unwrap_or_default();
        assert_eq!(first.chars().count(), CARDS_LINE_LIMIT, "{f}");
        assert!(f.ends_with("\nConfidence: High · CR 2026-08-19"), "{f}");
        Ok(())
    }

    fn card(n: u128, name: &str) -> Card {
        Card {
            id: CardId::new(Uuid::from_u128(n)),
            name: name.into(),
            layout: Layout::Normal,
            faces: NonEmpty::new(Face {
                name: name.into(),
                oracle_text: String::new(),
                mana_cost: String::new(),
                type_line: "Land".into(),
            }),
        }
    }

    const LONG_ENOUGH: &str =
        "Trample assigns lethal damage to blockers first, and the rest to the player.";

    #[test]
    fn fit_keeps_short_text_and_marks_cut_text() {
        assert_eq!(fit("hello  ", 10), "hello");
        let long = "x".repeat(50);
        let cut = fit(&long, 20);
        assert_eq!(cut.chars().count(), 20);
        assert!(cut.ends_with(TRUNCATION_MARKER));
        // Multi-byte chars count as one each and never split.
        let uni = "é".repeat(30);
        let cut = fit(&uni, 10);
        assert_eq!(cut.chars().count(), 10);
        assert!(cut.ends_with(TRUNCATION_MARKER));
        // A limit smaller than the marker hard-cuts instead of overflowing.
        assert_eq!(fit("abcdef", 2), "ab");
        assert_eq!(fit("abcdef", 0), "");
    }

    const ASKER: u64 = 1_000_000_000_000_000_042;
    /// Real emoji ids are 19-digit snowflakes; a short one would understate
    /// how much of the budget a tag costs (`<:mana_t:1…>` is 29 characters).
    const TAP_ID: u64 = 1_411_688_015_155_265_557;
    const GREEN_ID: u64 = 1_411_688_015_155_265_558;

    #[test]
    fn header_mentions_the_asker_and_truncates_the_question() {
        let h = header(Locale::En, ASKER, "Does  trample\nwork here?");
        assert_eq!(h, "<@1000000000000000042> asked: Does trample work here?");
        let long = "why ".repeat(200);
        let h = header(Locale::En, ASKER, &long);
        assert!(h.starts_with("<@1000000000000000042> asked: why why"));
        assert!(h.ends_with(TRUNCATION_MARKER), "{h}");
        assert!(
            h.chars().count() <= QUESTION_LIMIT + "<@1000000000000000042> asked: ".len(),
            "{h}"
        );
        assert_eq!(h.lines().count(), 1, "the header is a single line");
    }

    #[test]
    fn with_header_prefixes_and_keeps_the_content_budget() {
        let c = with_header(Locale::En, ASKER, "short?", "The answer.", &no_symbols());
        assert_eq!(c, "<@1000000000000000042> asked: short?\n\nThe answer.");
        // An over-long body is cut from the end; the header survives intact.
        let c = with_header(
            Locale::En,
            ASKER,
            &"q ".repeat(400),
            &"body ".repeat(1000),
            &no_symbols(),
        );
        assert!(c.chars().count() <= CONTENT_LIMIT, "{}", c.len());
        assert!(c.starts_with("<@1000000000000000042> asked: q q"));
        assert!(c.contains("\n\nbody"));
        assert!(c.ends_with(TRUNCATION_MARKER));
    }

    #[test]
    fn answer_respects_both_limits() -> Res {
        let v = validated(&"word ".repeat(1000), 60)?;
        let a = answer(
            Locale::En,
            &v,
            None,
            ASKER,
            "does a long answer still fit?",
            &no_symbols(),
        );
        assert!(
            a.content.chars().count() <= CONTENT_LIMIT,
            "{}",
            a.content.len()
        );
        assert!(
            a.content
                .starts_with("<@1000000000000000042> asked: does a long answer")
        );
        assert!(a.content.ends_with(TRUNCATION_MARKER));
        assert!(a.citations.chars().count() <= EMBED_DESCRIPTION_LIMIT);
        assert!(a.citations.contains("… and "), "{}", a.citations);
        assert!(a.citations.starts_with(
            "[702.19b](https://yawgatog.com/resources/magic-rules/#R70219b) “The controller"
        ));
        assert_eq!(a.footer, "Confidence: High · CR 2026-08-19");
        Ok(())
    }

    #[test]
    fn short_answer_follows_the_header_with_one_citation_per_line() -> Res {
        let v = validated(LONG_ENOUGH, 2)?;
        let a = answer(Locale::En, &v, None, ASKER, "trample?", &no_symbols());
        assert_eq!(
            a.content,
            format!("<@1000000000000000042> asked: trample?\n\n{LONG_ENOUGH}")
        );
        assert_eq!(a.citations.lines().count(), 2);
        assert!(a.citations.lines().all(|l| {
            l.starts_with("[702.19b](https://yawgatog.com/resources/magic-rules/#R70219b) “")
                && l.ends_with('”')
        }));
        Ok(())
    }

    #[test]
    fn rule_anchor_matches_the_mirror_ids() -> Res {
        // Formats observed in the live page's HTML on 2026-08-30.
        for (id, anchor) in [
            ("100.1", "R1001"),
            ("702.19b", "R70219b"),
            ("613.1a", "R6131a"),
            ("702", "R702"),
            ("704.5aa", "R7045aa"),
        ] {
            assert_eq!(rule_anchor(&RuleId::try_new(id.to_owned())?), anchor);
        }
        assert_eq!(
            rule_url(&RuleId::try_new("100.1".to_owned())?),
            "https://yawgatog.com/resources/magic-rules/#R1001"
        );
        Ok(())
    }

    #[test]
    fn citation_lines_for_every_kind() -> Result<(), Box<dyn std::error::Error>> {
        let id = CardId::new(Uuid::from_u128(1));
        let url = "https://scryfall.com/search?q=oracleid%3A00000000-0000-0000-0000-000000000001";
        assert_eq!(scryfall_url(id), url);
        let ctx = Context {
            cards: vec![card(1, "Dark Confidant")],
            ..Context::default()
        };
        let r = Citation::ScryfallRuling {
            card: id,
            ruling: ruling_key("2020-01-01", "a ruling"),
            quote: judge_core::Quote::try_new("a  ruling\nwith   space")?,
        };
        assert_eq!(
            citation_line(Locale::En, &r, Some(&ctx), &no_symbols()).to_string(),
            format!("[Ruling — Dark Confidant]({url}) “a ruling with space”")
        );
        // Without a context the link still works; only the name is missing.
        assert_eq!(
            citation_line(Locale::En, &r, None, &no_symbols()).to_string(),
            format!("[Scryfall ruling]({url}) “a ruling with space”")
        );
        let p = Citation::PriorCall {
            id: judge_core::CallId::new(Uuid::from_u128(2)),
            quote: judge_core::Quote::try_new("x".repeat(300))?,
        };
        let line = citation_line(Locale::En, &p, Some(&ctx), &no_symbols()).to_string();
        assert!(line.starts_with("[prior call] “"));
        assert!(line.chars().count() <= QUOTE_LIMIT + 20);
        assert!(line.contains(TRUNCATION_MARKER));
        let o = Citation::OracleText {
            card: id,
            face: 0,
            quote: judge_core::Quote::try_new("Flying")?,
        };
        assert_eq!(
            citation_line(Locale::En, &o, Some(&ctx), &no_symbols()).to_string(),
            format!("[Oracle text — Dark Confidant]({url}) “Flying”")
        );
        assert_eq!(
            citation_line(Locale::En, &o, None, &no_symbols()).to_string(),
            format!("[Oracle text]({url}) “Flying”")
        );
        let back = Citation::OracleText {
            card: id,
            face: 1,
            quote: judge_core::Quote::try_new("Insectile")?,
        };
        assert_eq!(
            citation_line(Locale::En, &back, Some(&ctx), &no_symbols()).to_string(),
            format!("[Oracle text, face 2 — Dark Confidant]({url}) “Insectile”")
        );
        Ok(())
    }

    #[test]
    fn linked_citations_still_fit_the_embed_budget() -> Result<(), Box<dyn std::error::Error>> {
        let quote = judge_core::Quote::try_new("q".repeat(300))?;
        // Worst case: every line carries a long name, a full URL and a full
        // quote; fit_lines must count the whole markdown, not just the label.
        let id = CardId::new(Uuid::from_u128(7));
        let ctx = Context {
            cards: vec![card(7, &"Asmoranomardicadaistinaculdacar ".repeat(3))],
            ..Context::default()
        };
        let lines: Vec<Rendered> = (0..40)
            .map(|i| {
                citation_line(
                    Locale::En,
                    &Citation::ScryfallRuling {
                        card: id,
                        ruling: ruling_key("2020-01-01", &i.to_string()),
                        quote: quote.clone(),
                    },
                    Some(&ctx),
                    &no_symbols(),
                )
            })
            .collect();
        assert!(lines.iter().all(|l| l.len() > 300));
        let out = fit_lines(Locale::En, &lines, EMBED_DESCRIPTION_LIMIT);
        assert!(
            out.chars().count() <= EMBED_DESCRIPTION_LIMIT,
            "{}",
            out.len()
        );
        assert!(out.contains("… and "), "some lines must have been dropped");
        Ok(())
    }

    #[test]
    fn fit_lines_drops_whole_lines() {
        let texts: Vec<String> = (0..10)
            .map(|i| format!("line {i} {}", "x".repeat(30)))
            .collect();
        let lines: Vec<Rendered> = texts.iter().map(Rendered::plain).collect();
        let out = fit_lines(Locale::En, &lines, 120);
        assert!(out.chars().count() <= 120, "{out}");
        assert!(
            out.lines()
                .next()
                .is_some_and(|l| l == texts.first().map_or("", String::as_str))
        );
        assert!(out.ends_with(" more"), "{out}");
        assert_eq!(fit_lines(Locale::En, &[], 100), "");
        // One over-long line is cut rather than dropped.
        let one = vec![Rendered::plain("y".repeat(500))];
        let out = fit_lines(Locale::En, &one, 50);
        assert_eq!(out.chars().count(), 50);
    }

    #[test]
    fn symbols_become_emoji_in_the_answer_and_the_quotes() -> Res {
        let symbols = SymbolTable::new([
            ("mana_t".to_owned(), TAP_ID),
            ("mana_g".to_owned(), GREEN_ID),
        ]);
        let id = CardId::new(Uuid::from_u128(1));
        let ctx = Context {
            cards: vec![card(1, "Llanowar Elves")],
            ..Context::default()
        };
        let line = citation_line(
            Locale::En,
            &Citation::OracleText {
                card: id,
                face: 0,
                quote: judge_core::Quote::try_new("{T}: Add {G}.")?,
            },
            Some(&ctx),
            &symbols,
        );
        assert!(
            line.to_string()
                .ends_with(&format!("“<:mana_t:{TAP_ID}>: Add <:mana_g:{GREEN_ID}>.”")),
            "{line}"
        );
        // A symbol with no emoji uploaded stays as Scryfall wrote it.
        let v = validated("Tap it for {G}, not {W}: the elf makes green mana only.", 1)?;
        let a = answer(
            Locale::En,
            &v,
            Some(&ctx),
            ASKER,
            "how much for {G}?",
            &symbols,
        );
        assert!(
            a.content
                .contains(&format!("Tap it for <:mana_g:{GREEN_ID}>, not {{W}}:")),
            "{}",
            a.content
        );
        assert!(
            a.content
                .contains(&format!("asked: how much for <:mana_g:{GREEN_ID}>?")),
            "{}",
            a.content
        );
        Ok(())
    }

    /// A question of nothing but symbols used to expand past `CONTENT_LIMIT` on
    /// its own and truncate the ruling away entirely: `{W}` is three characters
    /// in, twenty-nine out, and `QUESTION_LIMIT` counted the three.
    #[test]
    fn a_symbol_only_question_cannot_crowd_out_the_answer() -> Res {
        let symbols = SymbolTable::new([("mana_t".to_owned(), TAP_ID)]);
        let v = validated(LONG_ENOUGH, 1)?;
        // Every length up to and past what QUESTION_LIMIT allows.
        for repeats in [1usize, 40, 68, 100, 400] {
            let question = "{T}".repeat(repeats);
            let a = answer(Locale::En, &v, None, ASKER, &question, &symbols);
            assert!(
                a.content.chars().count() <= CONTENT_LIMIT,
                "{repeats}: {} chars",
                a.content.chars().count()
            );
            let (head, body) = a
                .content
                .split_once(SEPARATOR)
                .ok_or("the header and body must stay separated")?;
            assert!(
                head.chars().count() <= HEADER_LIMIT,
                "{repeats}: header is {} chars",
                head.chars().count()
            );
            assert!(
                body.starts_with("Trample assigns"),
                "{repeats}: the answer was crowded out: {body:?}"
            );
        }
        Ok(())
    }

    /// One symbol-dense quote used to take over half the embed and push every
    /// other citation into "… and N more".
    #[test]
    fn a_symbol_heavy_quote_leaves_room_for_other_citations()
    -> Result<(), Box<dyn std::error::Error>> {
        let symbols = SymbolTable::new([("mana_t".to_owned(), TAP_ID)]);
        let id = CardId::new(Uuid::from_u128(1));
        let heavy = Citation::OracleText {
            card: id,
            face: 0,
            quote: judge_core::Quote::try_new("{T}".repeat(300))?,
        };
        let line = citation_line(Locale::En, &heavy, None, &symbols);
        assert!(
            line.len() <= CITATION_LINE_LIMIT,
            "one line took {} of {EMBED_DESCRIPTION_LIMIT}",
            line.len()
        );
        let lines: Vec<Rendered> = (0..6)
            .map(|_| citation_line(Locale::En, &heavy, None, &symbols))
            .collect();
        let out = fit_lines(Locale::En, &lines, EMBED_DESCRIPTION_LIMIT);
        assert!(out.chars().count() <= EMBED_DESCRIPTION_LIMIT);
        assert!(
            out.lines().count() >= 4,
            "only {} lines survived: {out}",
            out.lines().count()
        );
        Ok(())
    }

    #[test]
    fn a_symbol_heavy_answer_still_fits_the_content_limit() -> Res {
        // Each `{T}` becomes ~13 characters, so the raw answer is well under
        // the limit while the sent message would not be.
        let symbols = SymbolTable::new([("mana_t".to_owned(), TAP_ID)]);
        let v = validated(&"{T}".repeat(600), 1)?;
        let a = answer(Locale::En, &v, None, ASKER, "symbols?", &symbols);
        assert!(
            a.content.chars().count() <= CONTENT_LIMIT,
            "{}",
            a.content.chars().count()
        );
        assert!(a.content.ends_with(TRUNCATION_MARKER));
        // No tag was cut in half.
        let tags = a.content.matches(&format!("<:mana_t:{TAP_ID}>")).count();
        assert_eq!(
            tags,
            a.content.matches("<:").count(),
            "a tag was cut in half"
        );
        assert!(
            tags > 10,
            "only {tags} tags survived; the answer is nearly all symbols"
        );
        Ok(())
    }

    fn ambiguous(query: &str, names: &[&str]) -> Ambiguous {
        let cards: Vec<Card> = names
            .iter()
            .enumerate()
            .map(|(i, n)| card(i as u128 + 1, n))
            .collect();
        Ambiguous {
            query: query.into(),
            candidates: NonEmpty::from_vec(cards)
                .unwrap_or_else(|| NonEmpty::new(card(99, "fallback"))),
        }
    }

    #[test]
    fn a_buttonless_did_you_mean_lists_five_and_says_how_to_ask_again() {
        let seven = [
            "Urza's Mine",
            "Urza's Tower",
            "Urza's Power Plant",
            "Urza's Saga",
            "Urza's Bauble",
            "Urza's Rage",
            "Urza, Lord High Artificer",
        ];
        let spans = NonEmpty::from((
            ambiguous("urza", &seven),
            vec![ambiguous("bob", &["Dark Confidant", "Bob's Burgers"])],
        ));
        let text = did_you_mean(Locale::En, &spans);
        assert!(text.contains("**urza**") && text.contains("Did you mean"));
        assert!(text.contains("\n1. Urza's Mine"), "{text}");
        assert!(text.contains("5. Urza's Bauble") && !text.contains("Urza's Rage"));
        assert!(text.contains("…and 2 more."), "{text}");
        assert!(text.ends_with("written as [[Full Card Name]]."), "{text}");
        // error() renders the same text for the variant.
        assert_eq!(error(Locale::En, &JudgeError::AmbiguousCards(spans)), text);
        let one = NonEmpty::new(ambiguous(
            "bruna",
            &["Bruna, the Fading Light", "Bruna, Light of Alabaster"],
        ));
        assert!(!did_you_mean(Locale::En, &one).contains("more."));
    }

    fn prompt_for(question: &str, spans: &NonEmpty<Ambiguous>) -> Option<PickPrompt> {
        PickPrompt::new(Locale::En, ASKER, question, spans)
    }

    #[test]
    fn a_pick_prompt_restates_the_question_verbatim_above_the_list() {
        let spans = NonEmpty::new(ambiguous("bob", &["Dark Confidant", "Bob's Burgers"]));
        let p = prompt_for("does  bob\tdie to {W}?", &spans);
        assert_eq!(
            p.as_ref().map(PickPrompt::render).as_deref(),
            Some(
                "<@1000000000000000042> asked: does  bob\tdie to {W}?\n\n\
                 I'm not sure which card you mean by **bob**. Did you mean…?\n\
                 1. Dark Confidant\n\
                 2. Bob's Burgers"
            )
        );
        let mut seven: Vec<String> = (0..6).map(|i| format!("Card {i}")).collect();
        seven.insert(1, format!("  Long\n  {}  ", "n".repeat(PICK_NAME_LIMIT)));
        let seven: Vec<&str> = seven.iter().map(String::as_str).collect();
        let spans = NonEmpty::from((
            ambiguous("  urza\n\n  lands ", &seven),
            vec![ambiguous("bob", &["A", "B"]), ambiguous("x", &["C", "D"])],
        ));
        let body = prompt_for("q", &spans)
            .map(|p| p.render())
            .and_then(|c| c.split_once("\n\n").map(|(_, b)| b.to_owned()));
        let long = fit(
            &format!("Long {}", "n".repeat(PICK_NAME_LIMIT)),
            PICK_NAME_LIMIT,
        );
        assert!(long.ends_with(TRUNCATION_MARKER) && long.chars().count() == PICK_NAME_LIMIT);
        assert_eq!(
            body,
            Some(format!(
                "I'm not sure which card you mean by **urza lands**. Did you mean…?\n\
                 1. Card 0\n2. {long}\n3. Card 1\n4. Card 2\n5. Card 3\n\
                 (Showing the first five. If yours isn't here, write its full name as [[Full Card Name]].)\n\
                 After that I'll ask about 2 more names."
            ))
        );
        let two = NonEmpty::from((ambiguous("a", &["A", "B"]), vec![ambiguous("b", &["C"])]));
        assert!(prompt_for("q", &two).is_some_and(|p| {
            p.render()
                .ends_with("2. B\nAfter that I'll ask about one more name.")
        }));
    }

    #[test]
    fn a_pick_prompt_round_trips_any_question() {
        let long_span = "w".repeat(PICK_SPAN_LIMIT * 3);
        let seven: Vec<String> = (0..7).map(|i| format!("Card {i}")).collect();
        let seven: Vec<&str> = seven.iter().map(String::as_str).collect();
        let span_sets = [
            NonEmpty::new(ambiguous("bob", &["A", "B"])),
            NonEmpty::from((
                ambiguous(&long_span, &seven),
                vec![ambiguous("x", &["C", "D"]); 12],
            )),
            NonEmpty::new(ambiguous("**. Did you mean…?", &["A", "B"])),
            NonEmpty::new(ambiguous(
                "1. x",
                &[
                    "1. A",
                    "(Showing the first five.",
                    "After that I'll ask about one more name.",
                ],
            )),
        ];
        let max_ascii = "q".repeat(PICK_QUESTION_LIMIT);
        let max_wide = "é".repeat(PICK_QUESTION_LIMIT);
        let questions = [
            "",
            "plain question?",
            "**bold** `code` > quote\n> more _it_ ~~x~~ ||spoiler||",
            "two paragraphs\n\nI'm not sure which card you mean by **x**. Did you mean…?",
            "ends in a blank line\n\n",
            "trailing whitespace   ",
            "   leading whitespace",
            "\n\n\n",
            "<:mana_w:1> and {W}{U} and <@123> <@&456> @everyone @here",
            "<@1000000000000000042> asked: nested",
            "ünïcödé — “quotes” 🃏 日本語",
            "\r\n windows\r\n",
            max_ascii.as_str(),
            max_wide.as_str(),
        ];
        for locale in Locale::ALL {
            for spans in &span_sets {
                for q in questions {
                    let p = PickPrompt::new(locale, ASKER, q, spans);
                    assert!(p.is_some(), "{locale}: {q:?} must fit");
                    let Some(p) = p else { continue };
                    let content = p.render();
                    assert!(content.chars().count() <= CONTENT_LIMIT, "{locale}: {q:?}");
                    let back = PickPrompt::parse(locale, &content);
                    assert_eq!(back.as_ref(), Some(&p), "{locale}: {q:?}");
                    assert_eq!(back.as_ref().map(PickPrompt::question), Some(q));
                    assert_eq!(back.as_ref().map(PickPrompt::asker), Some(ASKER));
                    // Read back in another locale's template, it is no prompt.
                    for other in Locale::ALL.into_iter().filter(|o| *o != locale) {
                        assert_eq!(PickPrompt::parse(other, &content), None, "{other}: {q:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn a_pick_prompt_fits_at_the_limits_and_refuses_beyond() {
        // Every variable part at its longest.
        let seven: Vec<String> = (0..7).map(|i| format!("{i}{}", "ü".repeat(200))).collect();
        let seven: Vec<&str> = seven.iter().map(String::as_str).collect();
        let span = "ü".repeat(PICK_SPAN_LIMIT * 2);
        let spans = NonEmpty::from((
            ambiguous(&span, &seven),
            vec![ambiguous("x", &["C", "D"]); 3],
        ));
        for locale in Locale::ALL {
            let mut p = PickPrompt::new(locale, u64::MAX, &"q".repeat(PICK_QUESTION_LIMIT), &spans);
            if let Some(p) = p.as_mut() {
                p.later = usize::MAX;
            }
            let content = p.as_ref().map(PickPrompt::render);
            assert!(
                content
                    .as_ref()
                    .is_some_and(|c| c.chars().count() <= CONTENT_LIMIT),
                "{locale}: {:?}",
                content.map(|c| c.chars().count())
            );
        }
        // Characters outside the BMP count twice in UTF-16: refused, not cut.
        let cards = "🃏".repeat(PICK_QUESTION_LIMIT);
        assert_eq!(PickPrompt::new(Locale::En, ASKER, &cards, &spans), None);
        // So is anything past the content limit.
        assert_eq!(
            PickPrompt::new(Locale::En, ASKER, &"q".repeat(CONTENT_LIMIT), &spans),
            None
        );
    }

    #[test]
    fn a_pick_prompt_refuses_content_it_did_not_write() {
        let spans = NonEmpty::new(ambiguous("bob", &["A", "B"]));
        let Some(good) = prompt_for("does bob die?", &spans).map(|p| p.render()) else {
            unreachable!("a short question fits");
        };
        assert!(PickPrompt::parse(Locale::En, &good).is_some());
        let tampered = [
            String::new(),
            // What the message says while the pick is judged, and an answer.
            working(Locale::En, "Dark Confidant"),
            with_header(
                Locale::En,
                ASKER,
                "does bob die?",
                "Yes, it dies.",
                &no_symbols(),
            ),
            // The buttonless reply.
            with_header(
                Locale::En,
                ASKER,
                "does bob die?",
                &did_you_mean(Locale::En, &spans),
                &no_symbols(),
            ),
            good.replacen("<@", "<@!", 1),
            good.replacen("<@1", "<@a", 1),
            good.replacen("<@1000000000000000042>", "<@>", 1),
            good.replacen(" asked: ", " asked:", 1),
            format!("{good} "),
            format!("{good}\n"),
            format!(" {good}"),
            good.replace("**bob**", "**bob  b**"),
            good.replace("**bob**", "**bob\nb**"),
            good.replace("Did you mean…?", "Did you mean...?"),
            format!("{good}\nAfter that I'll ask about 0 more names."),
            format!("{good}\nAfter that I'll ask about 1 more names."),
            format!("{good}\nAfter that I'll ask about 02 more names."),
            format!("{good}\nAfter that I'll ask about +2 more names."),
            format!("{good}\n{PICK_LATER_ONE}\n{PICK_MORE}"),
            format!("{good}\n{PICK_LATER_ONE}\n{PICK_LATER_ONE}"),
            format!("{good}\n\n3. C"),
            good.replace("\n1. A\n2. B", ""),
            good.replace("2. B", "3. B"),
            good.replace("1. A", "1.  A"),
            good.replace("1. A", "1. A "),
            good.replace("\n1. A", "\n1. A\n"),
            good.replace("\n\n", "\n"),
            good.replace("\n\n", " "),
        ];
        for t in &tampered {
            assert_eq!(PickPrompt::parse(Locale::En, t), None, "{t:?}");
        }
        // A second prompt appended after a blank line is a different, valid
        // prompt whose question is everything before the last blank line.
        let nested = format!(
            "<@1> asked: {good}\n\n{}",
            good.split_once("\n\n").map_or("", |(_, b)| b)
        );
        assert_eq!(
            PickPrompt::parse(Locale::En, &nested).map(|p| p.question().to_owned()),
            Some(good.clone())
        );
    }

    #[test]
    fn every_error_variant_has_a_message() -> Res {
        let not_found = JudgeError::CardsNotFound(NonEmpty::from((
            "Xyzzy".to_owned(),
            vec!["Plugh".to_owned()],
        )));
        let m = error(Locale::En, &not_found);
        assert!(
            m.contains("**Xyzzy**") && m.contains("**Plugh**") && m.contains("[[Full Card Name]]"),
            "{m}"
        );

        for s in [Source::Tournament, Source::OutOfScope] {
            assert_eq!(error(Locale::En, &JudgeError::OutOfScope(s)), OUT_OF_SCOPE);
        }
        let bad = Citation::Rule {
            id: RuleId::try_new("702.19b".to_owned())?,
            quote: judge_core::Quote::try_new("nope")?,
        };
        assert_eq!(error(Locale::En, &JudgeError::BadCitation(bad)), FAILED);
        assert_eq!(
            error(
                Locale::En,
                &JudgeError::EmptyVerdict(EmptyVerdict::NoCitations)
            ),
            FAILED
        );
        assert_eq!(
            error(
                Locale::En,
                &JudgeError::EmptyVerdict(EmptyVerdict::ShortAnswer { chars: 3 })
            ),
            FAILED
        );
        assert_eq!(error(Locale::En, &JudgeError::LlmRefused), FAILED);
        assert_eq!(
            error(
                Locale::En,
                &JudgeError::Upstream(anyhow::anyhow!("db down"))
            ),
            FAILED
        );
        let cap = judge_llm::LlmError::SpendCapExceeded {
            spent: 5.0,
            cap: 5.0,
        };
        assert_eq!(
            error(
                Locale::En,
                &JudgeError::Upstream(anyhow::Error::new(cap).context("extract"))
            ),
            SPEND_CAP
        );
        // Nothing internal leaks into the generic message.
        assert!(
            !error(
                Locale::En,
                &JudgeError::Upstream(anyhow::anyhow!("secret detail"))
            )
            .contains("secret")
        );
        Ok(())
    }

    #[test]
    fn labels() {
        assert_eq!(rating_label(Locale::En, Score::Incorrect), "Incorrect");
        assert_eq!(
            rating_label(Locale::En, Score::Partial),
            "Partially correct"
        );
        assert_eq!(rating_label(Locale::En, Score::Correct), "Correct");
        assert_eq!(button_label("  Dark Confidant "), "Dark Confidant");
        let long = "Asmoranomardicadaistinaculdacar ".repeat(4);
        let l = button_label(&long);
        assert_eq!(l.chars().count(), BUTTON_LABEL_LIMIT);
        assert!(l.ends_with('…'));
        assert!(working(Locale::En, "Bob").contains("**Bob**"));
    }

    #[test]
    fn cr_date_formats_eight_digits_only() {
        assert_eq!(cr_date("20260819"), "2026-08-19");
        assert_eq!(cr_date("2026"), "2026");
    }
}

#[cfg(test)]
mod info_tests {
    use super::*;

    #[test]
    fn help_parts_each_fit_a_message_and_name_the_other_commands() -> anyhow::Result<()> {
        // The longest offer: a long repository URL, a dirty build.
        let offer = SourceOffer::new(
            judge_core::RepositoryUrl::try_new(
                "https://gitlab.example-hosting-company.com/some-organisation/some-team/mtg-judgebot-fork",
            )?,
            judge_core::Commit::Known {
                hash: judge_core::CommitHash::try_new("a37d495c937819de39a30ba0624f9bffbfc494d2")?,
                dirty: true,
            },
        );
        // …and the longest contact: a 32-character username and an address.
        let operator = judge_core::Operator::new(
            Some(judge_core::DiscordUsername::try_new(
                "a_judge.with_the_longest_name_32",
            )?),
            Some(judge_core::SupportEmail::try_new(
                "judgebot-support@some-organisation.example-hosting-company.com",
            )?),
        )
        .for_discord()?;
        // …and the longest data section: every line, the oldest age.
        let freshness = Freshness {
            cr_version: Some(judge_core::CrVersion::try_new("20260925".to_owned())?),
            refreshed_secs_ago: Some(u64::MAX),
            last_refresh_failed: true,
        };
        let parts = help(
            Locale::En,
            &offer,
            &operator,
            Some(&freshness),
            Feed::default(),
        );
        for part in &parts {
            assert!(
                !part.is_empty() && part.chars().count() <= CONTENT_LIMIT,
                "{}",
                part.chars().count()
            );
        }
        let help = parts.join("\n\n");
        for needle in [
            "/judge",
            "/forget",
            "[[Full Card Name]]",
            "Fan Content",
            "Scryfall",
            source::COPYRIGHT,
            "<https://gitlab.example-hosting-company.com/some-organisation/some-team/mtg-judgebot-fork>",
            "[commit a37d495, built with uncommitted changes](<https://gitlab.example-hosting-company.com/some-organisation/some-team/mtg-judgebot-fork/commit/a37d495c937819de39a30ba0624f9bffbfc494d2>)",
            "`@a_judge.with_the_longest_name_32` on Discord",
            "`judgebot-support@some-organisation.example-hosting-company.com`",
            "**Data.**\n- Comprehensive Rules: 2026-09-25\n- Last successful refresh: ",
            "**What is stored.**",
            "**The feed.**",
            " days ago\n- Latest refresh: failed",
        ] {
            assert!(help.contains(needle), "{needle}\n{help}");
        }
        assert!(
            parts
                .last()
                .is_some_and(|p| p.ends_with(&license(Locale::En, &offer, &operator))),
            "the second part ends with what /license says"
        );
        let off = super::help(Locale::En, &offer, &operator, None, Feed::Off).join("\n\n");
        assert!(off.contains("**What is stored.**") && !off.contains("**The feed.**"));
        // Without a support address the username stands alone.
        let alone = judge_core::Operator::new(
            Some(judge_core::DiscordUsername::try_new("somejudge")?),
            None,
        )
        .for_discord()?;
        assert!(
            contact(Locale::En, &alone).contains("`@somejudge` on Discord. "),
            "{}",
            contact(Locale::En, &alone)
        );
        // An unstamped build says so and links nothing for the commit.
        let unknown = license(
            Locale::En,
            &SourceOffer::upstream(judge_core::Commit::Unknown),
            &alone,
        );
        assert!(unknown.contains("(commit unknown)"), "{unknown}");
        assert!(!unknown.contains("/commit/"), "{unknown}");
        Ok(())
    }

    #[test]
    fn the_data_section_lists_what_was_read_or_says_it_was_not() -> anyhow::Result<()> {
        let fresh = Freshness {
            cr_version: Some(judge_core::CrVersion::try_new("20260925".to_owned())?),
            refreshed_secs_ago: Some(3 * 3600 + 120),
            last_refresh_failed: false,
        };
        assert_eq!(
            data(Locale::En, Some(&fresh)),
            "**Data.**\n- Comprehensive Rules: 2026-09-25\n- Last successful refresh: 3 hours ago"
        );
        assert_eq!(
            data(Locale::En, Some(&Freshness::default())),
            "**Data.**\n- Comprehensive Rules: none loaded\n- Last successful refresh: no refresh run recorded yet"
        );
        assert_eq!(
            data(Locale::En, None),
            "**Data.** Unknown: the database did not answer."
        );
        Ok(())
    }

    #[test]
    fn forgotten_counts_in_plain_english() {
        let f = |ratings, calls, failures| {
            (catalog::EN.forgotten)(judge_core::Forgotten {
                ratings,
                feedback: 0,
                failures,
                calls,
            })
        };
        let with_reasons = (catalog::EN.forgotten)(judge_core::Forgotten {
            ratings: 2,
            feedback: 1,
            failures: 0,
            calls: 0,
        });
        assert!(
            with_reasons.starts_with("Deleted your 2 ratings with their reasons."),
            "{with_reasons}"
        );
        assert_eq!(
            f(0, 0, 0),
            "You had no ratings, questions or failed calls on record; there is nothing else stored about you."
        );
        assert!(f(1, 0, 0).starts_with("Deleted your 1 rating."));
        assert!(f(7, 0, 0).starts_with("Deleted your 7 ratings."));
        assert!(f(0, 0, 2).starts_with("Anonymized 2 failed calls of yours."));
        assert!(f(0, 1, 0).starts_with("Removed your id and username from 1 question you asked."));
        assert!(
            f(1, 0, 1).starts_with("Deleted your 1 rating and anonymized 1 failed call of yours.")
        );
        assert_eq!(
            f(2, 3, 1),
            "Deleted your 2 ratings, removed your id and username from 3 questions you asked and \
             anonymized 1 failed call of yours. Nothing else is stored about you."
        );
    }
}

#[cfg(test)]
mod lookup_tests {
    use std::num::NonZeroU32;

    use judge_core::{CrVersion, Face, Layout, ruling_key};
    use uuid::Uuid;

    use super::*;

    type Res = Result<(), Box<dyn std::error::Error>>;

    fn offer(url: &str) -> Result<SourceOffer, Box<dyn std::error::Error>> {
        Ok(SourceOffer::new(
            judge_core::RepositoryUrl::try_new(url)?,
            judge_core::Commit::Unknown,
        ))
    }

    fn operator() -> Result<DiscordOperator, Box<dyn std::error::Error>> {
        Ok(
            judge_core::Operator::new(Some(judge_core::DiscordUsername::try_new("a_judge")?), None)
                .for_discord()?,
        )
    }

    #[test]
    fn only_an_incorrect_rating_says_where_to_report_it() -> Res {
        let (offer, op) = (offer("https://github.com/someone/fork")?, operator()?);
        assert!(
            rated(Locale::En, Score::Correct, None, true, &offer, &op, false).contains("judge")
        );
        assert!(
            !rated(Locale::En, Score::Correct, None, false, &offer, &op, false).contains("judge")
        );
        for score in [Score::Correct, Score::Partial] {
            assert!(!rated(Locale::En, score, None, false, &offer, &op, false).contains("a_judge"));
        }
        let wrong = rated(
            Locale::En,
            Score::Incorrect,
            Some(FeedbackCategory::WrongCard),
            false,
            &offer,
            &op,
            false,
        );
        assert!(wrong.contains("`@a_judge`"), "{wrong}");
        assert!(!wrong.contains("public"), "{wrong}");
        let public = rated(
            Locale::En,
            Score::Incorrect,
            Some(FeedbackCategory::WrongCard),
            false,
            &offer,
            &op,
            true,
        );
        assert!(
            public.contains("public web page, without your name"),
            "{public}"
        );
        assert!(
            !rated(Locale::En, Score::Correct, None, false, &offer, &op, true).contains("public"),
            "a Correct has no reason to show"
        );
        assert!(wrong.contains("Incorrect (Wrong card)"), "{wrong}");
        assert!(
            wrong
                .contains("<https://github.com/someone/fork/issues/new?template=wrong_answer.yml>"),
            "{wrong}"
        );
        assert!(wrong.chars().count() <= CONTENT_LIMIT);
        Ok(())
    }

    /// Keep says what is on record now, not what the prompt once showed:
    /// a Replace on the same prompt may have changed it since.
    #[test]
    fn keep_reads_back_the_rating_on_record() {
        let now = Earlier {
            score: Score::Incorrect,
            category: Some(FeedbackCategory::WrongCard),
        };
        assert_eq!(
            on_record(Locale::En, now),
            "Nothing changed. Your rating on record: **Incorrect** (Wrong card)."
        );
    }

    #[test]
    fn the_replace_prompt_names_both_ratings() {
        let earlier = Earlier {
            score: Score::Partial,
            category: Some(FeedbackCategory::WrongCard),
        };
        assert_eq!(
            replace_prompt(Locale::En, earlier, Score::Incorrect),
            "You already rated this answer **Partially correct** (Wrong card). Replace it with \
             **Incorrect**?"
        );
        let legacy = Earlier {
            score: Score::Incorrect,
            category: None,
        };
        assert_eq!(
            replace_prompt(Locale::En, legacy, Score::Correct),
            "You already rated this answer **Incorrect**. Replace it with **Correct**?"
        );
    }

    #[test]
    fn the_issue_form_is_offered_for_a_github_repository_and_nothing_else() -> Res {
        let form = |url: &str| offer(url).map(|o| report_url(&o));
        assert_eq!(
            form("https://github.com/sloshy/mtg-judgebot")?.as_deref(),
            Some("https://github.com/sloshy/mtg-judgebot/issues/new?template=wrong_answer.yml")
        );
        assert_eq!(
            form("https://github.com/sloshy/mtg-judgebot.git/")?.as_deref(),
            Some("https://github.com/sloshy/mtg-judgebot/issues/new?template=wrong_answer.yml")
        );
        for other in [
            "https://gitlab.com/someone/fork",
            "https://github.com.evil.example/someone/fork",
            "https://github.com/someone",
            "https://github.com/someone/fork/tree/main",
            "http://github.com/someone/fork",
            "https://GitHub.com/someone/fork",
            "https://github.com/some?one/fork",
            "https://github.com/someone/fo>rk",
            "https://github.com/someone/.git",
        ] {
            assert_eq!(form(other)?, None, "{other}");
        }
        // Without the form the operator is still named.
        let wrong = rated(
            Locale::En,
            Score::Incorrect,
            None,
            false,
            &offer("https://gitlab.com/someone/fork")?,
            &operator()?,
            false,
        );
        assert!(wrong.contains("`@a_judge`") && !wrong.contains("issues/new"));
        Ok(())
    }

    #[test]
    fn the_cooldown_notice_rounds_the_wait_up_and_names_the_free_commands() {
        let limit = UserLimit {
            max: NonZeroU32::new(6).unwrap_or(NonZeroU32::MIN),
            window: Duration::from_mins(10),
        };
        let text = cooling_down(Locale::En, limit, Duration::from_millis(61_500));
        assert!(
            text.contains("6 questions in the last 10 minutes"),
            "{text}"
        );
        assert!(text.contains("again in 2 minutes"), "{text}");
        assert!(text.contains("`/card`"));
        assert!(
            cooling_down(Locale::En, limit, Duration::from_millis(200)).contains("in 1 second.")
        );
        assert!(cooling_down(Locale::En, limit, Duration::from_secs(60)).contains("in 1 minute."));
    }

    fn bolt() -> Card {
        Card {
            id: CardId::new(Uuid::from_u128(7)),
            name: "Fire // Ice".into(),
            layout: Layout::Split,
            faces: NonEmpty::from((
                Face {
                    name: "Fire".into(),
                    oracle_text: "Fire deals 2 damage divided as you choose.".into(),
                    mana_cost: "{1}{R}".into(),
                    type_line: "Instant".into(),
                },
                vec![Face {
                    name: "Ice".into(),
                    oracle_text: "Tap target permanent.\nDraw a card.".into(),
                    mana_cost: "{1}{U}".into(),
                    type_line: "Instant".into(),
                }],
            )),
        }
    }

    fn ruling(text: &str) -> Ruling {
        Ruling {
            card: CardId::new(Uuid::from_u128(7)),
            key: ruling_key("2026-01-02", text),
            published_at: "2026-01-02".into(),
            text: text.into(),
        }
    }

    #[test]
    fn a_card_shows_every_face_and_whole_rulings_within_the_embed() {
        let symbols = SymbolTable::new([("mana_r".to_owned(), 1_234_567_890_123_456_789)]);
        let few = card(
            Locale::En,
            &bolt(),
            &[ruling("You divide the damage\nas you cast it.")],
            &symbols,
        );
        assert_eq!(few.title, "Fire // Ice");
        assert_eq!(few.url, scryfall_url(CardId::new(Uuid::from_u128(7))));
        let d = &few.description;
        assert!(d.contains("**Fire**") && d.contains("**Ice**"), "{d}");
        assert!(d.contains("<:mana_r:1234567890123456789>"), "{d}");
        assert!(d.contains("{U}"), "an unknown symbol stays literal: {d}");
        assert!(d.contains("**Rulings (1)**"));
        assert!(
            d.contains("`2026-01-02` You divide the damage as you cast it."),
            "{d}"
        );

        let many: Vec<Ruling> = (0..200)
            .map(|n| {
                ruling(&format!(
                    "Ruling {n}. {}",
                    "A long ruling sentence. ".repeat(8)
                ))
            })
            .collect();
        let full = card(Locale::En, &bolt(), &many, &symbols);
        assert!(full.description.chars().count() <= EMBED_DESCRIPTION_LIMIT);
        assert!(full.description.contains("**Rulings (200)**"));
        assert!(
            full.description.contains("more"),
            "dropped rulings are counted"
        );

        assert!(
            card(Locale::En, &bolt(), &[], &symbols)
                .description
                .ends_with("No rulings.")
        );
    }

    #[test]
    fn card_misses_echo_a_bounded_name_and_ping_nobody_by_construction() {
        let long = "x".repeat(5000);
        assert!(card_not_found(Locale::En, &long).chars().count() <= CONTENT_LIMIT);
        let names: Vec<String> = (0..50).map(|n| format!("Urza's Thing {n}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let text = card_ambiguous(Locale::En, "urza", &refs);
        assert!(text.chars().count() <= CONTENT_LIMIT);
        assert_eq!(text.matches("\n- ").count(), MAX_CHOICES * 2);
    }

    fn chunk(
        id: &str,
        body: &str,
        examples: &[&str],
    ) -> Result<RuleChunk, Box<dyn std::error::Error>> {
        Ok(RuleChunk {
            id: RuleId::try_new(id.to_owned())?,
            parent_id: None,
            subsection: RuleId::try_new("702".to_owned())?,
            heading: "Trample".into(),
            body: body.into(),
            examples: examples.iter().map(|e| (*e).to_owned()).collect(),
            cr_version: CrVersion::try_new("20260819".to_owned())?,
        })
    }

    #[test]
    fn a_rule_shows_its_text_examples_and_cr_date_and_links_to_the_mirror() -> Res {
        let asked = RuleId::try_new("702.19b".to_owned())?;
        let chunks = [chunk(
            "702.19b",
            "702.19b The controller assigns damage.",
            &["A 2/2."],
        )?];
        let Some(view) = rules(Locale::En, &asked, &chunks, &SymbolTable::empty()) else {
            return Err("a rule with a chunk renders".into());
        };
        assert_eq!(view.title, "CR 702.19b — Trample");
        assert_eq!(view.url, rule_url(&asked));
        assert!(
            view.description
                .contains("702.19b The controller assigns damage.\n> Example: A 2/2.")
        );
        assert_eq!(view.footer.as_deref(), Some("CR 2026-08-19"));
        assert!(rules(Locale::En, &asked, &[], &SymbolTable::empty()).is_none());

        // A rule-level chunk carries its examples in the body, each quoted
        // under the line it belongs to.
        let asked = RuleId::try_new("903.3".to_owned())?;
        let chunks = [chunk(
            "903.3",
            "903.3. Each deck has a commander.\nExample: Face down.\n903.3a Some cards can be.",
            &[],
        )?];
        let Some(view) = rules(Locale::En, &asked, &chunks, &SymbolTable::empty()) else {
            return Err("a rule with a chunk renders".into());
        };
        assert!(view.description.contains(
            "903.3. Each deck has a commander.\n> Example: Face down.\n903.3a Some cards can be."
        ));
        Ok(())
    }

    #[test]
    fn a_whole_section_is_cut_at_a_rule_boundary_under_the_embed_limit() -> Res {
        let asked = RuleId::try_new("702".to_owned())?;
        let body = "A rule sentence that goes on for a while. ".repeat(20);
        let chunks: Vec<RuleChunk> = (1..=60)
            .map(|n| chunk(&format!("702.{n}"), &format!("702.{n}. {body}"), &[]))
            .collect::<Result<_, _>>()?;
        let Some(view) = rules(Locale::En, &asked, &chunks, &SymbolTable::empty()) else {
            return Err("a section renders".into());
        };
        assert!(view.description.chars().count() <= EMBED_DESCRIPTION_LIMIT);
        assert!(view.description.contains("more"));
        assert!(!view.description.contains("longer than Discord shows"));

        // One rule longer than the embed: cut, and told where the rest is.
        let asked = RuleId::try_new("205.3".to_owned())?;
        let long = [chunk("205.3", &"A very long rule. ".repeat(500), &[])?];
        let Some(view) = rules(Locale::En, &asked, &long, &SymbolTable::empty()) else {
            return Err("a long rule renders".into());
        };
        assert!(view.description.chars().count() <= EMBED_DESCRIPTION_LIMIT);
        assert!(
            view.description.ends_with("follow the title link."),
            "{}",
            view.description
        );
        Ok(())
    }
}

#[cfg(test)]
mod retry_tests {
    use super::*;

    #[test]
    fn a_retry_prompt_reads_back_exactly_what_it_wrote() {
        for locale in Locale::ALL {
            for q in [
                "does bob  die?",
                "two\n\nparagraphs\n\nhere",
                "{W}{U} and [[Bolt]]",
            ] {
                let p = RetryPrompt::new(locale, 42, q).map(|p| p.render());
                let back = p.as_deref().and_then(|c| RetryPrompt::parse(locale, c));
                assert_eq!(back.as_ref().map(RetryPrompt::question), Some(q), "{q:?}");
                assert_eq!(back.map(|b| b.asker()), Some(42));
            }
            let longest = "é".repeat(RETRY_QUESTION_LIMIT);
            assert!(
                RetryPrompt::new(locale, u64::MAX, &longest).is_some(),
                "{locale}"
            );
        }
        let longest = "é".repeat(RETRY_QUESTION_LIMIT);
        assert!(RetryPrompt::new(Locale::En, u64::MAX, &longest).is_some());
        let content = RetryPrompt::new(Locale::En, 1, "q")
            .map(|p| p.render())
            .unwrap_or_default();
        assert_eq!(RetryPrompt::parse(Locale::En, &format!("{content} ")), None);
        assert_eq!(
            RetryPrompt::parse(Locale::En, &content.replace("<@1>", "<@x>")),
            None
        );
        assert_eq!(
            RetryPrompt::parse(Locale::En, catalog::EN.maintenance),
            None
        );
    }
}
