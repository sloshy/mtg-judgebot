//! Golden test for the provider seam: the Anthropic request bodies and headers
//! of the four call shapes (`docs/proposals/providers.md` §2) must be
//! byte-identical to what the pre-seam code sent.
//!
//! The fixtures under `tests/fixtures/anthropic/` were captured at commit
//! `ebac125` from the pre-seam `judge_anthropic::Client` against the same
//! mock replies: the `Context` and question are stored alongside
//! (`context.json`, `question.json`) so the rendering input is the same, and
//! each `*.body.json` / `*.headers.txt` pair is what wiremock received. Bodies
//! are compared byte for byte; headers as a set of `name: value` lines (order
//! is not significant in HTTP, and `host` carries the mock's port).
//!
//! A change to a prompt, a schema, the wire mapping or the header set fails
//! here. When the change is intended, re-capture on purpose and review the
//! fixture diff like any other change (as `harness_tests` does for the prompt
//! digest):
//!
//! ```sh
//! UPDATE_GOLDEN=1 cargo test -p judge-bot --test anthropic_golden
//! ```

use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use judge_anthropic::{Anthropic, Endpoint};
use judge_bot::{
    extract::{ExtractConfig, LlmExtractor},
    synth::LlmSynthesizer,
};
use judge_core::{
    Card, Citation, Context, CrVersion, Extraction, Extractor as _, JudgeError, Qa, Question, Rejection, Retriever,
    RuleChunk, RuleId, Synthesizer as _,
};
use judge_llm::{ChatModel, Metered, SpendMeter, SynthConfig};
use serde::Deserialize;
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_string_contains, method, path},
};

type R = Result<(), Box<dyn std::error::Error>>;

struct StubRetriever {
    table: Vec<RuleChunk>,
    calls: Mutex<Vec<Vec<RuleId>>>,
}

#[async_trait]
impl Retriever for StubRetriever {
    async fn retrieve(&self, _q: &Question, _c: &[Card], _e: &Extraction) -> Result<Context, JudgeError> {
        Err(anyhow::anyhow!("not used").into())
    }
    async fn lookup_rules(&self, ids: &[RuleId]) -> Result<Vec<RuleChunk>, JudgeError> {
        self.calls.lock().unwrap_or_else(PoisonError::into_inner).push(ids.to_vec());
        Ok(self.table.iter().filter(|c| ids.contains(&c.id)).cloned().collect())
    }
}

#[derive(Deserialize)]
struct Asked {
    question: Question,
    history: Vec<Qa>,
}

fn fixtures() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/anthropic")
}

fn read(name: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    Ok(std::fs::read(fixtures().join(name))?)
}

fn message(stop: &str, content: &Value) -> Value {
    json!({
        "id": "msg_1", "model": "claude-opus-5", "role": "assistant",
        "content": content, "stop_reason": stop,
        "usage": {"input_tokens": 10, "output_tokens": 5}
    })
}

/// The production stack against the mock server: the Anthropic backend behind the spend cap.
fn model(server: &MockServer) -> Result<Arc<dyn ChatModel>, judge_llm::LlmError> {
    let backend = Anthropic::new(Endpoint::Direct { base_url: server.uri(), api_key: "test-key".into() })?;
    Ok(Arc::new(Metered::new(backend, SpendMeter::new())?))
}

/// Headers as the capture wrote them: sorted `name: value` lines, `host` left out (the mock port varies).
fn headers(r: &wiremock::Request) -> String {
    let mut lines: Vec<String> = r
        .headers
        .iter()
        .filter(|(k, _)| k.as_str() != "host")
        .map(|(k, v)| format!("{}: {}", k.as_str(), v.to_str().unwrap_or("<bin>")))
        .collect();
    lines.sort();
    lines.join("\n") + "\n"
}

/// `UPDATE_GOLDEN=1`: write what was sent as the new fixtures instead of comparing.
fn updating() -> bool {
    std::env::var("UPDATE_GOLDEN").is_ok_and(|v| !v.trim().is_empty() && v.trim() != "0")
}

fn assert_golden(name: &str, r: &wiremock::Request) -> R {
    if updating() {
        std::fs::write(fixtures().join(format!("{name}.body.json")), &r.body)?;
        std::fs::write(fixtures().join(format!("{name}.headers.txt")), headers(r))?;
        eprintln!("rewrote fixtures for {name}");
        return Ok(());
    }
    let body = read(&format!("{name}.body.json"))?;
    if r.body != body {
        // Show a JSON-level diff before failing on the raw bytes: far easier to read.
        let got: Value = serde_json::from_slice(&r.body)?;
        let want: Value = serde_json::from_slice(&body)?;
        assert_eq!(got, want, "{name}: body differs from the fixture");
        return Err(format!("{name}: body is JSON-equal but not byte-identical to the fixture").into());
    }
    let want = String::from_utf8(read(&format!("{name}.headers.txt"))?)?;
    assert_eq!(headers(r), want, "{name}: headers differ from the fixture");
    Ok(())
}

