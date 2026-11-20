//! Ask a model about text selected on a page.
//!
//! The client speaks the OpenAI Chat Completions wire format, which OpenAI, Ollama, LM Studio and
//! most gateways accept at `<base URL>/chat/completions`. The request is built from the selected
//! text, its page number and, when the caller passes it, the text around it on that page; the
//! system prompt asks the model to cite pages as `[p. 12]` ([`crate::cite`] finds those).
//!
//! Safety, in the order a request meets it:
//! - the endpoint must be `https`, or `http` to this machine (a local model server): the key is
//!   never sent in the clear to another host;
//! - no redirect is followed, so `Authorization` goes to the one host the person chose;
//! - the whole exchange is bounded by [`TIMEOUT`], the answer by [`MAX_RESPONSE_BYTES`], and every
//!   piece of the prompt by the `MAX_*` limits below;
//! - the key appears only in the header: not in any error, log or `Debug` output.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Value, json};

use crate::secret::ApiKey;

/// The endpoint used when none is set: OpenAI's.
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// The model used when none is set. `gpt-6-luna` is OpenAI's most efficient current model; its
/// model page (developers.openai.com/api/docs/models/gpt-6-luna) lists Chat Completions among its
/// supported endpoints. Any model id the endpoint knows can replace it.
pub const DEFAULT_MODEL: &str = "gpt-6-luna";

/// Environment variables that override the base URL and the model (headless use).
pub const BASE_URL_ENV: &str = "PDFCRAFT_AI_BASE_URL";
pub const MODEL_ENV: &str = "PDFCRAFT_AI_MODEL";

/// How long one question may take, from connecting to the last byte.
pub const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
/// The most of an answer that is read.
pub const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// The most characters of the selection, of the surrounding page text and of the question sent.
pub const MAX_SELECTED_CHARS: usize = 8_000;
pub const MAX_CONTEXT_CHARS: usize = 12_000;
pub const MAX_QUESTION_CHARS: usize = 2_000;
/// Earlier question and answer pairs kept in a conversation, and the most of each answer kept.
pub const MAX_HISTORY_TURNS: usize = 6;
pub const MAX_HISTORY_ANSWER_CHARS: usize = 2_000;

/// What the model is told it is doing.
pub const SYSTEM_PROMPT: &str = "You are a reading assistant inside a PDF viewer. The user selected text on a page and asks about it. \
Answer from the selected text and the page text provided; if they do not contain the answer, say so instead of guessing. \
Be concise. Cite the page of every claim in square brackets, like [p. 12]; when the answer comes from several pages, cite each, like [p. 3] [p. 7]. \
Use only page numbers given in the message.";

/// The endpoint and model: everything about the connection except the key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AiConfig {
    pub base_url: String,
    pub model: String,
}

impl Default for AiConfig {
    fn default() -> Self {
        AiConfig { base_url: DEFAULT_BASE_URL.to_string(), model: DEFAULT_MODEL.to_string() }
    }
}

impl AiConfig {
    /// The defaults, with `PDFCRAFT_AI_BASE_URL` and `PDFCRAFT_AI_MODEL` applied where set.
    pub fn from_env() -> Self {
        let mut config = AiConfig::default();
        if let Some(url) = std::env::var(BASE_URL_ENV).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) {
            config.base_url = url;
        }
        if let Some(model) = std::env::var(MODEL_ENV).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) {
            config.model = model;
        }
        config
    }

    /// The Chat Completions URL, after checking the base URL is one a key may be sent to.
    pub fn endpoint(&self) -> Result<String, AiError> {
        let bad = |why: &str| AiError::InvalidBaseUrl(why.to_string());
        let base = self.base_url.trim();
        let url = craft_http_url::Url::parse(base).map_err(|e| bad(&e.to_string()))?;
        if base.contains('#') || url.path.contains('?') {
            return Err(bad("remove the query and fragment"));
        }
        if !url.tls && !is_local_host(&url.host) {
            return Err(bad("use https; plain http is accepted only for a model server on this computer (localhost)"));
        }
        let scheme = if url.tls { "https" } else { "http" };
        let path = url.path.trim_end_matches('/');
        Ok(format!("{scheme}://{}{path}/chat/completions", url.host_header()))
    }
}

/// Whether `host` (as `craft_http_url` lowercases it, without brackets) is this computer.
fn is_local_host(host: &str) -> bool {
    host == "localhost" || host.ends_with(".localhost") || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// One finished exchange of a conversation, sent again so the model has the thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Turn {
    pub question: String,
    pub answer: String,
}

