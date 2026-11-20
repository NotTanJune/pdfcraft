//! The card beside the pointer when it rests on a link.
//!
//! - A link to a page of the same document: its label, and after a short rest a small picture of
//!   the part of that page the link goes to ([`pdfcraft_engine::link_preview`]), drawn off the UI
//!   thread with the page renderer.
//! - A web link (an annotation, or an address written in the page text): the whole address and the
//!   host as the browser will connect to it, at once. With Preferences ▸ "Fetch web link previews"
//!   (off by default), after a longer rest, also the page's title and description, read by
//!   [`pdfcraft_assist::preview`] off the UI thread: a bounded GET with no cookies and no
//!   scripts, remembered per address for the session. The address is never contacted while the
//!   setting is off, and never by a pointer that only passes over the link.
//!
//! The pages decide what lies under the pointer (`Hit`) and claim the cursor and the tip, as for
//! every page module; this only draws what the winning claim says, from `canvas`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};

use egui::{Pos2, Vec2, vec2};
use pdfcraft_assist::preview::{FetchPolicy, LinkCard, PreviewError};
use pdfcraft_engine::DocId;
use pdfcraft_render::{DestView, LinkTarget, PageInfo, RenderedPage};

use crate::PdfCraftApp;

/// How long the pointer rests on a page link before its picture is drawn, in seconds.
const PICTURE_AFTER: f64 = 0.3;
/// How long it rests on a web link before its page is fetched. A pointer sweeping across a page
/// of links fetches nothing.
const FETCH_AFTER: f64 = 0.6;
/// The most pictures and cards kept.
const MAX_REGIONS: usize = 24;
const MAX_CARDS: usize = 64;
/// The card's text width, and the largest the picture is shown, in points.
const CARD_WIDTH: f32 = 340.0;
const PICTURE_MAX: Vec2 = vec2(340.0, 200.0);

/// A web address a link goes to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WebLink {
    /// The address, normalised as the browser sends it. This is what is fetched.
    pub url: String,
    /// The host as the browser connects to it (international hosts in punycode).
    pub host: String,
    /// The host mixes alphabets, a common way to imitate another site's name.
    pub mixed_scripts: bool,
}

/// What the link under the pointer goes to, as far as a card can show it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Target {
    /// A page of this document, 0-based, and where on it.
    Page {
        page: usize,
        view: DestView,
    },
    Web(WebLink),
}

impl Target {
    /// The card for a web address written as `uri`: `None` unless it is an `http` or `https`
    /// address that passes the check every address from a document passes.
    pub(crate) fn of_uri(uri: &str) -> Option<Target> {
        let web = pdfcraft_engine::link_preview::web_target(uri)?;
        Some(Target::Web(WebLink { url: web.url, host: web.host.ascii, mixed_scripts: web.host.mixed_scripts }))
    }

    /// The card for a link annotation's action, if it has one.
    pub(crate) fn of_link(target: &LinkTarget) -> Option<Target> {
        match target {
            LinkTarget::Page(page, view) => Some(Target::Page { page: *page, view: *view }),
            LinkTarget::Uri(uri) => Self::of_uri(uri),
            _ => None,
        }
    }
}

/// What the winning claim on the pages says is under the pointer.
#[derive(Clone, Debug)]
pub(crate) struct Hover {
    pub pos: Pos2,
    /// What the plain tip said (the link's address, or "Go to page N").
    pub tip: String,
    pub target: Target,
}

/// A picture of part of a page: the document's bytes (by address, so an edit gives a new key), the
/// page, the area in points and the width asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct RegionKey {
    doc: DocId,
    bytes: usize,
    page: usize,
    rect: [u32; 4],
    width: u32,
}

enum Region {
    Pending(Receiver<RenderedPage>),
    Ready(egui::TextureHandle),
    Failed,
}

enum Card {
    Pending(Receiver<Result<LinkCard, PreviewError>>),
    Ready(LinkCard),
    Failed(String),
}

