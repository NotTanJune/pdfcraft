//! Other files to combine with or copy pages from: opening them, and why one can't be used.

use std::sync::Arc;

use crate::{EditError, SourceProblem};

/// A file to combine: its name (the bookmark title), bytes, and page range (`None`: all).
pub type CombineSource = (String, Arc<Vec<u8>>, Option<String>);

/// What a combine produced: the new PDF bytes, and the fields renamed to keep colliding names
/// separate (old fully-qualified name → new one), in order across the sources.
#[derive(Debug)]
pub struct Combined {
    pub bytes: Arc<Vec<u8>>,
    pub renamed_fields: Vec<(String, String)>,
}

/// Convert the document `bytes` named `name` to PDF and check the result opens.
///
/// Word (.docx), Markdown, LaTeX, CSV and Excel (.xlsx) files are typeset in process
/// (`pdfcraft-convert`): no helper process and nothing to install. Every other Office format
/// (.doc, .pptx, .odt, ...), and a Word or Excel file the in-process reader could not read, goes
/// to an installed LibreOffice, which runs as a separate process for seconds, so callers keep it
/// off the frame thread. LibreOffice is desktop only: on the web such a file returns an error.
pub fn office_to_pdf(name: &str, bytes: &Arc<Vec<u8>>) -> Result<Arc<Vec<u8>>, EditError> {
    let pdf = match convert_in_process(name, bytes)? {
        Some(done) => Arc::new(done.pdf),
        None => Arc::new(pdfcraft_create::office_to_pdf(name, bytes)?),
    };
    open_source(name, &pdf)?;
    Ok(pdf)
}

/// A document typeset in process: its PDF, page count and what the conversion left out.
#[derive(Debug)]
pub struct DocumentPdf {
    pub pdf: Arc<Vec<u8>>,
    pub pages: usize,
    pub warnings: Vec<String>,
}

/// Convert a Word, Markdown, LaTeX, CSV or Excel file to PDF in process, with `settings` (paper,
/// margin, text size). Never runs LibreOffice: a file that cannot be read says why.
pub fn convert_document(name: &str, bytes: &Arc<Vec<u8>>, settings: &pdfcraft_convert::Settings) -> Result<DocumentPdf, EditError> {
    let done = pdfcraft_convert::convert(name, bytes, settings).map_err(|e| pdfcraft_create::CreateError::Office(format!("{name}: {e}")))?;
    let pdf = Arc::new(done.pdf);
    open_source(name, &pdf)?;
    Ok(DocumentPdf { pdf, pages: done.pages, warnings: done.warnings })
}

/// The in-process conversion of `name`, or `None` when LibreOffice is the way: the format is not
/// one `pdfcraft-convert` handles, or it could not read this Word or Excel file and LibreOffice is
/// installed to try.
fn convert_in_process(name: &str, bytes: &Arc<Vec<u8>>) -> Result<Option<pdfcraft_convert::Converted>, EditError> {
    if pdfcraft_convert::Format::of_name(name).is_none() {
        return Ok(None);
    }
    match pdfcraft_convert::convert(name, bytes, &pdfcraft_convert::Settings::default()) {
        Ok(done) => Ok(Some(done)),
        Err(e) => {
            let text_format =
                name.rsplit_once('.').is_some_and(|(_, ext)| pdfcraft_create::DOCUMENT_EXTENSIONS.iter().any(|d| ext.eq_ignore_ascii_case(d)));
            #[cfg(not(target_arch = "wasm32"))]
            if !text_format && pdfcraft_create::libreoffice().is_some() {
                return Ok(None);
            }
            let _ = text_format;
            Err(pdfcraft_create::CreateError::Office(format!("{name}: {e}")).into())
        }
    }
}

/// Parse another PDF to copy pages from.
pub(crate) fn open_source(name: &str, bytes: &Arc<Vec<u8>>) -> Result<pdfcraft_cos::Document, EditError> {
    open_source_with(name, bytes, None)
}

/// [`open_source`], authenticating with `password` (user or owner) for an encrypted file.
pub(crate) fn open_source_with(name: &str, bytes: &Arc<Vec<u8>>, password: Option<&str>) -> Result<pdfcraft_cos::Document, EditError> {
    source_document(bytes, password).map_err(|p| EditError::Source(format!("{name}: {p}")))
}

