# Decisions log

Answers to the Phase 2 clarifying questions and the platform choice. Every Phase 3 doc follows these.

| # | Topic | Decision (2026-11-01) |
| --- | --- | --- |
| P | Platform | (b) Native SwiftUI + PDFKit, macOS 26 and iOS 26. In-process libraries only where Apple lacks something |
| U | Mac updates | Sparkle + GitHub Releases |
| A | Apple account | Paid Apple Developer Program account |
| D | Distribution | Personal use. Open-source repo containing the Mac and iOS apps; iOS installed by a build command |
| 1 | LaTeX | Compile .tex to PDF, and export PDF to LaTeX. Compiling must work on iOS |
| 2 | Change color | All meanings: view-time invert/grayscale/sepia, recolor written into the file, annotation colors |
| 3 | Vault | Encrypted, unlocked with Face ID / Touch ID or passcode (Mac: Touch ID or login password). Vault files appear in search only while unlocked |
| 4 | Excel / CSV | Both directions (to PDF and from PDF) |
| 5 | Shared library | Across your own devices only |
| 6 | Enhance | Low-resolution pages first. Show original vs enhanced side by side; you choose to keep the original or replace it |
| 7 | Links | Preview both web links and internal links. Cmd-click opens in the default browser |
| 8 | Dependencies | No bundled apps, no helper processes, no servers; RAM as low as possible. PDF to Word with layout fidelity is dropped; export to formats Google Docs imports (DOCX with simple structure, TXT, and similar) |
| 9 | Cloud drives | Direct API integration with Google Drive and OneDrive using your own developer projects; no server |
| 10 | AI | No ChatGPT or Claude account sign-in. Bring-your-own API key, any provider (OpenAI first; Gemini and Claude later). Page images may be sent. Repo is open source |
| 11 | Formats | DOCX, XLSX, CSV, images now. PPTX, HTML, Markdown listed as later candidates |