/// A question about a passage.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Question {
    /// What to ask. Empty asks for an explanation of the selection.
    pub question: String,
    /// The selected text.
    pub selected: String,
    /// The page the selection is on, from 1.
    pub page: usize,
    /// The text of the page around the selection, if the person or caller wants it sent.
    pub context: Option<String>,
    /// The conversation so far, oldest first.
    pub history: Vec<Turn>,
}

/// `text` cut to at most `max` characters, at a character boundary, with a note when it was cut.
fn clip(text: &str, max: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() { format!("{head}\n[cut: the text is longer than {max} characters]") } else { head }
}

impl Question {
    /// The chat messages sent for this question: the system prompt, the thread so far, then the
    /// passage with its page number and the question. This is exactly what leaves the machine.
    pub fn messages(&self) -> Vec<Value> {
        let mut messages = vec![json!({"role": "system", "content": SYSTEM_PROMPT})];
        let skip = self.history.len().saturating_sub(MAX_HISTORY_TURNS);
        for turn in self.history.iter().skip(skip) {
            messages.push(json!({"role": "user", "content": clip(&turn.question, MAX_QUESTION_CHARS)}));
            messages.push(json!({"role": "assistant", "content": clip(&turn.answer, MAX_HISTORY_ANSWER_CHARS)}));
        }
        let question = self.question.trim();
        let question = if question.is_empty() { "Explain this passage." } else { question };
        let mut text = format!("Selected text (page {}):\n\"\"\"\n{}\n\"\"\"\n", self.page, clip(self.selected.trim(), MAX_SELECTED_CHARS));
        if let Some(context) = self.context.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
            text.push_str(&format!("\nText around it on page {}:\n\"\"\"\n{}\n\"\"\"\n", self.page, clip(context, MAX_CONTEXT_CHARS)));
        }
        text.push_str(&format!("\nQuestion: {}", clip(question, MAX_QUESTION_CHARS)));
        messages.push(json!({"role": "user", "content": text}));
        messages
    }

    /// The JSON body of the Chat Completions request. No sampling or token-limit fields are
    /// sent: reasoning models reject some of them, and the endpoint's defaults are right.
    pub fn request_body(&self, config: &AiConfig) -> Value {
        json!({"model": config.model.trim(), "messages": self.messages()})
    }
}

/// A model's answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    pub text: String,
    /// The model the endpoint says answered (it may differ from the one asked for).
    pub model: String,
}

/// Why a question got no answer.
#[derive(Debug, thiserror::Error)]
pub enum AiError {
    #[error("AI is off: no API key is set")]
    NoKey,
    #[error("the endpoint address is not usable: {0}")]
    InvalidBaseUrl(String),
    #[error("nothing selected to ask about")]
    EmptySelection,
    #[error("cancelled")]
    Cancelled,
    #[error("asking AI needs the desktop app; this build runs in a browser")]
    Unsupported,
    #[error("could not reach the endpoint: {0}")]
    Network(String),
    #[error("the endpoint answered {status}: {message}")]
    Http { status: u16, message: String },
    #[error("the endpoint's answer could not be read: {0}")]
    BadResponse(String),
}

/// A flag the caller sets to stop waiting for an answer. The request already on the wire ends on
/// its own (at the latest at [`TIMEOUT`]); its result is dropped.
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// One line of `text`, whitespace collapsed and at most `max` characters, for an error message.
fn one_line(text: &str, max: usize) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = joined.chars();
    let head: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() { format!("{head}…") } else { head }
}

