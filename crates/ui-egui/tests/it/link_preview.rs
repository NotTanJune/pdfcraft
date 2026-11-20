//! Hovering a link shows a card; ⌘-click (Ctrl-click) on a web link opens it at once.

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use pdfcraft_assist::mock::{MockServer, Reply};
use pdfcraft_ui_egui::{LinkOrigin, PdfCraftApp, PendingLink};

use crate::gpu;

/// Two 300x400 pages. Page 1: a link to page 2 (x 50..250, y 300..350), a web link to `url`
/// (y 200..250) and a mail link (y 100..150).
fn fixture(url: &str) -> Vec<u8> {
    let (c1, c2) = ("BT /F1 18 Tf 40 360 Td (Go to the clause) Tj ET", "BT /F1 18 Tf 20 360 Td (The clause itself) Tj ET");
    let objs = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 /MediaBox [0 0 300 400] /Resources << /Font << /F1 5 0 R >> >> >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /Contents 6 0 R /Annots [7 0 R 8 0 R 9 0 R] >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /Contents 10 0 R >>".to_string(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        format!("<< /Length {} >>\nstream\n{c1}\nendstream", c1.len()),
        "<< /Type /Annot /Subtype /Link /Rect [50 300 250 350] /Border [0 0 0] /Dest [4 0 R /XYZ 20 380 0] >>".to_string(),
        format!("<< /Type /Annot /Subtype /Link /Rect [50 200 250 250] /Border [0 0 0] /A << /S /URI /URI ({url}) >> >>"),
        "<< /Type /Annot /Subtype /Link /Rect [50 100 250 150] /Border [0 0 0] /A << /S /URI /URI (mailto:a@example.org) >> >>".to_string(),
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

fn harness(url: &str, fetch: bool) -> Harness<'static, PdfCraftApp> {
    let url = url.to_string();
    let mut h = Harness::builder().with_size(egui::vec2(1400.0, 900.0)).build_eframe(move |_cc| {
        let mut app = PdfCraftApp::new();
        app.set_option("language", "en").unwrap();
        app.run_inline = true;
        app.open_bytes("links.pdf", None, fixture(&url)).unwrap();
        app.set_option("left", "closed").unwrap();
        app.set_option("panel", "none").unwrap();
        app.set_option("zoom", "150").unwrap();
        if fetch {
            app.set_option("fetch-link-previews", "true").unwrap();
        }
        app.allow_private_link_previews();
        app
    });
    h.run_steps(8);
    h
}

/// The point at height `y` (PDF points) in the middle of page 1.
fn at(h: &Harness<'static, PdfCraftApp>, y: f32) -> egui::Pos2 {
    let r = h.state().views[0].page_screen_rect(0).expect("page 1 on screen");
    egui::pos2(r.min.x + r.width() * 0.5, r.min.y + r.height() * (1.0 - y / 400.0))
}

fn rest_on(h: &mut Harness<'static, PdfCraftApp>, at: egui::Pos2) {
    h.hover_at(at);
    // Quarter-second steps: the card's delays pass.
    h.run_steps(8);
}

#[test]
fn resting_on_a_link_to_a_page_draws_the_part_of_the_page_it_goes_to() {
    let _gpu = gpu();
    let mut h = harness("https://example.org/", false);
    let p = at(&h, 325.0);
    h.hover_at(p);
    h.run_steps(1);
    assert!(h.state().link_preview_report().hovering, "the tip's card is up at once");
    assert_eq!(h.state().link_preview_report().pictures_ready, 0, "the picture waits for the pointer to rest");
    h.run_steps(8);
    assert_eq!(h.state().link_preview_report().pictures_ready, 1);
    h.get_by_label("Go to page 2");
    // Moving off the link drops the card.
    h.hover_at(egui::pos2(5.0, 5.0));
    h.run_steps(2);
    assert!(!h.state().link_preview_report().hovering);
}

