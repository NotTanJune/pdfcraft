# Product

<!-- impeccable:product-schema 1 -->

## Platform

ios

Apple-native app on Mac, iPad and iPhone (SwiftUI, macOS 26 / iPadOS 26 / iOS 26), following each platform's conventions.

## Users

One person, the owner: a solo AI engineer comfortable with Rust, Swift and TypeScript. Uses the app in four situations, all confirmed as primary:

- Reading long research papers, specs and books on the Mac: highlighting, taking notes, asking AI about passages.
- Handling paperwork: scanning bills, contracts and IDs on iPhone, recognizing text, compressing to upload, filing into a drive or the vault.
- Converting and editing: merging, splitting, converting DOCX, XLSX, CSV and LaTeX, then sending the result somewhere.
- Casual reading: ebooks, manuals and articles read for long stretches, often on the phone.

## Product Purpose

One personal PDF app that replaces a reader, a scanner, a converter and a cloud file browser, while staying very light and fast. Success: it is the app the owner opens for every PDF task on every device, with no background services and no waiting.

## Positioning

Everything runs on the device in one process: OCR, compression, conversion and search work offline; documents leave the device only when the owner asks the AI (with their own API key) or syncs to a drive they connected. Bring-your-own-key AI for any provider.

## Operating Context

- Mac at a desk for long reading and annotation sessions; iPhone in hand for scanning and quick reading; iPad for reading and annotation.
- Files arrive from Finder/Files, drag and drop, links, the iPhone scanner, iCloud, Google Drive and OneDrive.
- Updates on Mac arrive through Sparkle from the open-source GitHub repo; iOS and iPad builds are installed from the repo.

## Capabilities and Constraints

- Full scope: `docs/decisions.md` and `docs/01-gap-map.md` (29 wishlist items). Built so far: library, reader, highlights and notes, link preview, page operations, OCR with text layer, full-text search, compression.
- No helper processes, bundled apps or servers; RAM as low as possible.
- Liquid Glass and native animations are required; deployment target 26.
- Open decision: the product name ("pdocf" is a working name).

## Brand Commitments

- Name is open; "pdocf" is a placeholder.
- No binding visual identity. Folio (MIT) was the starting reference; its look may be replaced entirely.

## Evidence on Hand

- No real user content, testimonials or metrics; personal tool. Test fixtures are generated PDFs and Folio's `sample.pdf` (MIT).

## Product Principles

1. Fast and light before feature-rich: nothing runs in the background.
2. Private by default: content leaves the device only on an explicit action.
3. One tool for every PDF job, on every Apple device.
4. Respect each platform's conventions; personality lives in details, not in fighting the OS.
