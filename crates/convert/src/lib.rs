//! pdfcraft-convert: documents turned into PDF in process (L4).
//!
//! Word (.docx), Markdown, LaTeX, CSV and Excel (.xlsx) files are read by the craft-libs format
//! crates into the shared flow model (`craft_flow`), then typeset onto pages by [`typeset`]: no
//! helper process, no installed office suite. Formats not handled here (.doc, .pptx, .odt, ...)
//! stay with `pdfcraft_create::office_to_pdf`, which runs LibreOffice when it is installed.
//!
//! Layout is not Word's: lines break greedily, fonts are the standard ones (with the bundled Inter
//! face for characters those lack), and drawings, charts, text boxes, headers and footers are not
//! drawn. What was left out comes back in [`Converted::warnings`].

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod import;
mod numfmt;
mod sheet;
pub mod typeset;

pub use typeset::{Options, PageSetup, Typeset, typeset};

use craft_flow::Flow;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ConvertError {
    /// The file could not be read as the format its name says.
    #[error("{0}")]
    Read(String),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    TooLarge(String),
    #[error("{0}")]
    Write(String),
}

/// A file format converted in process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// Word: .docx, .docm, .dotx, .dotm.
    Docx,
    Markdown,
    /// The everyday subset of LaTeX; macros are not expanded.
    Latex,
    /// Delimited text: .csv and .tsv.
    Csv,
    /// Excel: .xlsx, .xlsm, .xltx, .xltm and .xlsb.
    Xlsx,
}

/// The file extensions converted in process, lower case.
pub const EXTENSIONS: &[&str] =
    &["docx", "docm", "dotx", "dotm", "md", "markdown", "tex", "latex", "csv", "tsv", "xlsx", "xlsm", "xltx", "xltm", "xlsb"];

impl Format {
    /// The format a file name's extension names, if converted in process.
    pub fn of_name(name: &str) -> Option<Format> {
        let (_, ext) = name.rsplit_once('.')?;
        match ext.to_ascii_lowercase().as_str() {
            "docx" | "docm" | "dotx" | "dotm" => Some(Format::Docx),
            "md" | "markdown" => Some(Format::Markdown),
            "tex" | "latex" => Some(Format::Latex),
            "csv" | "tsv" => Some(Format::Csv),
            "xlsx" | "xlsm" | "xltx" | "xltm" | "xlsb" => Some(Format::Xlsx),
            _ => None,
        }
    }

    /// A short name for messages.
    pub fn label(self) -> &'static str {
        match self {
            Format::Docx => "Word",
            Format::Markdown => "Markdown",
            Format::Latex => "LaTeX",
            Format::Csv => "CSV",
            Format::Xlsx => "Excel",
        }
    }
}

/// The paper a converted document is set on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Paper {
    /// A Word document's own page; US Letter for the rest.
    #[default]
    Auto,
    A4,
    Letter,
}

/// Settings for [`convert`].
#[derive(Clone, Debug)]
pub struct Settings {
    pub paper: Paper,
    /// The body text size in points.
    pub font_size: f64,
    /// Every margin, in points; the paper's own when `None`.
    pub margin: Option<f64>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { paper: Paper::Auto, font_size: 11.0, margin: None }
    }
}

/// A converted document.
#[derive(Debug)]
pub struct Converted {
    pub pdf: Vec<u8>,
    pub pages: usize,
    /// What the conversion left out or could not draw, each said once.
    pub warnings: Vec<String>,
}

/// Converts the file `bytes` named `name` to PDF, by its extension. Panics inside the format
/// readers or the layout come back as errors.
pub fn convert(name: &str, bytes: &[u8], settings: &Settings) -> Result<Converted, ConvertError> {
    let format = Format::of_name(name).ok_or_else(|| ConvertError::Invalid(format!("{name}: this file type is not converted in process")))?;
    let title = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    let title = title.rsplit(['/', '\\']).next().unwrap_or(title).to_string();
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(format, bytes, settings, &title)))
        .unwrap_or_else(|_| Err(ConvertError::Read(format!("{name}: the file could not be read"))))
}

fn run(format: Format, bytes: &[u8], settings: &Settings, title: &str) -> Result<Converted, ConvertError> {
    let mut own_page = None;
    let mut warnings = Vec::new();
    let (flow, fit_wide, numbered): (Flow, bool, bool) = match format {
        Format::Docx => {
            let (flow, page) = import::docx(bytes)?;
            own_page = page;
            (flow, false, false)
        }
        Format::Markdown => (import::markdown(bytes), false, false),
        Format::Latex => (import::latex(bytes), false, false),
        Format::Csv => {
            let (flow, w) = sheet::csv(bytes)?;
            warnings = w;
            (flow, true, true)
        }
        Format::Xlsx => {
            let (flow, w) = sheet::xlsx(bytes)?;
            warnings = w;
            (flow, true, true)
        }
    };
    let mut page = match settings.paper {
        Paper::A4 => PageSetup::A4,
        Paper::Letter => PageSetup::LETTER,
        Paper::Auto => own_page.unwrap_or(PageSetup::LETTER),
    };
    if let Some(m) = settings.margin.filter(|m| m.is_finite() && *m >= 0.0) {
        page = page.with_margin(m);
    } else if fit_wide {
        // Tables are set with narrower margins than letters.
        page = page.with_margin(48.0);
    }
    let options = Options { page, font_size: settings.font_size, page_numbers: numbered, title: title.to_string(), fit_wide };
    let mut typeset = typeset(&flow, &options)?;
    warnings.append(&mut typeset.warnings);
    let pdf = pdfcraft_cos::write_full(&typeset.document, &pdfcraft_cos::SaveOptions::default()).map_err(|e| ConvertError::Write(e.to_string()))?;
    Ok(Converted { pdf, pages: typeset.pages, warnings })
}

#[cfg(test)]
mod tests;
