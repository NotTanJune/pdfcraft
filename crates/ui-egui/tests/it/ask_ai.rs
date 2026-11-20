//! Ask AI: the panel, the key, the question and the cited answer, against a server on 127.0.0.1.

use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use pdfcraft_assist::mock::{MockServer, Reply};
use pdfcraft_ui_egui::{AiOutcome, KeySource, PdfCraftApp};

use crate::gpu;

/// The in-memory key store is one per process: tests that set or clear the key take turns.
static KEY: Mutex<()> = Mutex::new(());

pub(crate) fn key_lock() -> MutexGuard<'static, ()> {
    pdfcraft_assist::secret::keep_in_memory_only();
    let guard = KEY.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _ = pdfcraft_assist::secret::delete();
    guard
}

fn pdf() -> Vec<u8> {
    let (c1, c2) = ("BT /F1 18 Tf 40 340 Td (Force majeure clause) Tj ET", "BT /F1 18 Tf 40 340 Td (Definitions of terms) Tj ET");
    let objs = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 /MediaBox [0 0 300 400] /Resources << /Font << /F1 5 0 R >> >> >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /Contents 6 0 R >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /Contents 7 0 R >>".to_string(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        format!("<< /Length {} >>\nstream\n{c1}\nendstream", c1.len()),
        format!("<< /Length {} >>\nstream\n{c2}\nendstream", c2.len()),
    ];
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offs = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offs.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
    }
    let x = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
    for o in offs {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{x}\n%%EOF\n", objs.len() + 1).as_bytes());
    out
}

fn harness(base_url: Option<String>, inline: bool) -> Harness<'static, PdfCraftApp> {
    let mut h = Harness::builder().with_size(egui::vec2(1400.0, 900.0)).build_eframe(move |_cc| {
        let mut app = PdfCraftApp::new();
        app.set_option("language", "en").unwrap();
        app.run_inline = inline;
        app.open_bytes("contract.pdf", None, pdf()).unwrap();
        app.set_option("left", "closed").unwrap();
        app.set_option("panel", "none").unwrap();
        if let Some(url) = &base_url {
            app.set_option("ai-base-url", url).unwrap();
            app.set_option("ai-model", "mock-model").unwrap();
        }
        app
    });
    h.run_steps(8);
    h
}

/// Select the first page's text and open Ask AI on it, as the page's menu does.
fn select_and_ask(h: &mut Harness<'static, PdfCraftApp>) {
    assert!(h.state_mut().views[0].select_all());
    h.run_steps(4);
    assert!(h.state_mut().execute("ai.ask"));
    h.run_steps(3);
}

/// Drag across the first words of page 1, as a hand selects them.
fn drag_select(h: &mut Harness<'static, PdfCraftApp>) {
    // The page's text layer arrives a frame or two after the page.
    h.run_steps(4);
    let (from, to) = {
        let v = &h.state().views[0];
        (v.glyph_screen_pos(0, 0).expect("text layer"), v.glyph_screen_pos(0, 10).expect("glyph"))
    };
    h.hover_at(from);
    h.run_steps(1);
    h.drag_at(from);
    h.run_steps(1);
    for k in 1..=4 {
        h.hover_at(from + (to - from) * (k as f32 / 4.0));
        h.run_steps(1);
    }
    h.drop_at(to);
    h.run_steps(3);
    assert!(h.state().views[0].selected_text().is_some(), "the drag selected text");
}

const ANSWER: &str = r#"{"model":"mock-model","choices":[{"message":{"role":"assistant","content":"It excuses delay [p. 2]."}}]}"#;

#[test]
fn ai_is_off_until_a_key_is_set() {
    let _gpu = gpu();
    let _key = key_lock();
    let mut h = harness(None, true);
    select_and_ask(&mut h);
    assert_eq!(h.state().ai.key_source(), KeySource::None);
    assert!(!h.state().ai.is_on());
    h.get_by_label("AI is off until you add an API key.");
    // Asking with no key sends nothing and says so.
    h.state_mut().ask_ai();
    assert!(h.state().ai.turns.is_empty());
    assert!(h.state().ai.notice().is_some_and(|n| n.contains("off")));
    // Setting a key turns it on.
    h.state_mut().ai.save_key("sk-ui-test");
    assert!(h.state().ai.is_on());
    h.state_mut().ai.remove_key();
    assert!(!h.state().ai.is_on());
}

