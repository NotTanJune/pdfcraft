//! The picture a link shows when the pointer rests on it: the part of the destination page the
//! link goes to, drawn small.
//!
//! [`region`] picks the part of the page, in the page's displayed coordinates (points from the
//! top-left, after `/Rotate`): the area at the destination's point (`/XYZ`, `/FitH`, `/FitV`), the
//! rectangle a `/FitR` names, or the whole page for `/Fit`. [`render_region`] draws that part with
//! the page renderer, at a width the caller chooses, and returns the pixels. Both are plain
//! functions of the page's data, so the viewer's hover card and the `link_preview` tool show the
//! same thing. Blocking: callers that animate (the viewer) run [`render_region`] off their UI
//! thread.

use std::sync::Arc;

use pdfcraft_render::{DestView, PageInfo, PageRenderer, RenderConfig, RenderRequest, RenderedPage, RequestKind, Tile};

/// The size of the area shown around a destination point, in page points.
pub const REGION_WIDTH: f32 = 340.0;
pub const REGION_HEIGHT: f32 = 190.0;

/// How far above and left of the destination point the area starts, so the target is not at the
/// very edge of the picture.
const LEAD: f32 = 14.0;
/// The smallest area shown for a `/FitR` rectangle.
const MIN_FIT_R: [f32; 2] = [90.0, 50.0];

/// The part of `page` a link with `view` goes to: `[x0, y0, x1, y1]` in displayed points from the
/// page's top-left, always inside the page and never empty. A page with no usable size gives a
/// 1 by 1 point rectangle at its corner.
pub fn region(page: &PageInfo, view: DestView) -> [f32; 4] {
    let (w, h) = (page.width, page.height);
    if !(w.is_finite() && h.is_finite() && w >= 1.0 && h >= 1.0) {
        return [0.0, 0.0, 1.0, 1.0];
    }
    let whole = [0.0, 0.0, w, h];
    // A box `bw` by `bh` whose top-left is `(x, y)`, moved to stay on the page.
    let window = |x: f32, y: f32, bw: f32, bh: f32| {
        let (bw, bh) = (bw.min(w), bh.min(h));
        let x0 = x.clamp(0.0, (w - bw).max(0.0));
        let y0 = y.clamp(0.0, (h - bh).max(0.0));
        [x0, y0, x0 + bw, y0 + bh]
    };
    let at = |x: Option<f32>, y: Option<f32>| {
        let [fx, fy] = page.dest_fraction(x, y, 0);
        (fx.map(|f| f * w), fy.map(|f| f * h))
    };
    match view {
        DestView::Fit => whole,
        DestView::Top => window(0.0, 0.0, REGION_WIDTH, REGION_HEIGHT),
        DestView::Xyz { left, top, .. } => {
            let (x, y) = at(left, top);
            window(x.map_or(0.0, |x| x - LEAD), y.map_or(0.0, |y| y - LEAD), REGION_WIDTH, REGION_HEIGHT)
        }
        DestView::FitH { top } => {
            let (_, y) = at(None, top);
            window(0.0, y.map_or(0.0, |y| y - LEAD), REGION_WIDTH, REGION_HEIGHT)
        }
        DestView::FitV { left } => {
            let (x, _) = at(left, None);
            window(x.map_or(0.0, |x| x - LEAD), 0.0, REGION_WIDTH, REGION_HEIGHT)
        }
        DestView::FitR { rect } => {
            let (ax, ay) = at(Some(rect[0]), Some(rect[3]));
            let (bx, by) = at(Some(rect[2]), Some(rect[1]));
            let (Some(ax), Some(ay), Some(bx), Some(by)) = (ax, ay, bx, by) else { return window(0.0, 0.0, REGION_WIDTH, REGION_HEIGHT) };
            let (x0, y0, x1, y1) = (ax.min(bx), ay.min(by), ax.max(bx), ay.max(by));
            // A little margin, a floor on the size, a ceiling at what a hover card can hold.
            let (bw, bh) = ((x1 - x0 + 16.0).max(MIN_FIT_R[0]).min(w), (y1 - y0 + 16.0).max(MIN_FIT_R[1]).min(h));
            window(x0 - 8.0, y0 - 8.0, bw, bh)
        }
    }
}

/// A web address a link goes to, ready to show and to fetch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebTarget {
    /// The address as the browser would send it: scheme and host lower-cased, an international
    /// host in punycode, characters percent-encoded. Fragment dropped.
    pub url: String,
    /// The host as the browser connects to it, and the warnings that go with it.
    pub host: crate::links::HostDisplay,
}

/// The web address behind `raw`, when it is one a hover card may show and fetch: `http` or
/// `https`, past the same check an address from a document meets before it is opened
/// ([`crate::links::document_url`]). `mailto:` and every other scheme give `None`.
pub fn web_target(raw: &str) -> Option<WebTarget> {
    let checked = crate::links::document_url(raw).ok()?;
    let mut parsed = url::Url::parse(&checked).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    parsed.set_fragment(None);
    let host = crate::links::display_host(parsed.as_str())?;
    Some(WebTarget { url: parsed.to_string(), host })
}

