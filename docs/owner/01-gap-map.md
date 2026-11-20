# 01. Gap map

One row per wishlist item. "Folio today" refers to `reference/folio` at `c72f8cf` (v1.10.9). "What's needed" is judged against the target (macOS + iOS, lightweight, Liquid Glass); a row marked "different architecture" means Folio's Electron stack cannot meet it as is. Library licenses: [V] verified from the project's license file or registry, [U] unverified. Effort is a guess for one person: S (days), M (1 to 3 weeks), L (1 to 2 months), XL (more).

## Conversion

| # | Item | Folio today | What's needed | Candidate approach / libraries (license) | Platform | Risk / effort |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | PDF to Word | Missing | New subsystem | No Apple API. Options: (a) own converter: PDFKit/Vision text runs + layout heuristics → DOCX via a DOCX writer (e.g. `docx-rs`, MIT [U]); (b) pdf2docx (MIT [V]) but it requires PyMuPDF (AGPL or commercial [V]), Python, not on iOS; (c) commercial SDK (Nutrient, Apryse; quote only [V]). | Both | High / L. Layout fidelity is hard; expect "editable text with basic structure", not pixel-perfect |
| 2 | Support for LaTeX | Partial: LaTeX math renders in AI chat only (KaTeX, `src/AiMarkdown.tsx`, `src/chatMath.ts`) | Depends on meaning (see clarifying questions) | Chat math: SwiftMath (MIT [V]) or KaTeX in a small web view (MIT [V]). Compile .tex → PDF: Tectonic (MIT [V], fetches support bundles over HTTP by default [S]; offline bundle size unknown). Typst (Apache-2.0 [V]) is a lighter alternative engine but is not LaTeX | Both (compile: macOS first) | Low for chat math / S. High for .tex compile on iOS / L |
| 3 | Doc to PDF | Partial: .docx/.doc only, via Word (Windows), LibreOffice, or mammoth/textutil (`electron/convert.cjs:151-194`) | Extend (port) | macOS: `NSAttributedString` reads .docx/.doc/.rtf/.odt, then render to PDF (built in, fidelity moderate [U]). iOS: no .docx reader in UIKit [U]; mammoth (BSD-2-Clause [V]) → HTML → `WKWebView.createPDF`, or QuickLook thumbnailing. Word/LibreOffice paths conflict with item 20 | Both | Medium / M. Fidelity of complex Word files is the risk |
| 4 | Excel to PDF | Missing | New subsystem | Read .xlsx with a parser (calamine, MIT [V], Rust; or CoreXLSX, Apache-2.0 [U], Swift) → HTML table → `WKWebView.createPDF`. Reverse (PDF table → Excel): Vision `RecognizeDocumentsRequest` table output (iOS 26+ [V]) → rust_xlsxwriter (Apache-2.0 [V]) | Both | Medium / M one way; High / L both ways |
| 5 | CSV and other classic conversions | Missing (webpage → PDF exists: `electron/webpage.cjs`) | New subsystem | CSV → HTML table → PDF (trivial). Images (JPEG/PNG/HEIC) → PDF via PDFKit `PDFPage(image:)` (built in). PDF → images via PDFKit render. Webpage → PDF via `WKWebView.createPDF` (built in). PDF → text/Markdown via PDFKit text | Both | Low / S per format. Scope needs a fixed list |

## Document operations

