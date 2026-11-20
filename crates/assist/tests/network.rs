//! The two network features against a server on 127.0.0.1: nothing here reaches the Internet.

use pdfcraft_assist::chat::{AiConfig, AiError, CancelToken, Question};
use pdfcraft_assist::mock::{MockServer, Reply};
use pdfcraft_assist::preview::{FetchPolicy, PreviewError, fetch};
use pdfcraft_assist::{ApiKey, ask};

const ANSWER: &str = r#"{"model":"test-model","choices":[{"message":{"role":"assistant","content":"It means X [p. 3]."}}]}"#;

fn question() -> Question {
    Question {
        question: "What is this?".into(),
        selected: "Force majeure".into(),
        page: 3,
        context: Some("the clause around it".into()),
        ..Question::default()
    }
}

fn config(server: &MockServer) -> AiConfig {
    AiConfig { base_url: format!("{}/v1", server.url()), model: "test-model".into() }
}

fn key() -> ApiKey {
    ApiKey::new("sk-test-key-123").unwrap()
}

#[test]
fn a_question_goes_to_chat_completions_with_the_key_and_the_page() {
    let server = MockServer::start(|_| Reply::json(ANSWER)).unwrap();
    let answer = ask(&config(&server), &key(), &question(), &CancelToken::new()).unwrap();
    assert_eq!(answer.text, "It means X [p. 3].");
    assert_eq!(answer.model, "test-model");
    let seen = server.requests();
    assert_eq!(seen.len(), 1);
    let request = &seen[0];
    assert_eq!((request.method.as_str(), request.path.as_str()), ("POST", "/v1/chat/completions"));
    assert_eq!(request.header("authorization"), Some("Bearer sk-test-key-123"));
    assert_eq!(request.header("content-type"), Some("application/json"));
    assert!(request.header("cookie").is_none());
    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body["model"], "test-model");
    let user = body["messages"][1]["content"].as_str().unwrap();
    assert!(user.contains("Selected text (page 3)") && user.contains("Force majeure") && user.contains("the clause around it"), "{user}");
    assert!(body["messages"][0]["content"].as_str().unwrap().contains("[p. 12]"), "the system prompt asks for page citations");
}

