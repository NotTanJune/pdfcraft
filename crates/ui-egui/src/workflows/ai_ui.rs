//! Ask AI: a question about text selected on a page, answered by the person's own OpenAI-compatible
//! endpoint ([`pdfcraft_assist`]), in a side panel that keeps the conversation.
//!
//! AI is off until an API key is set (in the panel; the system keychain keeps it, see
//! [`pdfcraft_assist::secret`]). Nothing is sent until the person presses Ask: the selected text,
//! its page number and, when "Send this page's text too" is ticked, the page's text. A request runs
//! on its own thread and can be stopped; its answer shows page citations (`[p. 12]`) as links that
//! go to the page. The endpoint and model are kept with the preferences, never the key.
//!
//! The web build has no network client here: the panel says so and nothing is sent.

use egui::{Align, Layout};
use pdfcraft_assist::chat::{AiConfig, AiError, Answer, CancelToken, Question, Turn};
use pdfcraft_assist::cite::{self, Segment};
use pdfcraft_assist::secret::{self, ApiKey, Store};
use pdfcraft_engine::DocId;
use serde_json::{Value, json};
use std::sync::mpsc::{Receiver, TryRecvError};

use crate::theme::{self, Tokens};
use crate::{PdfCraftApp, RightPanel, widgets};

/// The longest endpoint address or model name kept.
const MAX_SETTING_CHARS: usize = 512;
/// Past exchanges kept in the panel.
const MAX_TURNS: usize = 50;
/// The lines of the selection the panel shows before cutting it.
const SELECTION_PREVIEW_CHARS: usize = 400;

/// Text selected on a page, waiting to be asked about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AiSelection {
    pub doc: DocId,
    /// The page it starts on, 0-based.
    pub page: usize,
    pub text: String,
    /// The text of that page, sent only when the person ticks "Send this page's text too".
    pub page_text: Option<String>,
}

/// How an exchange ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Answer { text: String, model: String },
    Failed(String),
    Cancelled,
}

/// One question and what came of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatTurn {
    pub doc: DocId,
    /// 0-based.
    pub page: usize,
    pub selected: String,
    pub question: String,
    pub outcome: Outcome,
}

/// Where the key in use comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeySource {
    /// Not looked up yet.
    Unknown,
    /// No key: AI is off.
    None,
    /// The system keychain, or this session's memory where there is no keychain.
    Stored(Store),
    /// `PDFCRAFT_AI_API_KEY`.
    Environment,
}

struct Pending {
    receiver: Receiver<Result<Answer, AiError>>,
    cancel: CancelToken,
    turn: ChatTurn,
}

/// The panel's state. The API key is held in memory once looked up, and wiped when it is removed
/// or the app quits.
pub struct AiState {
    pub config: AiConfig,
    /// Send the selected page's text with the question.
    pub send_page_text: bool,
    pub selection: Option<AiSelection>,
    pub question: String,
    pub turns: Vec<ChatTurn>,
    key: Option<ApiKey>,
    key_source: KeySource,
    key_draft: String,
    show_connection: bool,
    pending: Option<Pending>,
    notice: Option<String>,
    focus_question: bool,
}

impl Default for AiState {
    fn default() -> Self {
        AiState {
            config: AiConfig::default(),
            send_page_text: false,
            selection: None,
            question: String::new(),
            turns: Vec::new(),
            key: None,
            key_source: KeySource::Unknown,
            key_draft: String::new(),
            show_connection: false,
            pending: None,
            notice: None,
            focus_question: false,
        }
    }
}

fn clean_setting(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty() && value.chars().count() <= MAX_SETTING_CHARS && !value.chars().any(char::is_control)).then(|| value.to_string())
}

impl AiState {
    /// The preferences' part: endpoint, model and the page-text choice. Never the key.
    pub(crate) fn to_json(&self) -> Value {
        json!({ "base_url": self.config.base_url, "model": self.config.model, "send_page_text": self.send_page_text })
    }

    /// Reads [`Self::to_json`]; anything missing or unusable keeps what is set.
    pub(crate) fn restore(&mut self, v: &Value) {
        if let Some(url) = v["base_url"].as_str().and_then(clean_setting) {
            self.config.base_url = url;
        }
        if let Some(model) = v["model"].as_str().and_then(clean_setting) {
            self.config.model = model;
        }
        if let Some(on) = v["send_page_text"].as_bool() {
            self.send_page_text = on;
        }
    }

