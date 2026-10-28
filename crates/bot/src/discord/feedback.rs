//! The rating form (D38): the modal *Partially correct* or *Incorrect* opens,
//! and the reason a submitted one carries.
//!
//! The form holds the category (a required menu) and two optional texts,
//! "What's wrong" and "The right answer". Discord takes a menu in a modal
//! only inside a Label component (type 18): [`modal`] writes it
//! ([`Layout::Labels`]), and [`fields`] reads the submitted values from the
//! interaction's components as Discord sent them, however it was delivered.
//!
//! Builds before D43 sent the gateway's raters a private category menu
//! first (their Discord library read a Label back as an empty row), with
//! the category in the form's id ([`FormId::category`]) and the texts in
//! action rows ([`Layout::Rows`]). A menu still showing on a private message
//! opens that form, and [`fields`] reads both shapes.

use judge_core::{Fault, FeedbackCategory, FeedbackText, Locale, Reason, feedback::BadText};
use serde_json::{Value, json};

use super::{catalog, ids::FormId};

/// The category menu's `custom_id` inside the form.
pub const CATEGORY_FIELD: &str = "category";
/// "What's wrong"'s `custom_id`.
pub const WRONG_FIELD: &str = "wrong";
/// "The right answer"'s `custom_id`.
pub const RIGHT_FIELD: &str = "right";

/// Interaction response type: open a modal.
const MODAL: u8 = 9;
/// Component types (docs.discord.com, component reference).
const ACTION_ROW: u8 = 1;
const STRING_SELECT: u8 = 3;
const TEXT_INPUT: u8 = 4;
const LABEL: u8 = 18;
/// Text input style: several lines.
const PARAGRAPH: u8 = 2;

/// How a form lays out its fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// The category menu and both texts, each in a Label: the form.
    Labels,
    /// The two texts in action rows, the category already chosen from an
    /// earlier build's menu.
    Rows,
}

impl Layout {
    /// The layout `form` takes: the menu is in the form unless its
    /// category was chosen before it opened.
    #[must_use]
    pub const fn of(form: FormId) -> Self {
        match form.category {
            None => Self::Labels,
            Some(_) => Self::Rows,
        }
    }
}

/// The interaction response that opens the form `form` in `locale`; with
/// `public` (`JUDGE_PUBLIC_FEEDBACK`) its hints say the texts are shown on
/// the public web page, without the rater's name.
#[must_use]
pub fn modal(locale: Locale, form: FormId, public: bool) -> Value {
    let m = catalog::messages(locale);
    let (wrong_hint, right_hint) = if public {
        (m.form_wrong_hint_public, m.form_right_hint_public)
    } else {
        (m.form_wrong_hint, m.form_right_hint)
    };
    let max = judge_core::feedback::FEEDBACK_TEXT_MAX_CHARS;
    let text = |field: &str, label: &str, hint: &str, layout: Layout| match layout {
        Layout::Labels => json!({
            "type": LABEL,
            "label": label,
            "description": hint,
            "component": {
                "type": TEXT_INPUT,
                "custom_id": field,
                "style": PARAGRAPH,
                "required": false,
                "max_length": max,
            },
        }),
        Layout::Rows => json!({
            "type": ACTION_ROW,
            "components": [{
                "type": TEXT_INPUT,
                "custom_id": field,
                "style": PARAGRAPH,
                "label": label,
                "placeholder": hint,
                "required": false,
                "max_length": max,
            }],
        }),
    };
    let layout = Layout::of(form);
    let mut components = Vec::with_capacity(3);
    if layout == Layout::Labels {
        let options: Vec<Value> = FeedbackCategory::ALL
            .into_iter()
            .map(|c| json!({ "label": (m.category_label)(c), "value": c.id() }))
            .collect();
        components.push(json!({
            "type": LABEL,
            "label": m.form_category,
            "component": {
                "type": STRING_SELECT,
                "custom_id": CATEGORY_FIELD,
                "placeholder": m.form_category_placeholder,
                "required": true,
                "min_values": 1,
                "max_values": 1,
                "options": options,
            },
        }));
    }
    components.push(text(WRONG_FIELD, m.form_wrong, wrong_hint, layout));
    components.push(text(RIGHT_FIELD, m.form_right, right_hint, layout));
    json!({
        "type": MODAL,
        "data": {
            "custom_id": form.to_custom_id(),
            "title": (m.form_title)(form.fault),
            "components": components,
        },
    })
}