/// The session's previews: pictures of page regions and fetched cards, and what the pointer rests
/// on now.
#[derive(Default)]
pub(crate) struct Previews {
    hover: Option<(Target, f64)>,
    regions: HashMap<RegionKey, Region>,
    region_order: Vec<RegionKey>,
    cards: HashMap<String, Card>,
    card_order: Vec<String>,
    /// What a fetch may reach. Strict unless a test says otherwise.
    policy: FetchPolicy,
}

/// What the cards hold, for tests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PreviewReport {
    /// The pointer rests on a link that has a card.
    pub hovering: bool,
    /// Pictures of page regions drawn, and still being drawn.
    pub pictures_ready: usize,
    pub pictures_pending: usize,
    /// Fetched cards, as (address asked for, title, description).
    pub cards: Vec<(String, Option<String>, Option<String>)>,
    /// Addresses whose fetch failed, with why.
    pub failed: Vec<(String, String)>,
    /// Addresses fetched or being fetched, in the order started.
    pub fetches_started: Vec<String>,
}

impl Previews {
    /// Takes in what the workers finished.
    fn poll(&mut self, ctx: &egui::Context) {
        for (key, region) in &mut self.regions {
            let Region::Pending(receiver) = region else { continue };
            let done = match receiver.try_recv() {
                Ok(out) => Some(out),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => {
                    *region = Region::Failed;
                    continue;
                }
            };
            let Some(out) = done else { continue };
            *region = if out.error.is_some() || out.width == 0 || out.height == 0 {
                Region::Failed
            } else {
                let image = egui::ColorImage::from_rgba_premultiplied([out.width as usize, out.height as usize], &out.rgba);
                Region::Ready(ctx.load_texture(format!("link-preview-{}-{}", key.page, key.bytes), image, egui::TextureOptions::LINEAR))
            };
        }
        for card in self.cards.values_mut() {
            let Card::Pending(receiver) = card else { continue };
            *card = match receiver.try_recv() {
                Ok(Ok(found)) => Card::Ready(found),
                Ok(Err(e)) => Card::Failed(e.to_string()),
                Err(TryRecvError::Empty) => continue,
                Err(TryRecvError::Disconnected) => Card::Failed("the fetch stopped".to_string()),
            };
        }
    }

    fn pending(&self) -> bool {
        self.regions.values().any(|r| matches!(r, Region::Pending(_))) || self.cards.values().any(|c| matches!(c, Card::Pending(_)))
    }

    /// Forgets the oldest entries past the caps.
    fn trim(&mut self) {
        while self.region_order.len() > MAX_REGIONS {
            let old = self.region_order.remove(0);
            self.regions.remove(&old);
        }
        while self.card_order.len() > MAX_CARDS {
            let old = self.card_order.remove(0);
            self.cards.remove(&old);
        }
    }
}

/// The page the pointer rests on, as one picture: where it comes from.
struct Source {
    id: DocId,
    bytes: Arc<Vec<u8>>,
    password: Option<String>,
    page: PageInfo,
}

