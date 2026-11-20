# Owner feature plan (fork of storytold/pdfcraft)

Source of requirements: `docs/owner/01-gap-map.md` and `docs/owner/decisions.md` (the former pdocf
wishlist). This fork replaces the pdocf Swift app, which is archived outside this repository.

## Already in PdfCraft (no work)

Compress (Reduce File Size), OCR (Latin script), merge (Combine), split, page rotation, flatten,
document tabs, images/text to PDF, high-contrast page colours, DOCX/HTML/RTF/Markdown export
(partial), Office open through LibreOffice (partial).

## Out of reach in PdfCraft

iOS/iPadOS app, the iPhone document scanner and Liquid Glass: PdfCraft is a Rust/egui desktop and
web app with no iOS target.

## Constraints from AGENTS.md

- Rust only, no webview, no handwritten JS. Web link previews are metadata cards, not live pages.
- Never crash: no `unwrap`/`expect`/`panic`, no `unsafe`, hostile-input bounds, new crates copy the
  `#![deny(...)]` block and `[lints] workspace = true`.
- Layering: nothing below L7 depends on egui/winit/eframe/rfd (`cargo xtask layers`).
- Licence allowlist (MIT, Apache-2.0, BSD, ISC, Unicode-3.0, ...). Check `deny.toml`.
- Every user-facing feature has an engine API, a tool in `crates/automation` with an end-to-end
  test, and an honest `parity/acrobat-features.toml` entry.
- Network features are opt-in and off by default.
- The wasm build must keep compiling; desktop-only features sit behind `cfg(not(target_arch = "wasm32"))`.

## Feature groups

### A. Reading + AI
1. Link preview: hovering an internal link shows a popover with the destination page region
   rendered; hovering a web link shows the URL, and (opt-in setting) a fetched title/description
   card. Bounded fetch (size, time, redirects), no cookies, no scripts.
2. Cmd-click (Ctrl-click elsewhere) on a web link opens it in the default browser without a prompt.
3. Highlight and ask AI: selection context menu "Ask AI"; a chat panel with the selected text and
   page number as context; answers cite pages. Bring-your-own-key, OpenAI-compatible endpoint
   (OpenAI first; base URL configurable so Ollama/LM Studio/Gemini/Claude-compatible proxies work).
   Key stored in the OS keychain. Off until a key is set. Automation tool `ai_ask`.

### B. Conversions (in-process, no LibreOffice)
1. A flow typesetter: craft-flow document (paragraphs, headings, lists, tables, pictures, linear
   math) laid out to PDF pages with the bundled fonts.
2. Import to PDF: DOCX (craft-docx), Markdown (craft-markdown), LaTeX (craft-latex, everyday
   subset, no macro expansion), CSV (craft-csv), XLSX (craft-xlsx).
3. Export from PDF: XLSX and CSV (table detection from the export crate's inferred tables),
   LaTeX (flow from the export crate, written with craft-latex).
4. Wire into File ▸ Open / Create PDF and Export As; LibreOffice becomes an optional fallback.

### C. Vault + library
1. Library: an index of documents (path, title, page count, tags, last opened, extracted text for
   search) stored as a JSON file in a sync folder (default: iCloud Drive `PdfCraft/`), so the
   library is shared across the owner's Macs.
2. Vault: AES-256-GCM encrypted files in the sync folder; the key is in the macOS keychain behind
   Touch ID or the login password. Vault documents open decrypted in memory only and appear in
   library search only while the vault is unlocked.

### D. Cloud drives
1. Google Drive and OneDrive backends on the craft-cloud contract: OAuth 2.0 with PKCE and a
   loopback redirect, the owner's own client IDs (entered in Settings), tokens in the keychain.
2. Browse, open (download on demand), save back, upload a copy. Off until connected.

## Order

1. Baseline release build installed as the default PDF app.
2. A and B in parallel (A owns `ui-egui`; B owns new crates + `create`/`export` + automation tools,
   with UI hooks added after A lands).
3. C, then D.
4. Release build of the fork, installed as the default PDF app.
