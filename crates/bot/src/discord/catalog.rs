//! The Discord surface's text, one [`Messages`] per [`Locale`]
//! (`judge_core::locale`). Every reply, button label and command description
//! the bot sends comes from here; [`super::render`] fills them in and fits
//! them to Discord's limits. A field per message, so a new locale's catalog
//! that misses one does not compile.
//!
//! The pipeline's replies (busy, out of scope, a failed answer, the spend
//! cap, an unknown card, "did you mean…?") are here too, and the HTTP API
//! sends them as its `message`s (`judge_api::shape`), so the two interfaces
//! say the same thing.
//!
//! The pieces a "did you mean…?" or a Retry message is parsed back from
//! (`asked` and `pick_*`, `retry_body`) are plain `&str`s, so the question
//! limits can be computed over every locale at compile time
//! ([`super::render::PICK_QUESTION_LIMIT`]).

use std::{fmt, time::Duration};

use judge_core::{Confidence, Fault, FeedbackCategory, Forgotten, Locale, Score, locale::Catalogs};

/// One locale's Discord text.
#[derive(Clone, Copy)]
pub struct Messages {
    // ---- the pipeline's replies, shared with the HTTP API ----
    /// Every judge slot is taken.
    pub busy: &'static str,
    /// Tournament-policy and non-rules questions.
    pub out_of_scope: &'static str,
    /// The pipeline errored or could not produce a verified answer.
    pub failed: &'static str,
    /// The operator's spend cap has been reached.
    pub spend_cap: &'static str,
    /// No card matched: the names as listed (each already bolded), joined
    /// by the caller with [`Self::or`].
    pub cards_not_found: fn(&str) -> String,
    /// Joins alternatives in a list (`A or B`).
    pub or: &'static str,
    /// The buttonless "did you mean…?" lead, given the span.
    pub did_you_mean_head: fn(&str) -> String,
    /// After the listed choices, how many more there are.
    pub did_you_mean_more: fn(usize) -> String,
    /// The buttonless "did you mean…?" closing line.
    pub did_you_mean_tail: &'static str,
    /// Any command but `/help` and `/license` while a major upgrade migrates.
    pub maintenance: &'static str,

    // ---- the restated question and the "did you mean…?" prompt ----
    /// Between the asker's mention and the question: ` asked: `.
    pub asked: &'static str,
    /// The prompt's lead, before the span.
    pub pick_head: &'static str,
    /// The prompt's lead, after the span.
    pub pick_ask: &'static str,
    /// More candidates than buttons.
    pub pick_more: &'static str,
    /// One more ambiguous name follows.
    pub pick_later_one: &'static str,
    /// Several more follow: before the count.
    pub pick_later_head: &'static str,
    /// Several more follow: after the count.
    pub pick_later_tail: &'static str,

    // ---- buttons and their replies ----
    /// A pick answered too late, or its message no longer holds the question.
    pub expired: &'static str,
    /// A second click on a pick.
    pub already_picked: &'static str,
    /// A pick that could not be checked.
    pub pick_failed: &'static str,
    /// Someone other than the asker picked.
    pub not_yours: &'static str,
    /// Someone other than the asker pressed Retry.
    pub not_your_retry: &'static str,
    /// A Retry pressed a second time.
    pub already_retried: &'static str,
    /// A retried question's message while it is answered.
    pub retrying: &'static str,
    /// A pick's message while it is judged, given the card.
    pub working: fn(&str) -> String,
    /// A button whose id did not parse.
    pub unknown_button: &'static str,
    /// A rating button's label.
    pub rating_label: fn(Score) -> &'static str,
    /// The rating's acknowledgement, given its label, its reason's category
    /// (labelled) when it has one, and whether it counts as a judge's.
    pub rated: fn(&str, Option<&str>, bool) -> String,
    /// The same, for a rating with a reason, when `JUDGE_PUBLIC_FEEDBACK`
    /// shows reasons on the public pages.
    pub rated_public: fn(&str, Option<&str>, bool) -> String,
    /// After an *Incorrect* rating: who reads the reason, given the
    /// operator's username.
    pub report_wrong: fn(&str) -> String,
    /// After [`Self::report_wrong`]: the issue form, given its URL.
    pub report_link: fn(&str) -> String,
    /// The rating could not be stored.
    pub rate_failed: &'static str,
    /// A rating of an answer the retention deleted.
    pub call_gone: &'static str,
    /// Correct clicked again: it is already recorded.
    pub already_rated: &'static str,
    /// "You already rated this…": the earlier rating's label, its
    /// category's label when it had one, and the new rating's label.
    pub replace_prompt: fn(&str, Option<&str>, &str) -> String,
    /// The Replace button's label.
    pub replace_label: &'static str,
    /// The Keep button's label.
    pub keep_label: &'static str,
    /// After Keep: the rating on record now, given its label and its
    /// category's label when it has one.
    pub on_record: fn(&str, Option<&str>) -> String,
    /// After Keep, when the rater has no rating on the answer any more.
    pub no_rating: &'static str,
    /// A feedback category, as the form lists it (at most 100 characters).
    pub category_label: fn(FeedbackCategory) -> &'static str,
    /// The rating form's title (at most 45 characters).
    pub form_title: fn(Fault) -> &'static str,
    /// The form's category menu: its label (at most 45 characters).
    pub form_category: &'static str,
    /// The category menu's placeholder (at most 100 characters).
    pub form_category_placeholder: &'static str,
    /// "What's wrong": the field's label (at most 45 characters).
    pub form_wrong: &'static str,
    /// Its hint (at most 100 characters).
    pub form_wrong_hint: &'static str,
    /// Its hint when reasons are shown publicly (at most 100 characters).
    pub form_wrong_hint_public: &'static str,
    /// "The right answer": the field's label (at most 45 characters).
    pub form_right: &'static str,
    /// Its hint (at most 100 characters).
    pub form_right_hint: &'static str,
    /// Its hint when reasons are shown publicly (at most 100 characters).
    pub form_right_hint_public: &'static str,
    /// Above the category menu shown before the form when interactions
    /// arrive over the gateway, given the rating's label.
    pub category_prompt: fn(&str) -> String,
    /// A form that came back without its category: nothing was recorded.
    pub form_incomplete: &'static str,
    /// A form field over its limit, given the limit: nothing was recorded.
    pub form_too_long: fn(usize) -> String,
    /// A form field holding a control character: nothing was recorded.
    pub form_bad_text: &'static str,
    /// The maintenance reply's body, above its Retry button.
    pub retry_body: &'static str,
    /// The Retry button's label.
    pub retry_label: &'static str,
    /// A Retry pressed while the upgrade is still migrating.
    pub still_upgrading: &'static str,
    /// The message could not be updated with the answer.
    pub edit_failed: &'static str,