#[test]
fn asking_about_a_selection_shows_the_answer_with_clickable_page_citations() {
    let _gpu = gpu();
    let _key = key_lock();
    let server = MockServer::start(|_| Reply::json(ANSWER)).unwrap();
    let mut h = harness(Some(format!("{}/v1", server.url())), true);
    h.state_mut().ai.save_key("sk-ui-test");
    select_and_ask(&mut h);
    assert_eq!(h.state().ai.selection.as_ref().map(|s| (s.page, s.text.trim().to_string())), Some((0, "Force majeure clause".into())));
    assert!(server.requests().is_empty(), "opening the panel sends nothing");
    h.state_mut().ai.question = "What does it do?".into();
    h.state_mut().ai.send_page_text = true;
    h.state_mut().ask_ai();
    h.run_steps(3);
    let seen = server.requests();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].header("authorization"), Some("Bearer sk-ui-test"));
    let body: serde_json::Value = serde_json::from_slice(&seen[0].body).unwrap();
    let user = body["messages"][1]["content"].as_str().unwrap();
    assert!(user.contains("Selected text (page 1)") && user.contains("Force majeure clause") && user.contains("What does it do?"), "{user}");
    assert!(matches!(&h.state().ai.turns[..], [turn] if matches!(&turn.outcome, AiOutcome::Answer { text, .. } if text.contains("excuses delay"))));
    h.get_by_label_contains("It excuses delay");
    // The citation is a link to the page.
    assert_eq!(h.state().views[0].current, 0);
    h.get_by_label("[p. 2]").click();
    h.run_steps(4);
    assert_eq!(h.state().views[0].current, 1, "the citation went to page 2");
}

#[test]
fn the_bar_beside_a_selection_opens_ask_ai_with_it() {
    let _gpu = gpu();
    let _key = key_lock();
    let mut h = harness(None, true);
    drag_select(&mut h);
    h.get_by_label("Ask AI").click();
    h.run_steps(3);
    assert_eq!(h.state().right, Some(pdfcraft_ui_egui::RightPanel::Assistant));
    assert_eq!(h.state().ai.selection.as_ref().map(|s| s.page), Some(0));
    assert!(h.state().ai.selection.as_ref().is_some_and(|s| s.text.starts_with("Force")));
}

#[test]
fn the_page_menu_offers_ask_ai_for_a_selection() {
    let _gpu = gpu();
    let _key = key_lock();
    let mut h = harness(None, true);
    drag_select(&mut h);
    // Put the bar away (Copy keeps the words selected), so only the page's menu says Ask AI.
    h.get_by_label("Copy").click();
    h.run_steps(2);
    assert_eq!(h.query_all_by_label("Ask AI").count(), 0);
    // On the selected words themselves: a right-click elsewhere puts the selection away.
    let at = h.state().views[0].glyph_screen_pos(0, 5).expect("glyph");
    h.hover_at(at);
    h.run_steps(1);
    for pressed in [true, false] {
        h.event(egui::Event::PointerButton { pos: at, button: egui::PointerButton::Secondary, pressed, modifiers: Default::default() });
    }
    h.run_steps(3);
    h.get_by_label("Ask AI").click();
    h.run_steps(3);
    assert_eq!(h.state().right, Some(pdfcraft_ui_egui::RightPanel::Assistant));
    assert!(h.state().ai.selection.is_some());
}

#[test]
fn stopping_a_question_drops_its_answer() {
    let _gpu = gpu();
    let _key = key_lock();
    let server = MockServer::start(|_| {
        std::thread::sleep(Duration::from_millis(700));
        Reply::json(ANSWER)
    })
    .unwrap();
    let mut h = harness(Some(format!("{}/v1", server.url())), false);
    h.state_mut().ai.save_key("sk-ui-test");
    select_and_ask(&mut h);
    h.state_mut().ask_ai();
    assert!(h.state().ai.is_asking());
    h.state_mut().cancel_ai();
    assert!(!h.state().ai.is_asking());
    assert!(matches!(&h.state().ai.turns[..], [turn] if turn.outcome == AiOutcome::Cancelled));
    std::thread::sleep(Duration::from_millis(1200));
    h.run_steps(3);
    assert_eq!(h.state().ai.turns.len(), 1, "the late answer was dropped");
}

#[test]
fn an_unusable_endpoint_says_why_in_the_panel() {
    let _gpu = gpu();
    let _key = key_lock();
    let mut h = harness(Some("http://api.example.com/v1".into()), true);
    h.state_mut().ai.save_key("sk-ui-test");
    select_and_ask(&mut h);
    h.state_mut().ask_ai();
    h.run_steps(3);
    assert!(matches!(&h.state().ai.turns[..], [turn] if matches!(&turn.outcome, AiOutcome::Failed(why) if why.contains("https"))));
}

#[test]
fn the_settings_keep_the_endpoint_and_model_but_never_the_key() {
    let _gpu = gpu();
    let _key = key_lock();
    let mut h = harness(Some("https://gateway.example.org/v1".into()), true);
    h.state_mut().ai.save_key("sk-never-persisted");
    let saved = h.state().persist();
    assert!(saved.contains("gateway.example.org") && saved.contains("mock-model"));
    assert!(!saved.contains("sk-never-persisted"), "the key is not in the preferences");
}