#[tokio::test]
async fn extraction_request_is_byte_identical() -> R {
    let asked: Asked = serde_json::from_slice(&read("question.json")?)?;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(message(
            "end_turn",
            &json!([{"type": "text", "text": r#"{"card_spans":["[[Humility]]","Bob"],"concepts":["lifelink"],"primary":{"category":"layers","confidence":"high"},"secondary":[],"source":"cr"}"#}]),
        )))
        .mount(&server)
        .await;
    let x = LlmExtractor::new(model(&server)?, ExtractConfig::default());
    x.extract(&asked.question, &asked.history).await?;
    let reqs = server.received_requests().await.unwrap_or_default();
    assert_eq!(reqs.len(), 1);
    assert_golden("extraction", reqs.first().ok_or("no extraction request")?)
}

#[tokio::test]
async fn synthesis_first_turn_continuation_and_retry_are_byte_identical() -> R {
    let asked: Asked = serde_json::from_slice(&read("question.json")?)?;
    let mut ctx: Context = serde_json::from_slice(&read("context.json")?)?;
    let server = MockServer::start().await;
    let verdict = json!({
        "answer": "Timestamps decide: the later effect wins within the same layer.", "confidence": "high",
        "citations": [{"kind": "rule", "id": "613.7", "quote": "usually done using a timestamp system"}],
        "category": "layers"
    })
    .to_string();
    // Mounted first: a request carrying a tool_result gets the verdict.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_string_contains(r#""type":"tool_result""#))
        .respond_with(ResponseTemplate::new(200).set_body_json(message("end_turn", &json!([{"type": "text", "text": verdict}]))))
        .mount(&server)
        .await;
    // The first fresh request gets a tool round whose assistant turn carries
    // every block kind the API can return; the continuation must replay all
    // of it verbatim.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(message(
            "tool_use",
            &json!([
                {"type": "thinking", "thinking": "", "signature": "sig-abc"},
                {"type": "redacted_thinking", "data": "opaque-data"},
                {"type": "text", "text": "Let me check the layer rules."},
                {"type": "future_block", "payload": {"k": [1, 2]}},
                {"type": "tool_use", "id": "toolu_01", "name": "lookup_rules", "input": {"ids": ["613.7", "613.1"]}},
                {"type": "tool_use", "id": "toolu_02", "name": "lookup_rules", "input": {"ids": ["613.1"]}}
            ]),
        )))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    // Any later fresh request (the citation retry) answers directly.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(message("end_turn", &json!([{"type": "text", "text": verdict}]))))
        .mount(&server)
        .await;

    let table = vec![RuleChunk {
        id: RuleId::try_new("613.1".to_owned())?,
        parent_id: None,
        subsection: RuleId::try_new("613".to_owned())?,
        heading: "Interaction of Continuous Effects".into(),
        body: "613.1. The values of an object's characteristics are determined by starting with the actual object.".into(),
        examples: vec![],
        cr_version: CrVersion::try_new("20250801".to_owned())?,
    }];
    let retriever = Arc::new(StubRetriever { table, calls: Mutex::new(Vec::new()) });
    let synth = LlmSynthesizer::new(model(&server)?, SynthConfig::default(), retriever.clone());
    synth.answer(&asked.question, &mut ctx, None).await?;
    assert_eq!(
        retriever.calls.lock().unwrap_or_else(PoisonError::into_inner).clone(),
        vec![vec![RuleId::try_new("613.7".to_owned())?, RuleId::try_new("613.1".to_owned())?]],
        "the tool round unions the ids of both calls"
    );
    let bad = Rejection::BadCitation(Citation::Rule { id: RuleId::try_new("613.7".to_owned())?, quote: judge_core::Quote::try_new("not there")? });
    let bad = judge_core::RejectedAttempt::new(bad, "Timestamps decide: the later effect wins within the same layer.\n\nPer 613.7.");
    synth.answer(&asked.question, &mut ctx, Some(&bad)).await?;

    let reqs = server.received_requests().await.unwrap_or_default();
    assert_eq!(reqs.len(), 3);
    assert_golden("synth_first", reqs.first().ok_or("no first request")?)?;
    assert_golden("synth_continuation", reqs.get(1).ok_or("no continuation")?)?;
    assert_golden("synth_retry", reqs.get(2).ok_or("no retry")?)?;
    Ok(())
}