/// What a submitted form held, by field; absent when the field was not in
/// it (or its value could not be read).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Fields {
    /// The category menu's chosen value.
    pub category: Option<String>,
    /// "What's wrong".
    pub wrong: Option<String>,
    /// "The right answer".
    pub right: Option<String>,
}

/// The values in a modal submission's `data.components`, whether each
/// field sits in a Label (`{"type":18,"component":{…}}`) or an action row
/// (`{"type":1,"components":[…]}`). A text input's is its `value`, a
/// menu's its first `values` entry.
#[must_use]
pub fn fields(components: &Value) -> Fields {
    let mut out = Fields::default();
    let mut visit = |c: &Value| {
        let Some(id) = c.get("custom_id").and_then(Value::as_str) else {
            return;
        };
        let value = c.get("value").and_then(Value::as_str).or_else(|| {
            c.get("values")
                .and_then(Value::as_array)
                .and_then(|v| v.first())
                .and_then(Value::as_str)
        });
        let Some(value) = value else { return };
        let slot = match id {
            CATEGORY_FIELD => &mut out.category,
            WRONG_FIELD => &mut out.wrong,
            RIGHT_FIELD => &mut out.right,
            _ => return,
        };
        *slot = Some(value.to_owned());
    };
    for c in components.as_array().into_iter().flatten() {
        if let Some(inner) = c.get("component") {
            visit(inner);
        } else if let Some(row) = c.get("components").and_then(Value::as_array) {
            row.iter().for_each(&mut visit);
        } else {
            visit(c);
        }
    }
    out
}

/// Why a submitted form records nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormProblem {
    /// No category came back: neither in the form's id nor in its menu (a
    /// gateway delivery of a form built for HTTP, after the operator
    /// switched how interactions arrive).
    NoCategory,
    /// A text is over its limit, or holds a control character.
    BadText(BadText),
}

/// The reason a submitted form gives: the category from its id when it was
/// chosen beforehand, else from its menu, and the two texts, trimmed, a
/// blank one absent.
///
/// # Errors
/// [`FormProblem`].
pub fn reason(form: FormId, fields: &Fields) -> Result<Reason, FormProblem> {
    let category = form
        .category
        .or_else(|| fields.category.as_deref().and_then(FeedbackCategory::parse))
        .ok_or(FormProblem::NoCategory)?;
    let text = |t: &Option<String>| {
        t.as_deref()
            .map_or(Ok(None), FeedbackText::optional)
            .map_err(FormProblem::BadText)
    };
    Ok(Reason {
        category,
        wrong: text(&fields.wrong)?,
        right: text(&fields.right)?,
    })
}