impl PdfCraftApp {
    /// Draws the card for `hover` beside the pointer, or forgets what the pointer rested on when
    /// there is none. Called once a frame, after the pages, with the tab that is showing.
    pub(crate) fn show_link_preview(&mut self, ctx: &egui::Context, index: usize, hover: Option<Hover>) {
        let Some(hover) = hover else {
            self.link_previews.hover = None;
            self.link_previews.poll(ctx);
            return;
        };
        let now = ctx.input(|i| i.time);
        let since = match &self.link_previews.hover {
            Some((target, since)) if *target == hover.target => *since,
            _ => {
                self.link_previews.hover = Some((hover.target.clone(), now));
                now
            }
        };
        let rested = now - since;
        self.link_previews.poll(ctx);
        let inline = self.run_inline || self.ctx.is_none();
        let mut picture: Option<RegionKey> = None;
        let mut card_url: Option<String> = None;
        match &hover.target {
            Target::Page { page, view } => {
                if rested >= PICTURE_AFTER
                    && let Some(source) = self.preview_source(index, *page)
                {
                    let rect = pdfcraft_engine::link_preview::region(&source.page, *view);
                    let ppp = ctx.pixels_per_point().max(1.0);
                    let width = (PICTURE_MAX.x * ppp).round().clamp(64.0, 1600.0) as u32;
                    let key =
                        RegionKey { doc: source.id, bytes: Arc::as_ptr(&source.bytes) as usize, page: *page, rect: rect.map(f32::to_bits), width };
                    self.link_previews.start_region(ctx, key, &source, rect, inline);
                    picture = Some(key);
                } else if rested < PICTURE_AFTER {
                    ctx.request_repaint_after(std::time::Duration::from_secs_f64(PICTURE_AFTER - rested + 0.01));
                }
            }
            Target::Web(web) if self.fetch_link_previews => {
                if rested >= FETCH_AFTER {
                    self.link_previews.start_card(ctx, &web.url, inline);
                    card_url = Some(web.url.clone());
                } else {
                    ctx.request_repaint_after(std::time::Duration::from_secs_f64(FETCH_AFTER - rested + 0.01));
                }
            }
            Target::Web(_) => {}
        }
        if self.link_previews.pending() {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
        self.link_previews.trim();
        draw(ctx, &hover, &self.link_previews, picture, card_url.as_deref());
    }

    /// The document of tab `index` and its 0-based `page`, for drawing a picture of it.
    fn preview_source(&self, index: usize, page: usize) -> Option<Source> {
        let doc = self.session.get(self.views.get(index)?.id)?;
        Some(Source {
            id: doc.id,
            bytes: doc.bytes.clone(),
            password: doc.password.as_deref().map(str::to_string),
            page: doc.info.pages.get(page)?.clone(),
        })
    }

    /// What the cards hold now (tests).
    #[doc(hidden)]
    pub fn link_preview_report(&self) -> PreviewReport {
        let p = &self.link_previews;
        let mut report = PreviewReport { hovering: p.hover.is_some(), fetches_started: p.card_order.clone(), ..PreviewReport::default() };
        for region in p.regions.values() {
            match region {
                Region::Ready(_) => report.pictures_ready += 1,
                Region::Pending(_) => report.pictures_pending += 1,
                Region::Failed => {}
            }
        }
        for (url, card) in &p.cards {
            match card {
                Card::Ready(c) => report.cards.push((url.clone(), c.meta.title.clone(), c.meta.description.clone())),
                Card::Failed(why) => report.failed.push((url.clone(), why.clone())),
                Card::Pending(_) => {}
            }
        }
        report
    }

    /// Lets link previews fetch from this computer's own addresses, so a test can serve a page
    /// from `127.0.0.1` (tests only: the app never does).
    #[doc(hidden)]
    pub fn allow_private_link_previews(&mut self) {
        self.link_previews.policy = FetchPolicy { allow_private_hosts: true };
    }
}

impl Previews {
    fn start_region(&mut self, ctx: &egui::Context, key: RegionKey, source: &Source, rect: [f32; 4], inline: bool) {
        if self.regions.contains_key(&key) {
            return;
        }
        let (bytes, password, page, width) = (source.bytes.clone(), source.password.clone(), key.page, key.width);
        let (sender, receiver) = std::sync::mpsc::channel();
        let work = move || {
            let out = pdfcraft_engine::link_preview::render_region(bytes, password.as_deref(), page, rect, width, true);
            let _ = sender.send(out);
        };
        self.region_order.push(key);
        if inline {
            work();
            self.regions.insert(key, Region::Pending(receiver));
            self.poll(ctx);
            return;
        }
        let repaint = ctx.clone();
        let spawned = std::thread::Builder::new().name("pdfcraft-link-region".into()).spawn(move || {
            work();
            repaint.request_repaint();
        });
        self.regions.insert(key, if spawned.is_ok() { Region::Pending(receiver) } else { Region::Failed });
    }