/// [`open_source`] for pages inserted into `dest`. A source that withholds page extraction but
/// allows assembly is accepted when `dest` will be saved under the very same protection (file
/// key and `/Encrypt` values), as when pages cut or copied from a document are pasted back into
/// it: they never leave its protection (#5081).
pub(crate) fn open_source_into(name: &str, bytes: &Arc<Vec<u8>>, dest: &pdfcraft_cos::Document) -> Result<pdfcraft_cos::Document, EditError> {
    let same_protection = |src: &pdfcraft_cos::Document| match (src.security(), dest.output_handler()) {
        (Some(s), Some(d)) => s.file_key() == d.file_key() && s.dict() == d.dict(),
        _ => false,
    };
    let permitted = |src: &pdfcraft_cos::Document| src.permissions().is_none_or(|p| p.extract_pages() || (p.assemble() && same_protection(src)));
    source_document_if(bytes, None, permitted).map_err(|p| EditError::Source(format!("{name}: {p}")))
}

/// Import the comments of `src` onto the pages of `doc` with the same numbers.
pub(crate) fn import_pdf_comments(doc: &mut pdfcraft_cos::Document, src: &pdfcraft_cos::Document) -> Result<(), EditError> {
    if pdfcraft_organize::import_comments(doc, src)? == 0 {
        return Err(pdfcraft_xfdf::DataError::NothingImported.into());
    }
    Ok(())
}

/// Parse another PDF to import comments from, whatever its permissions allow.
pub(crate) fn comment_source(bytes: &Arc<Vec<u8>>) -> Result<pdfcraft_cos::Document, SourceProblem> {
    match std::panic::catch_unwind(|| pdfcraft_cos::Document::open(bytes.clone())) {
        Ok(Ok(d)) => Ok(d),
        Ok(Err(pdfcraft_cos::CosError::NeedsPassword)) => Err(SourceProblem::Password),
        Ok(Err(e)) => Err(SourceProblem::Unreadable(e.to_string())),
        Err(_) => Err(SourceProblem::Unreadable("the file could not be read".into())),
    }
}

fn source_document(bytes: &Arc<Vec<u8>>, password: Option<&str>) -> Result<pdfcraft_cos::Document, SourceProblem> {
    source_document_if(bytes, password, |d| d.permissions().is_none_or(|p| p.extract_pages()))
}

/// Open `bytes` as a page source, refused as [`SourceProblem::NotPermitted`] unless `permitted`.
fn source_document_if(
    bytes: &Arc<Vec<u8>>,
    password: Option<&str>,
    permitted: impl Fn(&pdfcraft_cos::Document) -> bool,
) -> Result<pdfcraft_cos::Document, SourceProblem> {
    match std::panic::catch_unwind(|| pdfcraft_cos::Document::open_with_password(bytes.clone(), password)) {
        Ok(Ok(d)) if !permitted(&d) => Err(SourceProblem::NotPermitted),
        Ok(Ok(d)) => Ok(d),
        Ok(Err(pdfcraft_cos::CosError::NeedsPassword)) => Err(SourceProblem::Password),
        Ok(Err(pdfcraft_cos::CosError::WrongPassword)) => Err(SourceProblem::WrongPassword),
        Ok(Err(e)) => Err(SourceProblem::Unreadable(e.to_string())),
        Err(_) => Err(SourceProblem::Unreadable("the file could not be read".into())),
    }
}

/// Whether `bytes` can be combined, opened with `password` (user or owner) if given, and if not
/// why: what Combine would refuse it for, so the Combine files list can say so before Combine
/// is pressed. The owner (permissions) password lifts the restriction on copying pages.
pub fn combine_source_check(bytes: &Arc<Vec<u8>>, password: Option<&str>) -> Result<(), SourceProblem> {
    source_document(bytes, password).map(|_| ())
}

/// The number of pages of `bytes`, opened with `password` (user or owner) if given, whatever
/// its permissions allow; `None` when it can't be read.
pub fn source_page_count(bytes: &Arc<Vec<u8>>, password: Option<&str>) -> Option<usize> {
    let doc = std::panic::catch_unwind(|| pdfcraft_cos::Document::open_with_password(bytes.clone(), password)).ok()?.ok()?;
    pdfcraft_organize::page_count(&doc).ok()
}
