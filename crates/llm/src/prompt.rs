//! What an adapter adds to its prompt when the backend cannot enforce the
//! output schema server-side ([`StructuredOutput::JsonMode`] guarantees
//! valid JSON of no particular shape; [`StructuredOutput::PromptOnly`]
//! guarantees nothing). The schema goes into the *user turn*, never the
//! system prompt: the system prompt is tuned text whose Anthropic rendering
//! is pinned by digest, and it must stay byte-identical whichever backend
//! serves it.
//!
//! Decoding is unchanged either way (serde enforces the type), so a model
//! that ignores the block costs a retry, not a guarantee.

use schemars::Schema;

use crate::{Capabilities, OutputSchema, StructuredOutput, TextBlock};

/// Whether requests on a backend with `caps` must carry their output schema
/// in the prompt.
#[must_use]
pub fn needs_schema_in_prompt(caps: Capabilities) -> bool {
    match caps.structured_output {
        StructuredOutput::Enforced => false,
        StructuredOutput::JsonMode | StructuredOutput::PromptOnly => true,
    }
}

/// The user-turn block asking for `schema`: the full schemars schema, not a
/// backend's subset (a model reading a prompt can use every keyword).
#[must_use]
pub fn schema_block(schema: &OutputSchema) -> TextBlock {
    TextBlock::plain(schema_notice(&schema.schema))
}

/// The text of [`schema_block`].
#[must_use]
pub fn schema_notice(schema: &Schema) -> String {
    let json = serde_json::to_string_pretty(schema.as_value()).unwrap_or_default();
    format!(
        "\n# Output format\nRespond with exactly one JSON object and nothing else — no prose before or after it \
         and no code fence — conforming to this JSON Schema:\n```json\n{json}\n```\n"
    )
}

/// `text` without a surrounding Markdown code fence, if it has one. A model
/// asked for JSON in the prompt sometimes wraps it anyway; JSON never starts
/// with a backtick, so this cannot damage an unfenced answer.
#[must_use]
pub fn strip_json_fence(text: &str) -> &str {
    let t = text.trim();
    let Some(rest) = t.strip_prefix("```") else {
        return text;
    };
    let Some(inner) = rest.strip_suffix("```") else {
        return text;
    };
    // The opening fence may name a language (```json), on its own line or
    // run straight into the JSON; JSON never starts with a letter or digit,
    // so a leading alphanumeric run can only be that tag.
    let tag = inner.bytes().take_while(u8::is_ascii_alphanumeric).count();
    inner.get(tag..).unwrap_or(inner).trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_goes_in_the_prompt_only_when_not_enforced() {
        let caps = |s| Capabilities {
            structured_output: s,
            strict_tools: false,
            effort: false,
            cache_hints: false,
            refusal_fallbacks: false,
        };
        assert!(!needs_schema_in_prompt(caps(StructuredOutput::Enforced)));
        assert!(needs_schema_in_prompt(caps(StructuredOutput::JsonMode)));
        assert!(needs_schema_in_prompt(caps(StructuredOutput::PromptOnly)));
    }

    #[test]
    fn the_block_carries_the_untransformed_schema_uncached() {
        let b = schema_block(&OutputSchema::of::<judge_core::Verdict>());
        assert!(b.cache.is_none());
        assert!(b.text.starts_with("\n# Output format\n"), "{}", b.text);
        assert!(
            b.text.contains("```json\n{") && b.text.trim_end().ends_with("```"),
            "{}",
            b.text
        );
        // Full schemars output: the tagged enum is still a oneOf (a backend subset would have rewritten it).
        assert!(b.text.contains("\"oneOf\""), "{}", b.text);
    }

    #[test]
    fn fences_are_stripped_and_bare_json_is_untouched() {
        assert_eq!(strip_json_fence("{\"a\":1}"), "{\"a\":1}");
        assert_eq!(strip_json_fence("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_json_fence("```\n{\"a\":1}\n```\n"), "{\"a\":1}");
        assert_eq!(strip_json_fence("  ```JSON\n{\"a\": 1}```"), "{\"a\": 1}");
        // The tag and the JSON on one line (some small models do this).
        assert_eq!(strip_json_fence("```json{\"a\":1}```"), "{\"a\":1}");
        assert_eq!(strip_json_fence("```[1, 2]```"), "[1, 2]");
        // A fence with no closing fence, or prose around it, is left alone: the parse error should show it.
        assert_eq!(strip_json_fence("```json\n{"), "```json\n{");
        assert_eq!(
            strip_json_fence("Here:\n```json\n{}\n```"),
            "Here:\n```json\n{}\n```"
        );
    }
}