    fn start_card(&mut self, ctx: &egui::Context, url: &str, inline: bool) {
        if self.cards.contains_key(url) {
            return;
        }
        let (sender, receiver) = std::sync::mpsc::channel();
        let (target, policy) = (url.to_string(), self.policy);
        let work = move || {
            let found = match pdfcraft_engine::guard(|| pdfcraft_assist::preview::fetch(&target, &policy)) {
                Ok(found) => found,
                Err(_) => Err(PreviewError::Network("the fetch failed".into())),
            };
            let _ = sender.send(found);
        };
        self.card_order.push(url.to_string());
        if inline {
            work();
            self.cards.insert(url.to_string(), Card::Pending(receiver));
            self.poll(ctx);
            return;
        }
        let repaint = ctx.clone();
        let spawned = std::thread::Builder::new().name("pdfcraft-link-card".into()).spawn(move || {
            work();
            repaint.request_repaint();
        });
        self.cards.insert(url.to_string(), if spawned.is_ok() { Card::Pending(receiver) } else { Card::Failed("could not start".to_string()) });
    }
}

/// The card: the tip, then what the pointer's rest has brought.
fn draw(ctx: &egui::Context, hover: &Hover, previews: &Previews, picture: Option<RegionKey>, card_url: Option<&str>) {
    // Click-through, as the plain tip is: the pages lose the pointer under anything that isn't.
    egui::Area::new(egui::Id::unique("canvas-hover")).order(egui::Order::Tooltip).interactable(false).fixed_pos(hover.pos + vec2(14.0, 16.0)).show(
        ctx,
        |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_max_width(CARD_WIDTH);
                match &hover.target {
                    Target::Page { .. } => {
                        ui.label(&hover.tip);
                        match picture.and_then(|key| previews.regions.get(&key)) {
                            Some(Region::Ready(texture)) => {
                                let ppp = ui.ctx().pixels_per_point();
                                let natural = texture.size_vec2() / ppp;
                                let fit = (PICTURE_MAX.x / natural.x).min(PICTURE_MAX.y / natural.y).min(1.0);
                                let size = natural * fit;
                                let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
                                ui.painter().image(
                                    texture.id(),
                                    rect,
                                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                                    egui::Color32::WHITE,
                                );
                                ui.painter().rect_stroke(rect, 0.0, ui.visuals().widgets.noninteractive.bg_stroke, egui::StrokeKind::Outside);
                            }
                            Some(Region::Pending(_)) => {
                                ui.add(egui::Spinner::new().size(16.0));
                            }
                            Some(Region::Failed) | None => {}
                        }
                    }
                    Target::Web(web) => {
                        ui.add(egui::Label::new(egui::RichText::new(&hover.tip).monospace().small()).wrap());
                        ui.label(egui::RichText::new(&web.host).strong());
                        if web.mixed_scripts {
                            let warning =
                                tl!("This web address mixes letters from different alphabets, a common way to imitate another site's name.");
                            ui.add(egui::Label::new(egui::RichText::new(warning).color(egui::Color32::from_rgb(0xD1, 0x3B, 0x3B)).small()).wrap());
                        }
                        if let Some(card) = card_url.and_then(|url| previews.cards.get(url)) {
                            match card {
                                Card::Ready(found) => {
                                    ui.separator();
                                    if let Some(site) = found.meta.site_name.as_deref().filter(|s| Some(*s) != found.meta.title.as_deref()) {
                                        ui.label(egui::RichText::new(site).small().weak());
                                    }
                                    if let Some(title) = &found.meta.title {
                                        ui.add(egui::Label::new(egui::RichText::new(title).strong()).wrap());
                                    }
                                    if let Some(description) = &found.meta.description {
                                        ui.add(egui::Label::new(description).wrap());
                                    }
                                    if found.meta.title.is_none() && found.meta.description.is_none() {
                                        ui.label(egui::RichText::new(tl!("No preview available")).weak());
                                    }
                                }
                                Card::Pending(_) => {
                                    ui.add(egui::Spinner::new().size(16.0));
                                }
                                Card::Failed(_) => {
                                    ui.label(egui::RichText::new(tl!("No preview available")).weak());
                                }
                            }
                        }
                    }
                }
            });
        },
    );
}