    /// Sets the endpoint (the control channel and tests); an empty or unusable one restores
    /// OpenAI's.
    pub(crate) fn set_base_url(&mut self, value: &str) {
        self.config.base_url = clean_setting(value).unwrap_or_else(|| AiConfig::default().base_url);
    }

    /// Sets the model; an empty or unusable one restores the default.
    pub(crate) fn set_model(&mut self, value: &str) {
        self.config.model = clean_setting(value).unwrap_or_else(|| AiConfig::default().model);
    }

    /// Where the key comes from (after it was looked up), for the panel and for tests.
    pub fn key_source(&self) -> KeySource {
        self.key_source
    }

    /// Whether AI is on: a key is set.
    pub fn is_on(&self) -> bool {
        self.key.is_some()
    }

    /// A request is in flight.
    pub fn is_asking(&self) -> bool {
        self.pending.is_some()
    }

    /// The last thing the panel had to say about the key or a request that could not start.
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// Looks the key up: the environment's, else the keychain's. A refusal by the keychain is
    /// reported, not hidden.
    pub fn load_key(&mut self) {
        match secret::resolve(None) {
            Ok(Some(key)) => {
                self.key_source = if secret::from_env().is_some() { KeySource::Environment } else { KeySource::Stored(secret::store_kind()) };
                self.key = Some(key);
            }
            Ok(None) => {
                self.key = None;
                self.key_source = KeySource::None;
            }
            Err(e) => {
                self.key = None;
                self.key_source = KeySource::None;
                self.notice = Some(e.to_string());
            }
        }
        self.show_connection |= self.key.is_none();
    }

    /// Saves the key typed in the panel (or given by a test) and uses it.
    pub fn save_key(&mut self, text: &str) {
        match secret::save(text) {
            Ok(()) => {
                self.key = ApiKey::new(text);
                self.key_source = KeySource::Stored(secret::store_kind());
                self.notice = None;
            }
            Err(e) => self.notice = Some(e.to_string()),
        }
        self.key_draft.clear();
    }

    /// Removes the stored key; AI is off again (a key from the environment stays in use).
    pub fn remove_key(&mut self) {
        if let Err(e) = secret::delete() {
            self.notice = Some(e.to_string());
            return;
        }
        self.notice = None;
        self.key = None;
        self.key_source = KeySource::Unknown;
        self.load_key();
    }
}

/// What the panel asks the app to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PanelAction {
    /// Look up the key.
    LoadKey,
    Ask,
    Cancel,
    SaveKey(String),
    RemoveKey,
    /// Go to this page, 0-based.
    GoTo(usize),
    /// Take the text selected on the pages as the one to ask about.
    UseSelection,
    ClearConversation,
}

/// The Ask AI panel.
pub(crate) fn panel(ui: &mut egui::Ui, t: &Tokens, ai: &mut AiState, doc: DocId, page_count: usize) -> Option<PanelAction> {
    let mut action = None;
    if ai.key_source == KeySource::Unknown {
        action = Some(PanelAction::LoadKey);
    }
    let on = ai.is_on();
    if !on && ai.key_source != KeySource::Unknown {
        ui.label(egui::RichText::new(tl!("AI is off until you add an API key.")).color(t.text));
        ui.add_space(4.0);
    }
    // The endpoint, the model and the key.
    ui.toggle_value(&mut ai.show_connection, tl!("Connection"));
    if ai.show_connection {
        connection(ui, t, ai, &mut action);
    }
    ui.add_space(6.0);
    if let Some(notice) = ai.notice.clone() {
        ui.add(egui::Label::new(egui::RichText::new(notice).color(egui::Color32::from_rgb(0xD1, 0x3B, 0x3B))).wrap());
        ui.add_space(4.0);
    }
    // The conversation takes what the question box and the selection leave.
    let footer = 190.0;
    let height = (ui.available_height() - footer).max(80.0);
    egui::ScrollArea::vertical().id_salt("ai-conversation").max_height(height).auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
        if ai.turns.is_empty() && !ai.is_asking() {
            ui.label(egui::RichText::new(tl!("Select text on a page, then choose Ask AI.")).color(t.text_muted));
        }
        for turn in ai.turns.iter().filter(|turn| turn.doc == doc) {
            exchange(ui, t, turn, page_count, &mut action);
            ui.add_space(8.0);
        }
        if ai.is_asking() {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(16.0));
                ui.label(egui::RichText::new(tl!("Asking…")).color(t.text_muted));
            });
        }
    });
    ui.separator();
    ask_box(ui, t, ai, doc, on, &mut action);
    action
}