    // ---- the answer's embed ----
    /// The footer's confidence and CR line.
    pub footer_meta: fn(Confidence, &str) -> String,
    /// The footer's line naming the resolved cards, joined.
    pub cards_line: fn(&str) -> String,
    /// A ruling citation's label: its date (with a leading space) and card
    /// name, either possibly absent.
    pub ruling_label: fn(&str, Option<&str>) -> String,
    /// An Oracle-text citation's label: the face (from 2, when not the
    /// first) and card name.
    pub oracle_label: fn(Option<usize>, Option<&str>) -> String,
    /// A prior call citation's label.
    pub prior_call_label: &'static str,
    /// Opens a citation's quote, after its label.
    pub quote_open: &'static str,
    /// Closes a citation's quote.
    pub quote_close: &'static str,

    // ---- commands ----
    /// A command this build does not have.
    pub unknown_command: &'static str,
    /// `/judge` outside a server.
    pub server_only: &'static str,
    /// Past the per-user allowance: the limit, its window, and the wait.
    pub cooling_down: fn(u32, Duration, Duration) -> String,
    /// `/help`'s fixed part.
    pub help_body: &'static str,
    /// `/help`'s paragraph on what is stored, opening its second reply.
    pub help_stored: &'static str,
    /// `/help`'s paragraph on the public feed, shown unless `JUDGE_FEED=false`.
    pub help_feed: &'static str,
    /// `/help`'s closing paragraph: AI-generated, unofficial.
    pub help_disclaimer: &'static str,
    /// `/help`'s data section heading.
    pub data_heading: &'static str,
    /// `/help`'s data section when it could not be read.
    pub data_unknown: &'static str,
    /// Who runs the bot: the username (in a code span) and the email, if any.
    pub contact: fn(&str, Option<&str>) -> String,
    /// `/license`'s heading.
    pub license_heading: &'static str,
    /// `/license`'s offer: program, copyright, licence name, licence URL,
    /// repository and the (possibly linked) revision.
    pub license: fn(&License<'_>) -> String,
    /// `/forget`'s confirmation.
    pub forgotten: fn(Forgotten) -> String,
    /// `/forget` failed.
    pub forget_failed: &'static str,
    /// `/card` with a blank or over-long name.
    pub lookup_bad_name: &'static str,
    /// `/rule` with something that is not a rule number.
    pub lookup_bad_rule: &'static str,
    /// A lookup the database could not serve.
    pub lookup_failed: &'static str,
    /// `/card` for a card with no rulings.
    pub no_rulings: &'static str,
    /// `/card`'s rulings heading, given how many.
    pub rulings_heading: fn(usize) -> String,
    /// `/card` when the name could mean several cards: the lead, given the
    /// name.
    pub card_ambiguous_head: fn(&str) -> String,
    /// `/card` when the name could mean several cards: the closing line.
    pub card_ambiguous_tail: &'static str,
    /// `/card` when nothing matched, given the name.
    pub card_not_found: fn(&str) -> String,
    /// `/rule`'s title: the rule asked for, and its heading if it has one.
    pub rule_title: fn(&str, Option<&str>) -> String,
    /// `/rule`'s footer and a CR date: `CR 2026-08-19`.
    pub cr_label: fn(&str) -> String,
    /// Appended when even the first rule had to be cut.
    pub long_rule_note: &'static str,
    /// `/rule` when the number names nothing.
    pub rule_not_found: fn(&str) -> String,
    /// After a list cut to fit an embed, given how many lines were left
    /// out (the citations, `/card`'s rulings).
    pub more_lines: fn(usize) -> String,

    // ---- command registration ----
    /// The commands' descriptions, as Discord lists them.
    pub commands: Commands,
}

/// The facts `/license` states.
#[derive(Clone, Copy, Debug)]
pub struct License<'a> {
    /// The program's name.
    pub program: &'a str,
    /// The copyright line.
    pub copyright: &'a str,
    /// The licence, in words.
    pub license_name: &'a str,
    /// Where to read it.
    pub license_url: &'a str,
    /// Where the source is.
    pub repository: &'a str,
    /// The revision phrase, linked to the commit when the build knew it.
    pub revision: &'a str,
}

