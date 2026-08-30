//! `Synthesizer` adapter over `judge_anthropic::Synth`.

use std::{fmt::Write as _, sync::Arc};

use async_trait::async_trait;
use judge_anthropic::{Client, SendOutcome, Synth, SynthConfig};
use judge_core::{Citation, Context, JudgeError, Question, Retriever, Synthesizer, Unvalidated, Verdict};

pub struct AnthropicSynthesizer {
    pub client: Client,
    pub cfg: SynthConfig,
    pub retriever: Arc<dyn Retriever>,
    pub system_prompt: String,
}

#[async_trait]
impl Synthesizer for AnthropicSynthesizer {
    async fn answer(
        &self,
        q: &Question,
        ctx: &mut Context,
        rejected: Option<&Citation>,
    ) -> Result<Verdict<Unvalidated>, JudgeError> {
        let user = render_user_turn(q, ctx, rejected);
        let synth = Synth::new(self.client.clone(), &self.cfg, self.system_prompt.clone(), user);
        match synth.send().await? {
            SendOutcome::Done(v) => Ok(v),
            SendOutcome::ToolRequested(t) => {
                let chunks = self.retriever.lookup_rules(t.requested()).await?;
                // Borrow for the tool result first, then move the chunks into Context.
                let f = t.answer_tool(&chunks);
                ctx.extend_rules(chunks);
                f.finish().await
            }
        }
    }
}

fn render_user_turn(q: &Question, ctx: &Context, rejected: Option<&Citation>) -> String {
    let mut s = String::new();
    for c in &ctx.cards {
        let _ = writeln!(s, "## Card: {} ({:?})", c.name, c.layout);
        for f in &c.faces {
            let _ = writeln!(s, "### {} — {} — {}\n{}", f.name, f.mana_cost, f.type_line, f.oracle_text);
        }
    }
    match ctx.cr_version() {
        Some(v) => {
            let _ = writeln!(s, "\n## Comprehensive Rules (effective {v})");
        }
        None => s.push_str("\n## Comprehensive Rules\n"),
    }
    for r in &ctx.rules {
        let _ = writeln!(s, "[{}] {}\n{}", r.id, r.heading, r.body);
        for e in &r.examples {
            s.push_str(e);
            s.push('\n');
        }
    }
    if !ctx.rulings.is_empty() {
        s.push_str("\n## Scryfall rulings\n");
        for r in &ctx.rulings {
            let _ = writeln!(s, "[{}#{}] ({}) {}", r.card, r.idx, r.published_at, r.text);
        }
    }
    if !ctx.glossary.is_empty() {
        s.push_str("\n## Glossary\n");
        for g in &ctx.glossary {
            let _ = writeln!(s, "{}: {}", g.term, g.text);
        }
    }
    if !ctx.notes.is_empty() {
        s.push_str("\n## Notes on tricky cards\n");
        for n in &ctx.notes {
            let _ = writeln!(s, "[{}] {}", n.card, n.note);
        }
    }
    if !ctx.prior.is_empty() {
        s.push_str("\n## Prior calls (examples only; the CR always outranks these)\n");
        for p in &ctx.prior {
            let _ = write!(
                s,
                "[call {}] rating {:.1}/3 ({} votes), CR {}\nQ: {}\nA: {}\n",
                p.id, p.rating, p.rating_count, p.cr_version, p.question, p.answer
            );
        }
    }
    if !ctx.history.is_empty() {
        s.push_str("\n## Earlier in this thread\n");
        for h in &ctx.history {
            let _ = writeln!(s, "Q: {}\nA: {}", h.question, h.answer);
        }
    }
    if let Some(c) = rejected {
        let _ = writeln!(
            s,
            "\n## Previous attempt rejected\nYour earlier answer cited {c}, but that quote is not a verbatim \
             span of that source in the material above (or the source is not present). Cite only material \
             shown above and copy quotes exactly."
        );
    }
    let _ = writeln!(s, "\n## Question\n{}", q.text);
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use judge_core::{CrVersion, Qa, RuleChunk, RuleId};

    fn ctx() -> Result<Context, Box<dyn std::error::Error>> {
        Ok(Context {
            rules: vec![RuleChunk {
                id: RuleId::try_new("702.15b".to_owned())?,
                parent_id: None,
                subsection: RuleId::try_new("702".to_owned())?,
                heading: "Lifelink".into(),
                body: "gain that much life".into(),
                examples: vec![],
                cr_version: CrVersion::try_new("20250801".to_owned())?,
            }],
            history: vec![Qa { question: "earlier?".into(), answer: "yes".into() }],
            ..Context::default()
        })
    }

    #[test]
    fn renders_cr_version_history_and_rejection() -> Result<(), Box<dyn std::error::Error>> {
        let q = Question { thread_id: "t".into(), text: "why?".into() };
        let bad = Citation::Rule { id: RuleId::try_new("702.15b".to_owned())?, quote: "nope".into() };
        let s = render_user_turn(&q, &ctx()?, Some(&bad));
        assert!(s.contains("Comprehensive Rules (effective 20250801)"), "{s}");
        assert!(s.contains("## Earlier in this thread\nQ: earlier?\nA: yes"), "{s}");
        assert!(s.contains("## Previous attempt rejected") && s.contains("rule 702.15b: \"nope\""), "{s}");
        assert!(s.ends_with("## Question\nwhy?\n"), "{s}");
        assert!(!render_user_turn(&q, &ctx()?, None).contains("rejected"));
        Ok(())
    }
}