/// Draws `rect` (displayed points, from [`region`]) of page `page` (0-based) of the document whose
/// bytes are `bytes`, scaled so it is `pixel_width` pixels wide. `on_screen` lets installed fonts
/// stand in for CJK fonts a PDF doesn't embed, which is allowed for a picture that is only shown
/// (AGENTS.md §1.4) and never for one that is saved or returned to an agent. `pixels` is empty
/// and `error` set when the page can't be drawn.
pub fn render_region(bytes: Arc<Vec<u8>>, password: Option<&str>, page: usize, rect: [f32; 4], pixel_width: u32, on_screen: bool) -> RenderedPage {
    let config = RenderConfig { password: password.map(Arc::from), ..RenderConfig::default() };
    let config = if on_screen { config.on_screen() } else { config };
    let mut renderer = PageRenderer::new(bytes, config);
    let width = (rect[2] - rect[0]).max(1.0);
    // At most 32 pixels per point (a tiny `/FitR`), at least a sliver; the renderer caps the rest.
    let scale = (pixel_width.clamp(8, 4096) as f32 / width).clamp(0.02, 32.0);
    let tile = Tile {
        x: (rect[0].max(0.0) * scale).floor() as u32,
        y: (rect[1].max(0.0) * scale).floor() as u32,
        w: (width * scale).ceil().max(1.0) as u32,
        h: ((rect[3] - rect[1]).max(1.0) * scale).ceil().max(1.0) as u32,
    };
    renderer.render(RenderRequest { page, kind: RequestKind::Pixels, tile: Some(tile), scale, tag: 0, preview: false })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_web_addresses_are_targets_and_international_hosts_become_punycode() {
        let t = web_target("https://Example.ORG/a b?q=1#frag").unwrap();
        assert_eq!(t.url, "https://example.org/a%20b?q=1");
        assert_eq!(t.host.ascii, "example.org");
        assert_eq!(web_target("http://example.org").unwrap().url, "http://example.org/");
        for raw in [
            "mailto:a@example.org",
            "ftp://example.org/",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "https://",
            "",
            "not a url",
            "https://exa\u{202E}mple.org/",
        ] {
            assert_eq!(web_target(raw), None, "{raw:?}");
        }
        let idn = web_target("https://b\u{fc}cher.example/").unwrap();
        assert_eq!(idn.url, "https://xn--bcher-kva.example/");
        assert!(idn.host.international);
    }

    fn page(width: f32, height: f32, rotation: u16) -> PageInfo {
        let (w, h) = if rotation.is_multiple_of(180) { (width, height) } else { (height, width) };
        PageInfo { width, height, label: "1".into(), crop: [0.0, 0.0, w, h], rotation }
    }

    fn inside(r: [f32; 4], p: &PageInfo) {
        assert!(r[0] >= 0.0 && r[1] >= 0.0 && r[2] <= p.width + 1e-3 && r[3] <= p.height + 1e-3, "{r:?} leaves {}x{}", p.width, p.height);
        assert!(r[2] > r[0] && r[3] > r[1], "{r:?} is empty");
    }

    #[test]
    fn an_xyz_destination_starts_at_its_point() {
        let p = page(612.0, 792.0, 0);
        // User space is y-up: top 700 is 92 points below the page's top edge.
        let r = region(&p, DestView::Xyz { left: Some(100.0), top: Some(700.0), zoom: None });
        inside(r, &p);
        assert!((r[0] - (100.0 - LEAD)).abs() < 0.5 && (r[1] - (92.0 - LEAD)).abs() < 0.5, "{r:?}");
        assert!((r[2] - r[0] - REGION_WIDTH).abs() < 0.5 && (r[3] - r[1] - REGION_HEIGHT).abs() < 0.5);
    }

    #[test]
    fn a_point_near_the_far_corner_slides_the_area_back_onto_the_page() {
        let p = page(612.0, 792.0, 0);
        let r = region(&p, DestView::Xyz { left: Some(600.0), top: Some(5.0), zoom: None });
        inside(r, &p);
        assert!((r[2] - 612.0).abs() < 0.5 && (r[3] - 792.0).abs() < 0.5, "{r:?}");
    }

    #[test]
    fn missing_operands_show_the_top_and_fit_shows_the_whole_page() {
        let p = page(612.0, 792.0, 0);
        assert_eq!(region(&p, DestView::Top), [0.0, 0.0, REGION_WIDTH, REGION_HEIGHT]);
        assert_eq!(region(&p, DestView::Xyz { left: None, top: None, zoom: None })[..2], [0.0, 0.0]);
        assert_eq!(region(&p, DestView::Fit), [0.0, 0.0, 612.0, 792.0]);
        inside(region(&p, DestView::FitH { top: Some(300.0) }), &p);
        inside(region(&p, DestView::FitV { left: Some(300.0) }), &p);
    }

    #[test]
    fn a_fit_r_rectangle_is_shown_with_a_margin() {
        let p = page(612.0, 792.0, 0);
        let r = region(&p, DestView::FitR { rect: [100.0, 600.0, 300.0, 700.0] });
        inside(r, &p);
        assert!(r[0] <= 100.0 && r[2] >= 300.0 && r[1] <= 92.0 && r[3] >= 192.0, "{r:?}");
    }

    #[test]
    fn rotated_and_odd_pages_stay_inside() {
        for rotation in [90, 180, 270] {
            let p = page(612.0, 792.0, rotation);
            for view in [
                DestView::Top,
                DestView::Fit,
                DestView::Xyz { left: Some(50.0), top: Some(50.0), zoom: Some(2.0) },
                DestView::FitR { rect: [10.0, 10.0, 5000.0, 5000.0] },
                DestView::FitH { top: Some(-9e9) },
            ] {
                inside(region(&p, view), &p);
            }
        }
        assert_eq!(region(&page(0.0, 0.0, 0), DestView::Top), [0.0, 0.0, 1.0, 1.0]);
        assert_eq!(region(&page(f32::NAN, 10.0, 0), DestView::Top), [0.0, 0.0, 1.0, 1.0]);
        inside(region(&page(30.0, 20.0, 0), DestView::Xyz { left: Some(f32::NAN), top: Some(f32::INFINITY), zoom: None }), &page(30.0, 20.0, 0));
    }
}