/// The six commands' descriptions and their options' (Discord allows 100
/// characters each; a test holds every locale to it).
#[derive(Clone, Copy, Debug)]
pub struct Commands {
    /// `/judge`.
    pub judge: &'static str,
    /// `/judge question:`.
    pub judge_question: &'static str,
    /// `/judge private:`.
    pub judge_private: &'static str,
    /// `/card`.
    pub card: &'static str,
    /// `/card name:`.
    pub card_name: &'static str,
    /// `/card private:`.
    pub card_private: &'static str,
    /// `/rule`.
    pub rule: &'static str,
    /// `/rule id:`.
    pub rule_id: &'static str,
    /// `/rule private:`.
    pub rule_private: &'static str,
    /// `/help`.
    pub help: &'static str,
    /// `/license`.
    pub license: &'static str,
    /// `/forget`.
    pub forget: &'static str,
}

impl fmt::Debug for Messages {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("discord::catalog::Messages")
    }
}

const CATALOGS: Catalogs<Messages> = Catalogs::new(&EN, &STUB);

/// The Discord text in `locale`.
#[must_use]
pub const fn messages(locale: Locale) -> &'static Messages {
    CATALOGS.get(locale)
}

/// Each locale's Discord tags (below).
const TAGS: Catalogs<[&str]> = Catalogs::new(&[], &[]);

/// The Discord locale tags a locale's text is registered under for command
/// descriptions (`description_localizations`). Empty for the default
/// locale, whose text is the base description, and for the stub, which is
/// no language Discord knows.
#[must_use]
pub const fn discord_tags(locale: Locale) -> &'static [&'static str] {
    TAGS.get(locale)
}

