# pdfcraft-convert

Layer L4. Documents turned into PDF in process: no helper program, nothing to install.

- `convert(name, bytes, &Settings) -> Converted { pdf, pages, warnings }` reads a Word (`.docx`,
  `.docm`, `.dotx`, `.dotm`), Markdown, LaTeX, CSV/TSV or Excel (`.xlsx`, `.xlsm`, `.xltx`,
  `.xltm`, `.xlsb`) file by its extension. `Format::of_name` and `EXTENSIONS` say which names.
- The format crates of storytold/craft-libs read each file into the shared `craft_flow::Flow`
  (`craft-docx`, `craft-markdown`, `craft-latex`, `craft-csv`, `craft-xlsx`); `typeset(&Flow,
  &Options)` lays it out. `craft-wordmodel`, `craft-flow`, `craft-docx`, `craft-markdown`,
  `craft-latex` and `craft-rtf` exchange types, so the workspace pins them at one revision.
- The typesetter (`typeset/`) draws headings (as bookmarks), paragraphs with bold, italic,
  underline, strike, colour, size and sub/superscript, left, centre, right and justified lines,
  bullet and numbered lists, block quotes, code blocks, rules, tables (column widths from the
  document or from the content, borders, shading, column spans, header rows repeated on each
  page, a row taller than a page split between its lines), pictures, web and mail links (never
  `javascript:` or `file:` ones), footnotes (collected after the text) and equations as linear
  text. Text is selectable: the standard fonts (Helvetica, Times, Courier in four styles) in
  WinAnsiEncoding, and the bundled Inter face for characters those cannot draw. A character no
  font draws is set as `?` and reported in `warnings`.
- A CSV is one table whose first record is the header. A workbook is, for each visible sheet, a
  heading (a bookmark) and a table of the values its cells show: numbers under their number
  format (fixed decimals, thousands separators, percent, currency symbols, dates and times), text,
  booleans and errors; hidden rows and columns are left out and merged cells span. A table wider
  than the portrait page turns the whole document on its side (`Options::fit_wide`).
- Word pages take the document's own page size and margins (`Paper::Auto`); everything else is US
  Letter. `Paper::A4`, `Paper::Letter` and `Settings::margin` override either.

Not drawn, and said in `warnings` where it matters: shapes, charts, text boxes and group drawings,
headers and footers, comments, tracked-change marks, picture formats `pdfcraft_create` cannot embed
(shown as their description), LaTeX macros (not expanded). Line breaking is greedy, so lines break
where Word's would not always. A cell that spans rows is drawn in its first row only.

## Bounds

Input is hostile. The readers bound their own work; this crate adds: at most 16 million words
(`MAX_PIECES`), 10,000 pages, 400,000 table cells, 200 table columns, 4,000 characters of text in
a cell, 64 MiB for one picture and 512 MiB for all of them, 2,000 footnotes and 2,000 bookmarks,
tables nested 8 deep. Pagination never loops: a line taller than a page is drawn past the margin
rather than retried. An escaped panic comes back as `ConvertError::Read`.

Where it is used: `pdfcraft_engine::office_to_pdf` tries this crate for the formats above and falls
back to an installed LibreOffice (`pdfcraft_create::office_to_pdf`) for every other Office format,
and for a Word or Excel file this crate could not read. `doc_create_from_document` in
`pdfcraft-automation` calls it directly, never LibreOffice.