#[test]
fn a_web_link_shows_its_address_and_never_fetches_while_the_setting_is_off() {
    let _gpu = gpu();
    let server = MockServer::start(|_| Reply::html("<title>Nope</title>")).unwrap();
    let url = format!("{}/page", server.url());
    let mut h = harness(&url, false);
    let p = at(&h, 225.0);
    rest_on(&mut h, p);
    h.get_by_label(&url);
    h.get_by_label("127.0.0.1");
    let report = h.state().link_preview_report();
    assert!(report.hovering && report.fetches_started.is_empty() && report.cards.is_empty(), "{report:?}");
    assert!(server.requests().is_empty(), "nothing leaves the machine");
}

#[test]
fn with_the_setting_on_a_web_link_shows_the_fetched_title_and_description() {
    let _gpu = gpu();
    let server = MockServer::start(|_| {
        Reply::html(
            r#"<head><title>x</title><meta property="og:title" content="A Fetched Title"><meta name="description" content="Its description"></head>"#,
        )
    })
    .unwrap();
    let url = format!("{}/page", server.url());
    let mut h = harness(&url, true);
    let p = at(&h, 225.0);
    h.hover_at(p);
    h.run_steps(1);
    assert!(server.requests().is_empty(), "a pointer that has only arrived fetches nothing");
    h.run_steps(8);
    let report = h.state().link_preview_report();
    assert_eq!(report.cards, vec![(url.clone(), Some("A Fetched Title".into()), Some("Its description".into()))], "{report:?}");
    h.get_by_label("A Fetched Title");
    h.get_by_label("Its description");
    // The same address is fetched once for the session.
    h.hover_at(egui::pos2(5.0, 5.0));
    h.run_steps(2);
    rest_on(&mut h, p);
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn a_mail_link_has_a_tip_and_no_card() {
    let _gpu = gpu();
    let mut h = harness("https://example.org/", true);
    let p = at(&h, 125.0);
    rest_on(&mut h, p);
    let report = h.state().link_preview_report();
    assert!(!report.hovering && report.fetches_started.is_empty(), "{report:?}");
}

fn click_with(h: &mut Harness<'static, PdfCraftApp>, at: egui::Pos2, modifiers: egui::Modifiers) {
    h.hover_at(at);
    h.run_steps(2);
    // The key is held before the button goes down and after it comes up, as a hand does it.
    h.event(egui::Event::ModifiersChanged(modifiers));
    h.run_steps(1);
    for pressed in [true, false] {
        h.event(egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers });
    }
    h.run_steps(4);
    h.event(egui::Event::ModifiersChanged(egui::Modifiers::NONE));
    h.run_steps(1);
}

#[test]
fn a_plain_click_on_a_web_link_still_asks_first() {
    let _gpu = gpu();
    let mut h = harness("https://example.org/web", false);
    let p = at(&h, 225.0);
    click_with(&mut h, p, egui::Modifiers::NONE);
    assert_eq!(h.state().pending_link, Some(PendingLink { url: "https://example.org/web".into(), origin: LinkOrigin::Link }));
    assert_eq!(h.state().last_opened_url, None);
}

#[test]
fn a_command_click_on_a_web_link_opens_it_at_once() {
    let _gpu = gpu();
    let mut h = harness("https://example.org/web", false);
    let p = at(&h, 225.0);
    click_with(&mut h, p, egui::Modifiers::COMMAND);
    assert_eq!(h.state().last_opened_url.as_deref(), Some("https://example.org/web"));
    assert_eq!(h.state().pending_link, None, "no question was asked");
}

#[test]
fn a_command_click_still_asks_before_writing_an_email_and_goes_to_a_page_link_as_usual() {
    let _gpu = gpu();
    let mut h = harness("https://example.org/web", false);
    let p = at(&h, 125.0);
    click_with(&mut h, p, egui::Modifiers::COMMAND);
    assert_eq!(h.state().last_opened_url, None);
    assert_eq!(h.state().pending_link.as_ref().map(|p| p.url.as_str()), Some("mailto:a@example.org"));
}