/// Endpoint, model and key.
fn connection(ui: &mut egui::Ui, t: &Tokens, ai: &mut AiState, action: &mut Option<PanelAction>) {
    egui::Grid::new("ai-connection-grid").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
        // Each field is named by its label, for screen readers.
        let label = ui.label(tl!("Endpoint"));
        ui.add(egui::TextEdit::singleline(&mut ai.config.base_url).desired_width(f32::INFINITY).hint_text(pdfcraft_assist::chat::DEFAULT_BASE_URL))
            .labelled_by(label.id);
        ui.end_row();
        let label = ui.label(tl!("Model"));
        ui.add(egui::TextEdit::singleline(&mut ai.config.model).desired_width(f32::INFINITY).hint_text(pdfcraft_assist::chat::DEFAULT_MODEL))
            .labelled_by(label.id);
        ui.end_row();
        let label = ui.label(tl!("API key"));
        ui.add(egui::TextEdit::singleline(&mut ai.key_draft).password(true).desired_width(f32::INFINITY)).labelled_by(label.id);
        ui.end_row();
    });
    if let Err(why) = ai.config.endpoint() {
        ui.add(egui::Label::new(egui::RichText::new(why.to_string()).color(egui::Color32::from_rgb(0xD1, 0x3B, 0x3B)).small()).wrap());
    }
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        let can_save = ApiKey::new(&ai.key_draft).is_some();
        if ui.add_enabled(can_save, egui::Button::new(tl!("Save key"))).clicked() {
            *action = Some(PanelAction::SaveKey(std::mem::take(&mut ai.key_draft)));
        }
        let removable = matches!(ai.key_source, KeySource::Stored(_));
        if ui.add_enabled(removable, egui::Button::new(tl!("Remove key"))).clicked() {
            *action = Some(PanelAction::RemoveKey);
        }
    });
    let status = match ai.key_source {
        KeySource::Stored(Store::Keychain) => Some(tl!("The key is kept in your system keychain.")),
        KeySource::Stored(Store::SessionOnly) => Some(tl!("This system has no keychain here: the key is kept until you quit PdfCraft.")),
        KeySource::Environment => Some(tl!("Using the key from PDFCRAFT_AI_API_KEY.")),
        KeySource::None | KeySource::Unknown => None,
    };
    if let Some(status) = status {
        ui.add(egui::Label::new(egui::RichText::new(status).color(t.text_muted).small()).wrap());
    }
    ui.add(egui::Label::new(egui::RichText::new(tl!("Asking sends the selected text to this endpoint.")).color(t.text_muted).small()).wrap());
}

/// One question and its answer, with the page citations as links.
fn exchange(ui: &mut egui::Ui, t: &Tokens, turn: &ChatTurn, page_count: usize, action: &mut Option<PanelAction>) {
    egui::Frame::NONE.fill(t.hover).corner_radius(egui::CornerRadius::same(6)).inner_margin(egui::Margin::same(8)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            if ui.link(crate::i18n::fmt(tl!("Page {p}"), &[("p", &(turn.page + 1).to_string())])).clicked() {
                *action = Some(PanelAction::GoTo(turn.page));
            }
        });
        ui.add(egui::Label::new(egui::RichText::new(clip(&turn.selected, SELECTION_PREVIEW_CHARS)).color(t.text_muted).italics().small()).wrap());
    });
    let question = turn.question.trim();
    if !question.is_empty() {
        ui.add(egui::Label::new(egui::RichText::new(question).font(theme::semibold(13.0))).wrap());
    }
    match &turn.outcome {
        Outcome::Answer { text, model } => {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                for segment in cite::segments(text, Some(page_count)) {
                    match segment {
                        Segment::Text(text) => {
                            ui.add(egui::Label::new(text).wrap());
                        }
                        Segment::Cite { text, pages } => {
                            if let [page] = pages.as_slice() {
                                if ui.link(text).clicked() {
                                    *action = Some(PanelAction::GoTo(page.saturating_sub(1)));
                                }
                            } else {
                                for page in pages {
                                    if ui.link(format!("[p. {page}]")).clicked() {
                                        *action = Some(PanelAction::GoTo(page.saturating_sub(1)));
                                    }
                                }
                            }
                        }
                    }
                }
            });
            if !model.is_empty() {
                ui.label(egui::RichText::new(model).color(t.text_faint).small());
            }
        }
        Outcome::Failed(why) => {
            ui.add(egui::Label::new(egui::RichText::new(why).color(egui::Color32::from_rgb(0xD1, 0x3B, 0x3B))).wrap());
        }
        Outcome::Cancelled => {
            ui.label(egui::RichText::new(tl!("Cancelled")).color(t.text_muted));
        }
    }
}