/// The fault's form id with no category yet (the one-form flow).
#[must_use]
pub const fn form(call: judge_core::CallId, fault: Fault) -> FormId {
    FormId {
        call,
        fault,
        category: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use judge_core::CallId;
    use uuid::Uuid;

    fn fixture(name: &str) -> Result<Value, Box<dyn std::error::Error>> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/discord")
            .join(name);
        Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
    }

    fn nil() -> CallId {
        CallId::new(Uuid::nil())
    }

    /// The one-form modal, byte for byte: a Label per field, the menu
    /// listing every category, as Discord's component reference has them.
    #[test]
    fn the_one_form_modal_is_pinned() -> Result<(), Box<dyn std::error::Error>> {
        let got = modal(Locale::En, form(nil(), Fault::Incorrect), false);
        assert_eq!(got, fixture("rating_form_labels.json")?);
        // With reasons public, the hints say so.
        let public = modal(Locale::En, form(nil(), Fault::Incorrect), true).to_string();
        assert!(public.contains("Shown on the public web page"), "{public}");
        Ok(())
    }

    /// The gateway's form: the category already in its id, the texts in
    /// action rows.
    #[test]
    fn the_gateway_form_is_pinned() -> Result<(), Box<dyn std::error::Error>> {
        let id = FormId {
            call: nil(),
            fault: Fault::Partial,
            category: Some(FeedbackCategory::WrongCard),
        };
        assert_eq!(Layout::of(id), Layout::Rows);
        assert_eq!(
            modal(Locale::En, id, false),
            fixture("rating_form_rows.json")?
        );
        Ok(())
    }

    /// A submitted modal, read as the bot reads any interaction.
    fn submission(
        name: &str,
    ) -> Result<super::super::wire::ModalInteraction, Box<dyn std::error::Error>> {
        match super::super::wire::Interaction::from_value(fixture(name)?)? {
            super::super::wire::Interaction::Modal(m) => Ok(m),
            _ => Err("not a modal submit".into()),
        }
    }

    /// A submission in Discord's documented Label shape: its values arrive
    /// whole.
    #[test]
    fn a_label_submission_is_read_whole() -> Result<(), Box<dyn std::error::Error>> {
        let m = submission("rating_form_submit.json")?;
        let f = fields(&m.data.components);
        assert_eq!(f.category.as_deref(), Some("wrong_card"));
        assert_eq!(
            f.wrong.as_deref(),
            Some("  It read Bob as the wrong card.  ")
        );
        assert_eq!(f.right.as_deref(), Some(""));
        let form = FormId::parse(&m.data.custom_id)?;
        let r = reason(form, &f).map_err(|e| format!("{e:?}"))?;
        assert_eq!(r.category, FeedbackCategory::WrongCard);
        assert_eq!(
            r.wrong.map(|t| t.to_string()).as_deref(),
            Some("It read Bob as the wrong card.")
        );
        assert_eq!(r.right, None);
        Ok(())
    }

    /// The rows form (its category chosen before it opened) reads too; its
    /// category is the id's.
    #[test]
    fn a_rows_submission_is_read() -> Result<(), Box<dyn std::error::Error>> {
        let m = submission("rating_form_rows_submit.json")?;
        let form = FormId::parse(&m.data.custom_id)?;
        let f = fields(&m.data.components);
        assert_eq!(f.category, None);
        let r = reason(form, &f).map_err(|e| format!("{e:?}"))?;
        assert_eq!(r.category, FeedbackCategory::WrongCard);
        assert_eq!(r.wrong, None);
        assert_eq!(
            r.right.map(|t| t.to_string()).as_deref(),
            Some("Bob is legendary.")
        );
        Ok(())
    }

    #[test]
    fn a_form_without_its_category_or_over_its_limit_records_nothing() {
        let id = form(nil(), Fault::Partial);
        assert_eq!(reason(id, &Fields::default()), Err(FormProblem::NoCategory));
        let typo = Fields {
            category: Some("typo".into()),
            ..Fields::default()
        };
        assert_eq!(reason(id, &typo), Err(FormProblem::NoCategory));
        let long = Fields {
            category: Some("other".into()),
            right: Some("x".repeat(1001)),
            ..Fields::default()
        };
        assert!(matches!(
            reason(id, &long),
            Err(FormProblem::BadText(BadText::TooLong { .. }))
        ));
        let nul = Fields {
            category: Some("other".into()),
            wrong: Some("bad\u{0}byte".into()),
            ..Fields::default()
        };
        assert_eq!(
            reason(id, &nul),
            Err(FormProblem::BadText(BadText::Control))
        );
    }
}