/// English.
pub const EN: Messages = Messages {
    busy: "I'm answering as many questions as I can right now; please try again in a minute.",
    out_of_scope: "I only answer Comprehensive Rules and Commander rules questions. Tournament policy \
                   (MTR/IPG) and questions that aren't about the rules are out of my scope: for \
                   policy, see the Magic Tournament Rules or ask a tournament judge.",
    failed: "Sorry, I couldn't produce a verified answer this time. Please try again, or rephrase the question.",
    spend_cap: "I've hit my spending cap for now, so I can't answer until the operator raises it.",
    cards_not_found: |listed| {
        format!(
            "I couldn't find a card called {listed}. Check the spelling, or write the full name as [[Full Card Name]]."
        )
    },
    or: " or ",
    did_you_mean_head: |span| {
        format!("I'm not sure which card you mean by **{span}**. Did you mean…?")
    },
    did_you_mean_more: |n| format!("…and {n} more."),
    did_you_mean_tail: "Ask again with the card you mean written as [[Full Card Name]].",
    maintenance: "Judgebot is being upgraded and will be back in a minute or two. \
Please try again shortly.",

    asked: " asked: ",
    pick_head: "I'm not sure which card you mean by **",
    pick_ask: "**. Did you mean…?",
    pick_more: "(Showing the first five. If yours isn't here, write its full name as [[Full Card Name]].)",
    pick_later_one: "After that I'll ask about one more name.",
    pick_later_head: "After that I'll ask about ",
    pick_later_tail: " more names.",

    expired: "That choice has expired, or the message no longer holds the question. \
              Please ask it again.",
    already_picked: "A card has already been picked for that question.",
    pick_failed: "Sorry, I couldn't take that pick just now. Please try again.",
    not_yours: "Only the person who asked can pick a card for that question.",
    not_your_retry: "Only the person who asked can retry that question.",
    already_retried: "That question has already been asked again.",
    retrying: "Asking again. Working on it…",
    working: |card| format!("Got it: **{card}**. Working on it…"),
    unknown_button: "I don't recognise that button any more.",
    rating_label: |score| match score {
        Score::Incorrect => "Incorrect",
        Score::Partial => "Partially correct",
        Score::Correct => "Correct",
    },
    rated: |label, category, as_judge| {
        let who = if as_judge { " as a judge ruling" } else { "" };
        match category {
            Some(category) => format!(
                "Recorded your rating{who}: {label} ({category}). The operator sees your reason \
beside the answer. Rating again asks before replacing it."
            ),
            None => {
                format!(
                    "Recorded your rating{who}: {label}. Rating again asks before replacing it."
                )
            }
        }
    },
    rated_public: |label, category, as_judge| {
        let who = if as_judge { " as a judge ruling" } else { "" };
        let category = category.map(|c| format!(" ({c})")).unwrap_or_default();
        format!(
            "Recorded your rating{who}: {label}{category}. The operator sees your reason beside the \
answer, and it is shown on the answer's public web page, without your name. Rating again asks before \
replacing it."
        )
    },
    report_wrong: |username| format!("`@{username}` runs this bot and reads every reason given."),
    report_link: |url| {
        format!(
            " If the rules themselves are answered wrong, you can also [report it to the project](<{url}>)."
        )
    },
    rate_failed: "Sorry, I couldn't record that rating. Please try again.",
    call_gone: "This answer is no longer stored, so it can't be rated.",
    already_rated: "You already rated this answer **Correct**. Nothing changed.",
    replace_prompt: |earlier, category, new| {
        let category = category.map(|c| format!(" ({c})")).unwrap_or_default();
        format!("You already rated this answer **{earlier}**{category}. Replace it with **{new}**?")
    },
    replace_label: "Replace",
    keep_label: "Keep",
    on_record: |label, category| {
        let category = category.map(|c| format!(" ({c})")).unwrap_or_default();
        format!("Nothing changed. Your rating on record: **{label}**{category}.")
    },
    no_rating: "Nothing changed. You have no rating on this answer.",
    category_label: |category| match category {
        FeedbackCategory::WrongConclusion => "Wrong conclusion",
        FeedbackCategory::MissedPart => "Missed part of the question",
        FeedbackCategory::BadCitation => "A citation doesn't support it",
        FeedbackCategory::WrongCard => "Wrong card",
        FeedbackCategory::MisreadQuestion => "Misread the question",
        FeedbackCategory::WrongRefusal => "Shouldn't have refused",
        FeedbackCategory::Outdated => "Outdated",
        FeedbackCategory::Other => "Other",
    },
    form_title: |fault| match fault {
        Fault::Incorrect => "Incorrect: what went wrong?",
        Fault::Partial => "Partially correct: what's missing?",
    },
    form_category: "What kind of problem?",
    form_category_placeholder: "Choose one",
    form_wrong: "What's wrong",
    form_wrong_hint: "Optional. What did the answer get wrong?",
    form_wrong_hint_public: "Optional. Shown on the public web page, without your name.",
    form_right: "The right answer",
    form_right_hint: "Optional. What should it have said?",
    form_right_hint_public: "Optional. Also shown publicly, without your name.",
    category_prompt: |label| {
        format!(
            "You rated this answer **{label}**. What kind of problem is it? Choose one, then add \
details in the form."
        )
    },
    form_incomplete: "That form came back without a category, so nothing was recorded. Please rate the \
answer again.",
    form_bad_text: "A field held a character that isn't text, so nothing was recorded. Please rate the \
answer again.",
    form_too_long: |limit| {
        format!(
            "Each field takes at most {limit} characters, so nothing was recorded. Please rate the \
answer again."
        )
    },
    retry_body: "Judgebot is being upgraded and will be back in a minute or two. \
Press **Retry** to ask this again.",
    retry_label: "Retry",
    still_upgrading: "Still upgrading. Press **Retry** again in a minute; your question is kept on the message.",
    edit_failed: "I worked out an answer but couldn't update the message. Please ask the question again.",

    footer_meta: |confidence, cr| {
        let level = match confidence {
            Confidence::Low => "Low",
            Confidence::Medium => "Medium",
            Confidence::High => "High",
        };
        format!("Confidence: {level} · CR {cr}")
    },
    cards_line: |names| format!("Cards: {names}"),
    ruling_label: |date, name| match name {
        Some(name) => format!("Ruling{date} — {name}"),
        None => format!("Scryfall ruling{date}"),
    },
    oracle_label: |face, name| {
        let face_note = face.map(|n| format!(", face {n}")).unwrap_or_default();
        match name {
            Some(name) => format!("Oracle text{face_note} — {name}"),
            None => format!("Oracle text{face_note}"),
        }
    },
    prior_call_label: "prior call",
    quote_open: "“",
    quote_close: "”",

    unknown_command: "I don't recognise that command in this version. Please try again in a minute.",
    server_only: "`/judge` works in a server only, not in direct messages.",
    cooling_down: |max, window, wait| {
        format!(
            "You've asked {max} questions in the last {}, which is this bot's limit per person. Try \
again in {}. `/card` and `/rule` lookups are not limited.",
            en_span(window),
            en_span(wait)
        )
    },
    help_body: "**MTG Judgebot** answers Magic: The Gathering rules questions like a judge: every \
claim carries a citation to the Comprehensive Rules, an official ruling, or a card's Oracle text, and a \
citation is only shown after it has been checked against the source.\n\n\
**Asking.** `/judge question: <your question>`. Nicknames work (\"bob\", \"goyf\"). Use brackets like \
`[[Full Card Name]]` to avoid ambiguity: a bracketed name matches only the card with exactly that name, or the \
one card a listed nickname stands for. If a \
name could mean several cards you get a \"did you mean…?\" row instead of a guess, and every answer lists \
the cards it took your question to be about. Tournament policy and card prices are out of scope. Add \
`private: True` and only you see the answer. It reads no earlier questions and no later question reads \
it. It is still stored, for the operator only.\n\n\
**Looking things up.** `/card` shows a card's Oracle text and rulings and `/rule` shows rules text by \
number. Both are free and instant.\n\n\
**Rating.** The buttons under an answer record how right it was. *Partially correct* and *Incorrect* \
open a short form (what kind of problem, what's wrong, the right answer) that goes to the operator. Rating \
again asks before it replaces yours. Ratings decide which past answers are shown as examples later — the \
rules themselves always outrank them. Members with the server's judge role rate with an override.",
    help_stored: "**What is stored.** Each question and answer, the channel it was asked in, your user id and username, \
and your ratings with the reasons you give. A call that errored is kept for debugging, with its question. \
Nothing else is read: the bot sees only its slash commands. `/forget` deletes your ratings and their \
reasons and removes your id and username from your questions; a private question's text, and a failed \
one's, is removed too.",
    help_feed: "**The feed.** A question not asked private may be listed with its answer on this \
instance's public web feed, with your username (anonymous once you use `/forget`). The channel and server \
are never shown.",
    help_disclaimer: "Answers are AI-generated; verify anything important with a human judge. Unofficial \
Fan Content under the Fan Content Policy, not endorsed by Wizards of the Coast. Card data from Scryfall.",
    data_heading: "**Data.**",
    data_unknown: "**Data.** Unknown: the database did not answer.",
    contact: |username, email| {
        let email = email.map(|e| format!(" or `{e}`")).unwrap_or_default();
        format!(
            "**Who runs this bot.** `@{username}` on Discord{email}. Tell them about problems with this \
instance."
        )
    },
    license_heading: "**Source and licence.**",
    license: |l| {
        format!(
            "{} — {}. Free software under the [{}](<{}>): you may run, study, \
share and modify it, and anyone offering a modified version over a network must offer its source under \
the same licence. The source code of this instance is at <{}> ({}).",
            l.program, l.copyright, l.license_name, l.license_url, l.repository, l.revision
        )
    },
    forgotten: en_forgotten,
    forget_failed: "Sorry, I couldn't forget you just now. Please try again.",
    lookup_bad_name: "Give me a card name or nickname (at most 200 characters).",
    lookup_bad_rule: "That doesn't look like a rule number. Try `702.19`, `702.19b`, or `702` for a whole section.",
    lookup_failed: "Sorry, I couldn't look that up just now. Please try again.",
    no_rulings: "No rulings.",
    rulings_heading: |n| format!("**Rulings ({n})**"),
    card_ambiguous_head: |name| format!("\"{name}\" could be several cards:"),
    card_ambiguous_tail: "Ask again with the full name.",
    card_not_found: |name| {
        format!(
            "I couldn't find a card called \"{name}\". Check the spelling, or use the full name."
        )
    },
    rule_title: |asked, heading| match heading {
        Some(heading) => format!("CR {asked} — {heading}"),
        None => format!("CR {asked}"),
    },
    cr_label: |date| format!("CR {date}"),
    long_rule_note: "\n\nThis rule is longer than Discord shows. Ask for one sub-rule (add its letter), or follow the title link.",
    rule_not_found: |id| format!("There is no rule {id} in the Comprehensive Rules I have loaded."),
    more_lines: |n| format!("… and {n} more"),

    commands: Commands {
        judge: "Ask a Magic rules question; the answer cites the Comprehensive Rules.",
        judge_question: "Your rules question. Use brackets like [[Full Card Name]] to avoid ambiguity.",
        judge_private: "Show the answer only to you. It reads no earlier questions here and is never shown publicly.",
        card: "Look up a card: Oracle text and rulings. Free: no model is called.",
        card_name: "A card name or nickname.",
        card_private: "Show the card only to you.",
        rule: "Look up Comprehensive Rules text by number. Free: no model is called.",
        rule_id: "A rule number such as 702.19, 702.19b or 702.",
        rule_private: "Show the rule only to you.",
        help: "What the bot does, how to ask, what it stores, and where its source is.",
        license: "Where this bot's source code is, at which commit, and under which licence.",
        forget: "Delete your ratings and anonymize your failed calls; nothing else is stored about you.",
    },
};