/// The answer in a Chat Completions response body, or why there is none.
pub fn parse_response(status: u16, body: &[u8]) -> Result<Answer, AiError> {
    let json: Option<Value> = serde_json::from_slice(body).ok();
    if !(200..300).contains(&status) {
        let message = json
            .as_ref()
            .and_then(|j| j.pointer("/error/message").or_else(|| j.pointer("/error")).or_else(|| j.get("message")))
            .and_then(Value::as_str)
            .map(|m| one_line(m, 300))
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| one_line(&String::from_utf8_lossy(body.get(..body.len().min(300)).unwrap_or_default()), 300));
        let hint = match status {
            401 | 403 => "the API key was refused",
            404 => "the endpoint or model was not found",
            429 => "rate limit or quota reached",
            _ => "",
        };
        let message = match (hint.is_empty(), message.is_empty()) {
            (true, _) => message,
            (false, true) => hint.to_string(),
            (false, false) => format!("{hint} ({message})"),
        };
        return Err(AiError::Http { status, message });
    }
    let json = json.ok_or_else(|| AiError::BadResponse("not JSON (is the base URL an OpenAI-compatible /v1 endpoint?)".into()))?;
    let choice = json.pointer("/choices/0").ok_or_else(|| AiError::BadResponse("no choices in the answer".into()))?;
    let content = choice.pointer("/message/content");
    let text = match content {
        Some(Value::String(s)) => s.clone(),
        // Some gateways answer with content parts.
        Some(Value::Array(parts)) => parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join(""),
        _ => String::new(),
    };
    let text = text.trim().to_string();
    if text.is_empty() {
        let refusal = choice.pointer("/message/refusal").and_then(Value::as_str).map(|r| one_line(r, 300));
        return Err(AiError::BadResponse(match refusal {
            Some(r) if !r.is_empty() => format!("the model declined: {r}"),
            _ => "the answer is empty".into(),
        }));
    }
    let model = json.get("model").and_then(Value::as_str).map(|m| one_line(m, 100)).unwrap_or_default();
    Ok(Answer { text, model })
}

/// Sends `question` and waits for the answer. Blocks: run it off the UI thread.
pub fn ask(config: &AiConfig, key: &ApiKey, question: &Question, cancel: &CancelToken) -> Result<Answer, AiError> {
    if question.selected.trim().is_empty() {
        return Err(AiError::EmptySelection);
    }
    let endpoint = config.endpoint()?;
    if config.model.trim().is_empty() {
        return Err(AiError::InvalidBaseUrl("the model name is empty".into()));
    }
    if cancel.is_cancelled() {
        return Err(AiError::Cancelled);
    }
    let body = serde_json::to_vec(&question.request_body(config)).map_err(|e| AiError::BadResponse(e.to_string()))?;
    let (status, bytes) = send(&endpoint, key, body)?;
    if cancel.is_cancelled() {
        return Err(AiError::Cancelled);
    }
    parse_response(status, &bytes)
}

#[cfg(not(target_arch = "wasm32"))]
fn send(endpoint: &str, key: &ApiKey, body: Vec<u8>) -> Result<(u16, Vec<u8>), AiError> {
    let agent = crate::net::agent(TIMEOUT);
    let mut response = agent
        .post(endpoint)
        .header("Authorization", format!("Bearer {}", key.expose()))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .send(body)
        .map_err(|e| AiError::Network(network_message(&e)))?;
    let status = response.status().as_u16();
    if (300..400).contains(&status) {
        return Err(AiError::Http {
            status,
            message: "the endpoint redirected; PdfCraft does not follow redirects with an API key (use the final address)".into(),
        });
    }
    let (bytes, more) = crate::net::read_capped(&mut response, MAX_RESPONSE_BYTES);
    if more {
        return Err(AiError::BadResponse(format!("the answer is larger than {MAX_RESPONSE_BYTES} bytes")));
    }
    Ok((status, bytes))
}

#[cfg(target_arch = "wasm32")]
fn send(_endpoint: &str, _key: &ApiKey, _body: Vec<u8>) -> Result<(u16, Vec<u8>), AiError> {
    Err(AiError::Unsupported)
}