#[test]
fn an_error_answer_says_what_the_endpoint_said_and_not_the_key() {
    let server = MockServer::start(|_| Reply::new(401, "application/json", r#"{"error":{"message":"Incorrect API key provided."}}"#)).unwrap();
    let error = ask(&config(&server), &key(), &question(), &CancelToken::new()).unwrap_err();
    assert!(matches!(&error, AiError::Http { status: 401, message } if message.contains("Incorrect API key")), "{error:?}");
    assert!(!error.to_string().contains("sk-test-key-123"));
}

#[test]
fn a_redirect_is_not_followed_so_the_key_stays_on_its_host() {
    let target = MockServer::start(|_| Reply::json(ANSWER)).unwrap();
    let location = format!("{}/steal", target.url());
    let server = MockServer::start(move |_| Reply::redirect(307, &location)).unwrap();
    let error = ask(&config(&server), &key(), &question(), &CancelToken::new()).unwrap_err();
    assert!(matches!(error, AiError::Http { status: 307, .. }), "{error:?}");
    assert!(target.requests().is_empty(), "the redirect target saw nothing");
}

#[test]
fn a_cleartext_endpoint_off_this_machine_is_refused_before_any_connection() {
    let config = AiConfig { base_url: "http://api.example.com/v1".into(), model: "m".into() };
    assert!(matches!(ask(&config, &key(), &question(), &CancelToken::new()), Err(AiError::InvalidBaseUrl(_))));
}

#[test]
fn a_huge_answer_is_refused() {
    let big = "x".repeat(5 * 1024 * 1024);
    let server = MockServer::start(move |_| Reply::json(&big)).unwrap();
    assert!(matches!(ask(&config(&server), &key(), &question(), &CancelToken::new()), Err(AiError::BadResponse(_))));
}

const PAGE: &str = r#"<!doctype html><html><head><title>Plain</title>
<meta property="og:title" content="The Example Title"><meta property="og:description" content="What the page says.">
</head><body><h1>Hi</h1></body></html>"#;

fn local() -> FetchPolicy {
    FetchPolicy { allow_private_hosts: true }
}

#[test]
fn a_page_card_has_the_title_and_description_and_the_request_is_bare() {
    let server = MockServer::start(|_| Reply::html(PAGE)).unwrap();
    let card = fetch(&format!("{}/article?x=1", server.url()), &local()).unwrap();
    assert_eq!(card.meta.title.as_deref(), Some("The Example Title"));
    assert_eq!(card.meta.description.as_deref(), Some("What the page says."));
    let seen = server.requests();
    assert_eq!(seen[0].method, "GET");
    assert_eq!(seen[0].path, "/article?x=1");
    for header in ["cookie", "referer", "authorization"] {
        assert!(seen[0].header(header).is_none(), "{header} must not be sent");
    }
    assert!(seen[0].header("user-agent").is_some_and(|u| u.starts_with("PdfCraft/")));
}

#[test]
fn redirects_are_followed_up_to_three() {
    let server = MockServer::start(|r| match r.path.as_str() {
        "/a" => Reply::redirect(301, "/b"),
        "/b" => Reply::redirect(302, "/c"),
        "/c" => Reply::redirect(307, "/d"),
        "/d" => Reply::html(PAGE),
        _ => Reply::redirect(302, "/a"),
    })
    .unwrap();
    let card = fetch(&format!("{}/a", server.url()), &local()).unwrap();
    assert!(card.final_url.ends_with("/d"));
    assert_eq!(card.meta.title.as_deref(), Some("The Example Title"));
}

#[test]
fn a_fourth_redirect_stops_it() {
    let server = MockServer::start(|r| match r.path.as_str() {
        "/a" => Reply::redirect(301, "/b"),
        "/b" => Reply::redirect(302, "/c"),
        "/c" => Reply::redirect(307, "/d"),
        "/d" => Reply::redirect(307, "/e"),
        _ => Reply::html(PAGE),
    })
    .unwrap();
    assert_eq!(fetch(&format!("{}/a", server.url()), &local()), Err(PreviewError::TooManyRedirects));
}

#[test]
fn a_redirect_off_the_web_schemes_is_refused() {
    let server = MockServer::start(|_| Reply::redirect(302, "file:///etc/passwd")).unwrap();
    assert!(matches!(fetch(&format!("{}/", server.url()), &local()), Err(PreviewError::InvalidUrl(_))));
}

#[test]
fn only_the_first_half_mebibyte_is_read_and_a_head_there_still_gives_a_card() {
    let mut page = String::from(PAGE);
    page.push_str(&"<p>filler</p>".repeat(200_000));
    let server = MockServer::start(move |_| Reply::html(&page)).unwrap();
    let card = fetch(&format!("{}/", server.url()), &local()).unwrap();
    assert_eq!(card.meta.title.as_deref(), Some("The Example Title"));
}

#[test]
fn a_title_past_the_limit_is_not_found() {
    let mut page = "<head>".to_string();
    page.push_str(&"<!-- pad -->".repeat(60_000));
    page.push_str("<title>Too late</title></head>");
    let server = MockServer::start(move |_| Reply::html(&page)).unwrap();
    let card = fetch(&format!("{}/", server.url()), &local()).unwrap();
    assert_eq!(card.meta.title, None);
}

#[test]
fn a_page_that_is_not_html_gives_no_card() {
    let server = MockServer::start(|_| Reply::new(200, "application/pdf", b"%PDF-1.7".to_vec())).unwrap();
    assert!(matches!(fetch(&format!("{}/", server.url()), &local()), Err(PreviewError::NotHtml(_))));
    let server = MockServer::start(|_| Reply::new(404, "text/html", "gone")).unwrap();
    assert_eq!(fetch(&format!("{}/", server.url()), &local()), Err(PreviewError::Status(404)));
}

#[test]
fn the_app_policy_does_not_reach_this_computer() {
    let server = MockServer::start(|_| Reply::html(PAGE)).unwrap();
    let result = fetch(&format!("{}/", server.url()), &FetchPolicy::default());
    assert!(matches!(result, Err(PreviewError::PrivateHost(_))), "{result:?}");
    assert!(server.requests().is_empty(), "nothing was sent");
}