/// The stub locale's (`judge_core::locale`, tests only): each message its
/// name in `⟦…⟧`, with its values. The pieces a prompt is parsed back from
/// keep English's shape (a space each side of `asked`, the span in bold),
/// so the stub's prompts parse like English ones.
pub const STUB: Messages = Messages {
    busy: "⟦busy⟧",
    out_of_scope: "⟦out_of_scope⟧",
    failed: "⟦failed⟧",
    spend_cap: "⟦spend_cap⟧",
    cards_not_found: |listed| format!("⟦cards_not_found {listed}⟧"),
    or: " ⟦or⟧ ",
    did_you_mean_head: |span| format!("⟦did_you_mean_head **{span}**⟧"),
    did_you_mean_more: |n| format!("⟦did_you_mean_more {n}⟧"),
    did_you_mean_tail: "⟦did_you_mean_tail⟧",
    maintenance: "⟦maintenance⟧",

    asked: " ⟦asked⟧ ",
    pick_head: "⟦pick_head⟧ **",
    pick_ask: "** ⟦pick_ask⟧",
    pick_more: "⟦pick_more⟧",
    pick_later_one: "⟦pick_later_one⟧",
    pick_later_head: "⟦pick_later_head⟧ ",
    pick_later_tail: " ⟦pick_later_tail⟧",

    expired: "⟦expired⟧",
    already_picked: "⟦already_picked⟧",
    pick_failed: "⟦pick_failed⟧",
    not_yours: "⟦not_yours⟧",
    not_your_retry: "⟦not_your_retry⟧",
    already_retried: "⟦already_retried⟧",
    retrying: "⟦retrying⟧",
    working: |card| format!("⟦working **{card}**⟧"),
    unknown_button: "⟦unknown_button⟧",
    rating_label: |score| match score {
        Score::Incorrect => "⟦rating_label incorrect⟧",
        Score::Partial => "⟦rating_label partial⟧",
        Score::Correct => "⟦rating_label correct⟧",
    },
    rated: |label, category, as_judge| {
        format!(
            "⟦rated {label} {} {as_judge}⟧",
            category.unwrap_or_default()
        )
    },
    rated_public: |label, category, as_judge| {
        format!(
            "⟦rated_public {label} {} {as_judge}⟧",
            category.unwrap_or_default()
        )
    },
    report_wrong: |username| format!("⟦report_wrong {username}⟧"),
    report_link: |url| format!(" ⟦report_link <{url}>⟧"),
    rate_failed: "⟦rate_failed⟧",
    call_gone: "⟦call_gone⟧",
    already_rated: "⟦already_rated⟧",
    replace_prompt: |earlier, category, new| {
        format!(
            "⟦replace_prompt {earlier} {} {new}⟧",
            category.unwrap_or_default()
        )
    },
    replace_label: "⟦replace_label⟧",
    keep_label: "⟦keep_label⟧",
    on_record: |label, category| format!("⟦on_record {label} {}⟧", category.unwrap_or_default()),
    no_rating: "⟦no_rating⟧",
    category_label: |category| match category {
        FeedbackCategory::WrongConclusion => "⟦category_label wrong_conclusion⟧",
        FeedbackCategory::MissedPart => "⟦category_label missed_part⟧",
        FeedbackCategory::BadCitation => "⟦category_label bad_citation⟧",
        FeedbackCategory::WrongCard => "⟦category_label wrong_card⟧",
        FeedbackCategory::MisreadQuestion => "⟦category_label misread_question⟧",
        FeedbackCategory::WrongRefusal => "⟦category_label wrong_refusal⟧",
        FeedbackCategory::Outdated => "⟦category_label outdated⟧",
        FeedbackCategory::Other => "⟦category_label other⟧",
    },
    form_title: |fault| match fault {
        Fault::Incorrect => "⟦form_title incorrect⟧",
        Fault::Partial => "⟦form_title partial⟧",
    },
    form_category: "⟦form_category⟧",
    form_category_placeholder: "⟦form_category_placeholder⟧",
    form_wrong: "⟦form_wrong⟧",
    form_wrong_hint: "⟦form_wrong_hint⟧",
    form_wrong_hint_public: "⟦form_wrong_hint_public⟧",
    form_right: "⟦form_right⟧",
    form_right_hint: "⟦form_right_hint⟧",
    form_right_hint_public: "⟦form_right_hint_public⟧",
    category_prompt: |label| format!("⟦category_prompt {label}⟧"),
    form_incomplete: "⟦form_incomplete⟧",
    form_bad_text: "⟦form_bad_text⟧",
    form_too_long: |limit| format!("⟦form_too_long {limit}⟧"),
    retry_body: "⟦retry_body⟧",
    retry_label: "⟦retry_label⟧",
    still_upgrading: "⟦still_upgrading⟧",
    edit_failed: "⟦edit_failed⟧",

    footer_meta: |confidence, cr| format!("⟦footer_meta {confidence:?} {cr}⟧"),
    cards_line: |names| format!("⟦cards_line {names}⟧"),
    ruling_label: |date, name| format!("⟦ruling_label{date} {}⟧", name.unwrap_or_default()),
    oracle_label: |face, name| {
        format!(
            "⟦oracle_label {} {}⟧",
            face.unwrap_or_default(),
            name.unwrap_or_default()
        )
    },
    prior_call_label: "⟦prior_call_label⟧",
    quote_open: "⟦quote_open⟧",
    quote_close: "⟦quote_close⟧",

    unknown_command: "⟦unknown_command⟧",
    server_only: "⟦server_only⟧",
    cooling_down: |max, window, wait| {
        format!(
            "⟦cooling_down {max} {} {}⟧",
            window.as_secs(),
            wait.as_secs()
        )
    },
    help_body: "⟦help_body⟧",
    help_stored: "⟦help_stored⟧",
    help_feed: "⟦help_feed⟧",
    help_disclaimer: "⟦help_disclaimer⟧",
    data_heading: "⟦data_heading⟧",
    data_unknown: "⟦data_unknown⟧",
    contact: |username, email| format!("⟦contact {username} {}⟧", email.unwrap_or_default()),
    license_heading: "⟦license_heading⟧",
    license: |l| {
        format!(
            "⟦license {} {} {} <{}> <{}> {}⟧",
            l.program, l.copyright, l.license_name, l.license_url, l.repository, l.revision
        )
    },
    forgotten: |f| {
        format!(
            "⟦forgotten {} {} {} {}⟧",
            f.ratings, f.feedback, f.calls, f.failures
        )
    },
    forget_failed: "⟦forget_failed⟧",
    lookup_bad_name: "⟦lookup_bad_name⟧",
    lookup_bad_rule: "⟦lookup_bad_rule⟧",
    lookup_failed: "⟦lookup_failed⟧",
    no_rulings: "⟦no_rulings⟧",
    rulings_heading: |n| format!("⟦rulings_heading {n}⟧"),
    card_ambiguous_head: |name| format!("⟦card_ambiguous_head {name}⟧"),
    card_ambiguous_tail: "⟦card_ambiguous_tail⟧",
    card_not_found: |name| format!("⟦card_not_found {name}⟧"),
    rule_title: |asked, heading| format!("⟦rule_title {asked} {}⟧", heading.unwrap_or_default()),
    cr_label: |date| format!("⟦cr_label {date}⟧"),
    long_rule_note: "\n\n⟦long_rule_note⟧",
    rule_not_found: |id| format!("⟦rule_not_found {id}⟧"),
    more_lines: |n| format!("⟦more_lines {n}⟧"),

    commands: Commands {
        judge: "⟦commands.judge⟧",
        judge_question: "⟦commands.judge_question⟧",
        judge_private: "⟦commands.judge_private⟧",
        card: "⟦commands.card⟧",
        card_name: "⟦commands.card_name⟧",
        card_private: "⟦commands.card_private⟧",
        rule: "⟦commands.rule⟧",
        rule_id: "⟦commands.rule_id⟧",
        rule_private: "⟦commands.rule_private⟧",
        help: "⟦commands.help⟧",
        license: "⟦commands.license⟧",
        forget: "⟦commands.forget⟧",
    },
};