| # | Item | Folio today | What's needed | Candidate approach / libraries (license) | Platform | Risk / effort |
| --- | --- | --- | --- | --- | --- | --- |
| 6 | Compress | Missing | New subsystem | PDFKit write options `saveImagesAsJPEGOption` + `optimizeImagesForScreenOption` [V exist; OS version U]. Better control: re-encode images at target DPI with qpdf (Apache-2.0 [V], C++) or lopdf (MIT [V], Rust) + ImageIO. Ghostscript is AGPL [V]: avoid | Both | Medium / M. Target-size ("under 5 MB") needs an iterative loop |
| 7 | OCR, done well | Missing (open PR #21 adds tesseract.js, English only, ~11 MB assets) | New subsystem | Vision `RecognizeDocumentsRequest` (iOS/macOS 26 [V iOS]) or `VNRecognizeTextRequest` (accurate mode). Write an invisible text layer: PDFKit `saveTextFromOCROption` write option exists [V], behavior untested. Tesseract (Apache-2.0 [V]) as fallback only | Both | Low-medium / M. Vision quality is generally better than Tesseract [U, needs our own test set] |
| 8 | Merge | Missing | New subsystem (small) | PDFKit `PDFDocument.insert(_:at:)` [U method name] or lopdf/qpdf. Watch outlines, links, form fields | Both | Low / S |
| 9 | Split | Missing | New subsystem (small) | PDFKit: copy page ranges into new `PDFDocument`; or qpdf `--pages` | Both | Low / S |
| 10 | Change orientation | Partial: view-only rotation (`viewer.pagesRotation`, `src/App.tsx:520-523`), saved in localStorage view state, never written to the file | Extend → real page rotation | PDFKit `PDFPage.rotation` then write [U: persistence to be tested in the spike]; lopdf: set `/Rotate` | Both | Low / S |
| 11 | Change color | Partial: Night reading (CSS invert with image overlay, `src/styles.css:479-483`, `src/nightImages.ts`), annotation colors | Depends on meaning (see clarifying questions) | View-time: Core Image color matrix on rendered tiles. Written to file: render pages through Core Image and rebuild (loses text) or recolor content-stream operators with lopdf (keeps text, harder) | Both | Low (view) / S; High (in-file, text-preserving) / L |
| 12 | Flatten | Missing. "Save a copy" uses pdf.js `saveDocument()`, which writes real annotation objects, not flattened (`src/App.tsx:717`; smoke test asserts `/Subtype /Highlight`, `scripts/smoke-test.mjs:203`) | New subsystem (small) | PDFKit `burnInAnnotationsOption` [V exists]. Form-field flattening: verify burn-in covers widgets [U]. Fallback: render annotation appearance streams into page content with qpdf (`--flatten-annotations`) | Both | Low / S |
| 13 | AI enhance (clearer, sharper) | Missing | New subsystem | Core Image `CIDocumentEnhancer` [S] + unsharp mask; Vision perspective correction for photos; on-device super-resolution Core ML model (license per model [U]). Output: replace page image, keep OCR text layer | Both | Medium / M. "AI" upscaling models add size; keep optional |

## Reading, links and AI

| # | Item | Folio today | What's needed | Candidate approach / libraries (license) | Platform | Risk / effort |
| --- | --- | --- | --- | --- | --- | --- |
| 14 | Link preview in-app | Missing. External links go to the system browser after an "Open link?" dialog (`electron/main.cjs:361-382`). Internal links jump (`PDFLinkService`, `src/App.tsx:194`); no preview | New subsystem | `PDFViewDelegate.pdfViewWillClick(onLink:with:)` [V] to intercept. Web links → popover/sheet with `WKWebView` (ephemeral data store, no cookies, mirroring Folio's isolation). Internal links → popover rendering the destination page region | Both | Medium / M |
| 15 | Cmd-click opens new tab | Missing. Single document, no tabs (`src/App.tsx:75-82,106`) | New subsystem (tabs) | macOS: native window tabbing (`NSWindow` tabbing / SwiftUI `WindowGroup`) or in-app tab strip. iOS: no Cmd-click; long-press menu "Open in new tab". Web links: in-app browser tab vs default browser (clarify) | Both (Cmd: macOS) | Medium / M |
| 16 | Highlight and ask AI | Has it (largely): `askAbout` from selection popover, highlight menu, notes card (`src/App.tsx:654-658,1205,1233,1115`); page citations (`src/AiMarkdown.tsx:25-29`) | Extend (port UX, rework provider + context) | Provider abstraction; retrieval over chunks; page images for scans/figures. Auth risk: see `02` section 5 | Both | Medium / M. Provider auth is the main risk |

## Performance and UX

| # | Item | Folio today | What's needed | Candidate approach / libraries (license) | Platform | Risk / effort |
| --- | --- | --- | --- | --- | --- | --- |
| 17 | Smooth on very long documents | Partial: pdf.js renders lazily but builds a DOM node per page; capped at 10,000 pages (`src/pdfInput.ts:1-11`) | Different architecture | Native: `PDFView` (tiled by the system) or custom `CATiledLayer`/Metal tile renderer over PDFKit or PDFium. Needs a benchmark (see `02` spike) | Both | Medium / M. PDFView behavior on 2,000+ pages unverified |
| 18 | Fluid zoom | Partial: pinch does CSS scale then re-render after 300 ms (`src/App.tsx:263-275`) | Different architecture | Native scroll view zoom with tiled re-rasterization (`PDFView` or `CATiledLayer`) | Both | Medium / M |
| 19 | Easy navigation | Has it (desktop): thumbnails, outline, page field, Home/End, search (`src/Sidebar.tsx`, `src/SearchBar.tsx`) | Extend (port UX) | SwiftUI sidebar + `PDFThumbnailView`, outline from `PDFOutline`; iOS scrubber | Both | Low / S-M |
| 20 | Lightweight, no background deps | Partial: no background services, but Electron bundles Chromium (macOS DMG 225.7 MB measured); Word/LibreOffice paths call external apps | Different architecture | Native app, system frameworks, in-process libraries only | Both | Decided in `02` |
| 21 | Quick, fluid animations | Partial (CSS transitions) | Different architecture (for native feel) | SwiftUI springs, matched geometry, interruptible transitions | Both | Low / S-M |
| 22 | Liquid Glass | Missing; not available natively in Electron. Third-party `electron-liquid-glass` uses private `NSGlassEffectView` APIs and warns against production use [V] | Different architecture | SwiftUI `.glassEffect` / `GlassEffectContainer`, iOS 26 / macOS 26 [V]. Implies deployment target 26 | Both | Low / S (native) |

## Platforms, library and cloud

| # | Item | Folio today | What's needed | Candidate approach / libraries (license) | Platform | Risk / effort |
| --- | --- | --- | --- | --- | --- | --- |
| 23 | iPhone and macOS app | macOS only (Electron); no iOS | Different architecture | SwiftUI multiplatform app; shared Swift package for logic | Both | Decided in `02` |
| 24 | Shared library | Missing. Recent list: 8 entries, full PDF copy in IndexedDB, skips files over 50 MiB (`src/recent.ts:19-21`, `src/App.tsx:345,356`) | New subsystem | Library DB (SQLite via GRDB, MIT [U]; or SwiftData) referencing files in place (bookmarks) + iCloud container for synced copies; FTS5 index incl. OCR text | Both | Medium / L |
| 25 | Cloud drives auto-connected (Google Drive, iCloud, OneDrive) | Missing | New subsystem | iCloud: ubiquity container (native). Google Drive and OneDrive: either (a) via their File Provider apps through the system document picker (no API work, but no "automatic" listing) or (b) direct APIs: Drive API with `drive.file` (non-sensitive) vs full `drive` (restricted, needs verification + annual CASA assessment for server access [V]); Microsoft Graph + MSAL, delta queries [V] | Both | High / L. Google restricted-scope review is the risk |
| 26 | Pull from drives directly | Missing | New subsystem | Same as 25: list + download on demand; Graph `delta`; Drive `changes` API [U] | Both | Medium / M (after 25) |
| 27 | iOS scanner saving to drives | Missing (Folio has camera capture for signatures only) | New subsystem | VisionKit `VNDocumentCameraViewController` (iOS 13+ [V]) → enhance (13) → OCR (7) → PDF → destination (iCloud folder, Drive/OneDrive upload, or vault) | iOS | Low-medium / M |
| 28 | Import documents | Partial: open/drag/drop/URL/Word on desktop | Extend | Document picker, share extension, Files "Open in", drag and drop, URL import (port Folio's SSRF guard) | Both | Low / S-M |
| 29 | Vault | Missing | New subsystem | Depends on meaning (see clarifying questions). If encrypted: per-file AES-GCM with key in Keychain (Data Protection class Complete), Face ID/Touch ID gate via LocalAuthentication; vault items excluded from search index or indexed in an encrypted store | Both | Medium / M |

## Summary

- Has it: 16 (largely), 19 (desktop).
- Partial: 2, 3, 10, 11, 17, 18, 20, 21, 28.
- Missing: 1, 4, 5, 6, 7, 8, 9, 12, 13, 14, 15, 22, 23, 24, 25, 26, 27, 29.

Corrections to the starting hints:
- Item 2 is partial, not missing: Folio renders LaTeX math in chat.
- Item 11 is partial: Night reading is a view-only color change.
- Items 17 to 21 are partial, not absent; the gap is the architecture (Electron/pdf.js), not missing features.
- Item 12 confirmed: "Save a copy" writes live annotations, nothing is flattened.
- Item 10 confirmed: rotation is view-only, persisted in `folio:content-view:<id>` localStorage, never in the file.