/// The selection, the question and the buttons.
fn ask_box(ui: &mut egui::Ui, t: &Tokens, ai: &mut AiState, doc: DocId, on: bool, action: &mut Option<PanelAction>) {
    let selection = ai.selection.clone().filter(|s| s.doc == doc);
    match &selection {
        Some(selection) => {
            ui.horizontal(|ui| {
                if ui.link(crate::i18n::fmt(tl!("Page {p}"), &[("p", &(selection.page + 1).to_string())])).clicked() {
                    *action = Some(PanelAction::GoTo(selection.page));
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.small_button(tl!("Use selection")).clicked() {
                        *action = Some(PanelAction::UseSelection);
                    }
                });
            });
            ui.add(
                egui::Label::new(egui::RichText::new(clip(&selection.text, SELECTION_PREVIEW_CHARS)).color(t.text_muted).italics().small()).wrap(),
            );
        }
        None => {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(tl!("Nothing selected")).color(t.text_muted));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.small_button(tl!("Use selection")).clicked() {
                        *action = Some(PanelAction::UseSelection);
                    }
                });
            });
        }
    }
    let question_id = egui::Id::unique("ai-question");
    let edit = egui::TextEdit::multiline(&mut ai.question)
        .id(question_id)
        .desired_rows(2)
        .desired_width(f32::INFINITY)
        .hint_text(tl!("Ask about the selection"));
    let response = ui.add(edit);
    if std::mem::take(&mut ai.focus_question) {
        response.request_focus();
    }
    let submitted = response.has_focus() && ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::Enter));
    let mut page_text = ai.send_page_text;
    if ui.add(widgets::checkbox(&mut page_text, tl!("Send this page's text too"))).changed() {
        ai.send_page_text = page_text;
    }
    ui.horizontal(|ui| {
        if ai.is_asking() {
            if widgets::pill_button(ui, tl!("Stop"), true).clicked() {
                *action = Some(PanelAction::Cancel);
            }
        } else {
            let ready = on && selection.is_some();
            let clicked = ui.add_enabled_ui(ready, |ui| widgets::pill_button(ui, tl!("Ask"), true)).inner.clicked();
            if ready && (clicked || submitted) {
                *action = Some(PanelAction::Ask);
            }
        }
        if !ai.turns.is_empty() && ui.button(tl!("Clear")).clicked() {
            *action = Some(PanelAction::ClearConversation);
        }
    });
}

/// `text` cut to `max` characters, with an ellipsis when it was cut.
fn clip(text: &str, max: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() { format!("{head}…") } else { head }
}

impl PdfCraftApp {
    /// Ask AI about the text selected in tab `index`: opens the panel with the selection ready,
    /// and asks nothing yet.
    pub(crate) fn ask_ai_about_selection(&mut self, index: usize) {
        let Some(view) = self.views.get(index) else { return };
        let Some(doc) = self.session.get(view.id) else { return };
        if !doc.allows_copying() {
            self.notify(crate::copy_refused());
            return;
        }
        // With nothing selected (the palette's Ask AI) the panel opens to say so, and to set up.
        if let Some((page, text, page_text)) = view.selection_for_ai() {
            self.ai.selection = Some(AiSelection { doc: doc.id, page, text, page_text });
            self.ai.focus_question = true;
        }
        self.choose_right_panel(Some(RightPanel::Assistant));
        if self.ai.key_source == KeySource::Unknown {
            self.ai.load_key();
        }
    }