/// A duration as an English reader says it: `45 seconds`, `1 minute`, `10
/// minutes`. Rounded up, so "try again in" never sends someone back too
/// early.
fn en_span(d: Duration) -> String {
    let secs = d
        .as_secs()
        .saturating_add(u64::from(d.subsec_nanos() > 0))
        .max(1);
    let (n, unit) = if secs < 60 {
        (secs, "second")
    } else {
        (secs.div_ceil(60), "minute")
    };
    format!("{n} {unit}{}", if n == 1 { "" } else { "s" })
}

/// `/forget`'s confirmation in English, naming how much there was to forget.
fn en_forgotten(f: Forgotten) -> String {
    let count = |n: u64, one: &str, many: &str| match n {
        1 => format!("1 {one}"),
        n => format!("{n} {many}"),
    };
    let mut done = Vec::new();
    if f.ratings > 0 {
        let reasons = if f.feedback > 0 {
            " with their reasons"
        } else {
            ""
        };
        done.push(format!(
            "deleted your {}{reasons}",
            count(f.ratings, "rating", "ratings")
        ));
    }
    if f.calls > 0 {
        done.push(format!(
            "removed your id and username from {} you asked",
            count(f.calls, "question", "questions")
        ));
    }
    if f.failures > 0 {
        done.push(format!(
            "anonymized {} of yours",
            count(f.failures, "failed call", "failed calls")
        ));
    }
    let Some((last, rest)) = done.split_last() else {
        return "You had no ratings, questions or failed calls on record; there is nothing else \
                stored about you."
            .to_owned();
    };
    let joined = if rest.is_empty() {
        last.clone()
    } else {
        format!("{} and {last}", rest.join(", "))
    };
    let mut chars = joined.chars();
    let capitalized: String = chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default();
    format!("{capitalized}. Nothing else is stored about you.")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Discord's limit on a command or option description, in characters.
    const DESCRIPTION_LIMIT: usize = 100;

    /// Every locale renders every message, and its command descriptions fit.
    #[test]
    fn every_locale_renders_every_message() {
        for l in Locale::ALL {
            let m = messages(l);
            let texts = [
                m.busy.to_owned(),
                m.out_of_scope.to_owned(),
                m.failed.to_owned(),
                m.spend_cap.to_owned(),
                (m.cards_not_found)("**X**"),
                (m.did_you_mean_head)("bob"),
                (m.did_you_mean_more)(3),
                m.did_you_mean_tail.to_owned(),
                m.maintenance.to_owned(),
                m.pick_head.to_owned(),
                m.pick_ask.to_owned(),
                m.pick_more.to_owned(),
                m.pick_later_one.to_owned(),
                m.expired.to_owned(),
                m.already_picked.to_owned(),
                m.pick_failed.to_owned(),
                m.not_yours.to_owned(),
                m.not_your_retry.to_owned(),
                m.already_retried.to_owned(),
                m.retrying.to_owned(),
                (m.working)("Card"),
                m.unknown_button.to_owned(),
                (m.rating_label)(Score::Partial).to_owned(),
                (m.rated)("Correct", None, true),
                (m.rated)("Incorrect", Some("Wrong card"), false),
                (m.rated_public)("Incorrect", Some("Wrong card"), false),
                m.already_rated.to_owned(),
                (m.replace_prompt)("Partially correct", Some("Wrong card"), "Incorrect"),
                m.replace_label.to_owned(),
                m.keep_label.to_owned(),
                (m.on_record)("Incorrect", Some("Wrong card")),
                m.no_rating.to_owned(),
                (m.category_prompt)("Incorrect"),
                m.form_incomplete.to_owned(),
                (m.form_too_long)(1000),
                m.form_bad_text.to_owned(),
                (m.report_wrong)("op"),
                (m.report_link)("https://example.com"),
                m.rate_failed.to_owned(),
                m.call_gone.to_owned(),
                m.retry_body.to_owned(),
                m.retry_label.to_owned(),
                m.still_upgrading.to_owned(),
                m.edit_failed.to_owned(),
                (m.footer_meta)(Confidence::High, "2026-08-19"),
                (m.cards_line)("A · B"),
                (m.ruling_label)(" (2018-07-13)", Some("Blood Moon")),
                (m.ruling_label)("", None),
                (m.oracle_label)(Some(2), None),
                m.prior_call_label.to_owned(),
                m.quote_open.to_owned(),
                m.quote_close.to_owned(),
                m.unknown_command.to_owned(),
                m.server_only.to_owned(),
                (m.cooling_down)(6, Duration::from_mins(10), Duration::from_secs(1)),
                m.help_body.to_owned(),
                m.help_stored.to_owned(),
                m.help_feed.to_owned(),
                m.help_disclaimer.to_owned(),
                m.data_heading.to_owned(),
                m.data_unknown.to_owned(),
                (m.contact)("op", Some("a@b.c")),
                m.license_heading.to_owned(),
                m.forget_failed.to_owned(),
                (m.forgotten)(Forgotten::default()),
                m.lookup_bad_name.to_owned(),
                m.lookup_bad_rule.to_owned(),
                m.lookup_failed.to_owned(),
                m.no_rulings.to_owned(),
                (m.rulings_heading)(2),
                (m.card_ambiguous_head)("bob"),
                m.card_ambiguous_tail.to_owned(),
                (m.card_not_found)("bob"),
                (m.rule_title)("702", None),
                (m.cr_label)("2026-08-19"),
                m.long_rule_note.to_owned(),
                (m.rule_not_found)("999"),
                (m.more_lines)(3),
            ];
            for t in &texts {
                assert!(!t.trim().is_empty(), "{l}: an empty message");
            }
            let c = m.commands;
            for d in [
                c.judge,
                c.judge_question,
                c.judge_private,
                c.card,
                c.card_name,
                c.card_private,
                c.rule,
                c.rule_id,
                c.rule_private,
                c.help,
                c.license,
                c.forget,
            ] {
                let n = d.chars().count();
                assert!(
                    (1..=DESCRIPTION_LIMIT).contains(&n),
                    "{l}: {d:?} is {n} characters"
                );
            }
        }
    }

    /// The form's pieces fit Discord's limits in every locale: a modal
    /// over one is refused whole.
    #[test]
    fn the_rating_form_fits_discords_limits() {
        let fits = |text: &str, limit: usize| {
            let n = text.chars().count();
            (1..=limit).contains(&n)
        };
        for l in Locale::ALL {
            let m = messages(l);
            for f in [Fault::Partial, Fault::Incorrect] {
                assert!(fits((m.form_title)(f), 45), "{l}: title");
            }
            for label in [m.form_category, m.form_wrong, m.form_right] {
                assert!(fits(label, 45), "{l}: {label:?}");
            }
            for hint in [
                m.form_category_placeholder,
                m.form_wrong_hint,
                m.form_right_hint,
                m.form_wrong_hint_public,
                m.form_right_hint_public,
            ] {
                assert!(fits(hint, 100), "{l}: {hint:?}");
            }
            for c in FeedbackCategory::ALL {
                assert!(fits((m.category_label)(c), 100), "{l}: {c}");
            }
            for label in [m.replace_label, m.keep_label] {
                assert!(fits(label, 80), "{l}: {label:?}");
            }
        }
    }

    #[test]
    fn english_spans_round_up_and_count_in_whole_units() {
        assert_eq!(en_span(Duration::from_millis(1)), "1 second");
        assert_eq!(en_span(Duration::from_secs(45)), "45 seconds");
        assert_eq!(en_span(Duration::from_secs(60)), "1 minute");
        assert_eq!(en_span(Duration::from_secs(61)), "2 minutes");
    }
}