/// What a transport error says, for people: ureq's text names the host, never a header.
#[cfg(not(target_arch = "wasm32"))]
fn network_message(e: &ureq::Error) -> String {
    match e {
        ureq::Error::Timeout(_) => "the request timed out".to_string(),
        other => one_line(&other.to_string(), 200),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question() -> Question {
        Question { question: "What does this mean?".into(), selected: "Force majeure".into(), page: 12, ..Question::default() }
    }

    #[test]
    fn the_message_names_the_page_and_the_selection() {
        let messages = question().messages();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        let user = messages[1]["content"].as_str().unwrap();
        assert!(user.contains("Selected text (page 12)"));
        assert!(user.contains("Force majeure"));
        assert!(user.ends_with("Question: What does this mean?"));
        assert!(!user.contains("Text around it"), "no context unless it is passed");
    }

    #[test]
    fn context_and_history_are_added_and_bounded() {
        let mut q = question();
        q.context = Some("c".repeat(MAX_CONTEXT_CHARS + 50));
        q.history = (0..10).map(|i| Turn { question: format!("q{i}"), answer: format!("a{i}") }).collect();
        let messages = q.messages();
        // system + 6 turns * 2 + the question
        assert_eq!(messages.len(), 1 + MAX_HISTORY_TURNS * 2 + 1);
        assert_eq!(messages[1]["content"], "q4", "the oldest turns are dropped");
        let user = messages.last().unwrap()["content"].as_str().unwrap();
        assert!(user.contains("Text around it on page 12"));
        assert!(user.contains("[cut: the text is longer than 12000 characters]"));
    }

    #[test]
    fn an_empty_question_asks_for_an_explanation() {
        let mut q = question();
        q.question = "  ".into();
        assert!(q.messages()[1]["content"].as_str().unwrap().ends_with("Question: Explain this passage."));
    }

    #[test]
    fn clipping_never_splits_a_character() {
        assert_eq!(clip("aé日本語", 3), "aé日\n[cut: the text is longer than 3 characters]");
        assert_eq!(clip("abc", 3), "abc");
    }

    #[test]
    fn the_request_body_carries_model_and_messages_only() {
        let body = question().request_body(&AiConfig::default());
        assert_eq!(body["model"], DEFAULT_MODEL);
        assert_eq!(body.as_object().unwrap().len(), 2);
    }

    #[test]
    fn the_endpoint_is_https_or_local() {
        let endpoint = |url: &str| AiConfig { base_url: url.into(), model: "m".into() }.endpoint();
        assert_eq!(endpoint("https://api.openai.com/v1").unwrap(), "https://api.openai.com/v1/chat/completions");
        assert_eq!(endpoint("https://api.openai.com/v1/").unwrap(), "https://api.openai.com/v1/chat/completions");
        assert_eq!(endpoint("http://localhost:11434/v1").unwrap(), "http://localhost:11434/v1/chat/completions");
        assert_eq!(endpoint("http://127.0.0.1:1234/v1").unwrap(), "http://127.0.0.1:1234/v1/chat/completions");
        assert_eq!(endpoint("http://[::1]:8080").unwrap(), "http://[::1]:8080/chat/completions");
        assert!(endpoint("http://api.example.com/v1").is_err(), "a key is not sent in the clear to another host");
        assert!(endpoint("http://localhost.evil.example/v1").is_err());
        assert!(endpoint("ftp://example.com").is_err());
        assert!(endpoint("https://user:pw@example.com/v1").is_err());
        assert!(endpoint("https://example.com/v1?key=1").is_err());
        assert!(endpoint("").is_err());
    }

    #[test]
    fn a_normal_answer_is_read() {
        let body = br#"{"model":"gpt-6-luna-2026","choices":[{"message":{"role":"assistant","content":" It means X [p. 12]. "}}]}"#;
        let answer = parse_response(200, body).unwrap();
        assert_eq!(answer.text, "It means X [p. 12].");
        assert_eq!(answer.model, "gpt-6-luna-2026");
    }

    #[test]
    fn content_parts_are_joined() {
        let body = br#"{"choices":[{"message":{"content":[{"type":"text","text":"A"},{"type":"text","text":"B"}]}}]}"#;
        assert_eq!(parse_response(200, body).unwrap().text, "AB");
    }

    #[test]
    fn errors_say_what_the_endpoint_said() {
        let body = br#"{"error":{"message":"Incorrect API key provided: sk-***","type":"invalid_request_error"}}"#;
        match parse_response(401, body) {
            Err(AiError::Http { status: 401, message }) => assert!(message.contains("API key was refused") && message.contains("Incorrect API key")),
            other => panic!("{other:?}"),
        }
        assert!(matches!(parse_response(500, b"<html>oops</html>"), Err(AiError::Http { status: 500, .. })));
        assert!(matches!(parse_response(200, b"not json"), Err(AiError::BadResponse(_))));
        assert!(matches!(parse_response(200, br#"{"choices":[]}"#), Err(AiError::BadResponse(_))));
        assert!(
            matches!(parse_response(200, br#"{"choices":[{"message":{"content":null,"refusal":"no"}}]}"#), Err(AiError::BadResponse(m)) if m.contains("declined"))
        );
    }

    #[test]
    fn nothing_selected_is_refused_before_any_request() {
        let key = ApiKey::new("sk-test").unwrap();
        let mut q = question();
        q.selected = "  \n".into();
        assert!(matches!(ask(&AiConfig::default(), &key, &q, &CancelToken::new()), Err(AiError::EmptySelection)));
    }

    #[test]
    fn a_cancelled_question_sends_nothing() {
        let key = ApiKey::new("sk-test").unwrap();
        let cancel = CancelToken::new();
        cancel.cancel();
        assert!(matches!(ask(&AiConfig::default(), &key, &question(), &cancel), Err(AiError::Cancelled)));
    }
}