    /// What the Ask AI panel asked for.
    pub(crate) fn ai_action(&mut self, index: usize, action: PanelAction) {
        match action {
            PanelAction::LoadKey => self.ai.load_key(),
            PanelAction::SaveKey(text) => self.ai.save_key(&text),
            PanelAction::RemoveKey => self.ai.remove_key(),
            PanelAction::UseSelection => self.ask_ai_about_selection(index),
            PanelAction::GoTo(page) => {
                if let Some(view) = self.views.get_mut(index) {
                    view.go_to_page(page);
                }
            }
            PanelAction::Ask => self.ask_ai(),
            PanelAction::Cancel => self.cancel_ai(),
            PanelAction::ClearConversation => {
                self.ai.turns.clear();
                self.ai.notice = None;
            }
        }
    }

    /// Sends the question in the panel about the chosen selection. Does nothing while one is in
    /// flight, with no key, or with nothing selected.
    pub fn ask_ai(&mut self) {
        let ai = &mut self.ai;
        if ai.pending.is_some() {
            return;
        }
        let Some(selection) = ai.selection.clone() else { return };
        let Some(key) = ai.key.clone() else {
            ai.notice = Some(tl!("AI is off until you add an API key.").to_string());
            return;
        };
        if self.session.get(selection.doc).is_none_or(|d| !d.allows_copying()) {
            self.notify(crate::copy_refused());
            return;
        }
        let ai = &mut self.ai;
        let history: Vec<Turn> = ai
            .turns
            .iter()
            .filter(|t| t.doc == selection.doc)
            .filter_map(|t| match &t.outcome {
                Outcome::Answer { text, .. } => Some(Turn { question: t.question.clone(), answer: text.clone() }),
                _ => None,
            })
            .collect();
        let question = Question {
            question: ai.question.trim().to_string(),
            selected: selection.text.clone(),
            page: selection.page + 1,
            context: ai.send_page_text.then(|| selection.page_text.clone()).flatten(),
            history,
        };
        let turn = ChatTurn {
            doc: selection.doc,
            page: selection.page,
            selected: selection.text,
            question: question.question.clone(),
            outcome: Outcome::Cancelled,
        };
        let config = ai.config.clone();
        ai.notice = None;
        let cancel = CancelToken::new();
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker_cancel = cancel.clone();
        let work = move || {
            let result = match pdfcraft_engine::guard(|| pdfcraft_assist::ask(&config, &key, &question, &worker_cancel)) {
                Ok(result) => result,
                Err(_) => Err(AiError::Network("the request failed unexpectedly".into())),
            };
            let _ = sender.send(result);
        };
        let ctx = self.ctx.clone();
        let inline = self.run_inline || ctx.is_none();
        #[cfg(not(target_arch = "wasm32"))]
        if !inline {
            let repaint = ctx.clone();
            let spawned = std::thread::Builder::new().name("pdfcraft-ask-ai".into()).spawn(move || {
                work();
                if let Some(ctx) = repaint {
                    ctx.request_repaint();
                }
            });
            if spawned.is_err() {
                self.ai.notice = Some(tl!("Couldn't start the request.").to_string());
                return;
            }
            self.ai.pending = Some(Pending { receiver, cancel, turn });
            return;
        }
        let _ = inline;
        work();
        self.ai.pending = Some(Pending { receiver, cancel, turn });
        self.poll_ai();
    }

    /// Stops waiting for the answer in flight. The request already sent ends by itself; its answer
    /// is dropped.
    pub fn cancel_ai(&mut self) {
        if let Some(pending) = self.ai.pending.take() {
            pending.cancel.cancel();
            self.push_turn(ChatTurn { outcome: Outcome::Cancelled, ..pending.turn });
        }
    }

    fn push_turn(&mut self, turn: ChatTurn) {
        self.ai.turns.push(turn);
        let extra = self.ai.turns.len().saturating_sub(MAX_TURNS);
        self.ai.turns.drain(..extra);
    }

    /// Takes in the answer when it has come. Called every frame.
    pub(crate) fn poll_ai(&mut self) {
        let Some(pending) = self.ai.pending.take() else { return };
        let result = match pending.receiver.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => {
                self.ai.pending = Some(pending);
                return;
            }
            Err(TryRecvError::Disconnected) => Err(AiError::Network("the request stopped".into())),
        };
        let outcome = match result {
            Ok(answer) => {
                // The question was answered: the box is ready for the next.
                self.ai.question.clear();
                Outcome::Answer { text: answer.text, model: answer.model }
            }
            Err(AiError::Cancelled) => Outcome::Cancelled,
            Err(e) => Outcome::Failed(e.to_string()),
        };
        self.push_turn(ChatTurn { outcome, ..pending.turn });
    }
}
