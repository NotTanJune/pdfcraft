//! Google Drive v3.
//!
//! - List: `files.list` with a `q` that keeps folders, PDFs, the formats PdfCraft converts and
//!   Google Docs and Sheets; text files are filtered again by extension. Paging by `pageToken`.
//! - Download: `files.get?alt=media`; a Google Docs or Sheets file has no bytes and goes through
//!   `files.export` as a PDF (Google limits an export to 10 MB).
//! - Upload: a multipart request up to [`SIMPLE_UPLOAD_BYTES`], a resumable session in chunks above.
//! - Scope: `https://www.googleapis.com/auth/drive` unless Settings names another.

use serde_json::{Value, json};

use crate::api::Api;
use crate::drive::{DEFAULT_PAGE_SIZE, Drive, MAX_PAGE_SIZE, SIMPLE_UPLOAD_BYTES, check_new_name, check_upload_size, local_name, mime_for};
use crate::error::CloudError;
use crate::http::Spec;
use crate::settings::Endpoints;
use crate::types::{Account, CancelToken, Download, Item, ListPage, Progress, ProviderKind, Query, is_openable};
use crate::url;

const FOLDER: &str = "application/vnd.google-apps.folder";
const NATIVE_PDF: &[&str] = &[
    "application/vnd.google-apps.document",
    "application/vnd.google-apps.spreadsheet",
    "application/vnd.google-apps.presentation",
    "application/vnd.google-apps.drawing",
];
/// Media types the listing asks for. Text-like ones are filtered again by extension.
const LISTED_MIMES: &[&str] = &[
    "application/pdf",
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    "text/csv",
    "text/tab-separated-values",
    "text/markdown",
    "text/plain",
    "text/x-markdown",
    "text/x-tex",
    "application/x-tex",
    "application/octet-stream",
    "application/vnd.google-apps.document",
    "application/vnd.google-apps.spreadsheet",
];
/// Media types whose files are kept only when the name has an extension PdfCraft opens.
const BY_EXTENSION: &[&str] = &[
    "text/plain",
    "text/x-markdown",
    "text/x-tex",
    "application/x-tex",
    "application/octet-stream",
    "text/markdown",
    "text/csv",
    "text/tab-separated-values",
];
const FIELDS: &str = "id,name,mimeType,size,modifiedTime,version,parents,capabilities(canEdit),webViewLink";
/// 256 KiB x 12: a resumable chunk must be a multiple of 256 KiB.
const CHUNK: usize = 12 * 256 * 1024;

pub(crate) struct GoogleDrive {
    api: Api,
    api_base: String,
    upload_base: String,
}

impl GoogleDrive {
    pub fn new(api: Api, endpoints: Endpoints) -> GoogleDrive {
        GoogleDrive {
            api,
            api_base: endpoints.api.trim_end_matches('/').to_string(),
            upload_base: endpoints.upload.trim_end_matches('/').to_string(),
        }
    }
}

/// A `q` string literal: `\` and `'` escaped.
fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// The `q` of a listing.
fn query_string(query: &Query) -> String {
    let mimes = LISTED_MIMES.iter().map(|m| format!("mimeType = {}", literal(m))).collect::<Vec<_>>().join(" or ");
    let kinds = format!("(mimeType = {} or {mimes})", literal(FOLDER));
    let scope = match query.search.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(term) => format!("name contains {}", literal(term)),
        None => format!("{} in parents", literal(query.folder.as_deref().unwrap_or("root"))),
    };
    format!("trashed = false and {scope} and {kinds}")
}

fn text(v: &Value, name: &str) -> Option<String> {
    v.get(name).and_then(Value::as_str).map(str::to_string)
}

/// A `files` resource as an [`Item`]; `None` when it has no id.
fn item(v: &Value) -> Option<Item> {
    let id = text(v, "id")?;
    let mime = text(v, "mimeType");
    let modified = text(v, "modifiedTime");
    Some(Item {
        id,
        name: text(v, "name").unwrap_or_default(),
        is_folder: mime.as_deref() == Some(FOLDER),
        size: v.get("size").and_then(|s| s.as_str().and_then(|s| s.parse().ok()).or_else(|| s.as_u64())),
        version: text(v, "version").or_else(|| modified.clone()),
        modified,
        exported_as_pdf: mime.as_deref().is_some_and(|m| NATIVE_PDF.contains(&m)),
        parent: v.pointer("/parents/0").and_then(Value::as_str).map(str::to_string),
        can_write: v.pointer("/capabilities/canEdit").and_then(Value::as_bool).unwrap_or(true),
        web_url: text(v, "webViewLink"),
        mime,
    })
}

/// Whether the browser lists `item`: folders, native documents and files with an openable name.
fn listed(item: &Item) -> bool {
    if item.is_folder || item.exported_as_pdf {
        return true;
    }
    match item.mime.as_deref() {
        Some("application/pdf") => true,
        Some(m) if BY_EXTENSION.contains(&m) => is_openable(&item.name),
        _ => is_openable(&item.name),
    }
}

impl Drive for GoogleDrive {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Google
    }

    fn account(&self) -> Result<Account, CloudError> {
        let address = format!("{}/about?fields=user(emailAddress,displayName)", self.api_base);
        let about = self.api.json("your account", &Spec::new("GET", &address))?;
        let user = about.get("user").cloned().unwrap_or(Value::Null);
        Ok(Account { provider: ProviderKind::Google, email: text(&user, "emailAddress"), name: text(&user, "displayName") })
    }

    fn list(&self, query: &Query) -> Result<ListPage, CloudError> {
        let size = if query.limit == 0 { DEFAULT_PAGE_SIZE } else { query.limit.min(MAX_PAGE_SIZE) };
        let mut address = format!(
            "{}/files?{}",
            self.api_base,
            url::form(&[
                ("q", &query_string(query)),
                ("pageSize", &size.to_string()),
                ("orderBy", "folder,name"),
                ("fields", &format!("nextPageToken,files({FIELDS})")),
                ("spaces", "drive"),
                ("supportsAllDrives", "true"),
                ("includeItemsFromAllDrives", "true"),
            ])
        );
        if let Some(token) = query.page.as_deref().filter(|t| !t.is_empty()) {
            address.push_str(&format!("&pageToken={}", url::encode(token)));
        }
        let page = self.api.json("the folder", &Spec::new("GET", &address))?;
        let items = page.get("files").and_then(Value::as_array).map(|a| a.iter().filter_map(item).filter(listed).collect()).unwrap_or_default();
        Ok(ListPage { items, next: text(&page, "nextPageToken").filter(|t| !t.is_empty()) })
    }

    fn stat(&self, id: &str) -> Result<Item, CloudError> {
        let address = format!("{}/files/{}?supportsAllDrives=true&fields={}", self.api_base, url::segment(id)?, url::encode(FIELDS));
        let v = self.api.json("the file", &Spec::new("GET", &address))?;
        item(&v).ok_or_else(|| CloudError::BadResponse("the file has no id".into()))
    }

    fn download(&self, id: &str, max_bytes: u64, cancel: &CancelToken, progress: &mut dyn FnMut(Progress)) -> Result<Download, CloudError> {
        let info = self.stat(id)?;
        if info.is_folder {
            return Err(CloudError::Invalid(format!("{} is a folder", info.name)));
        }
        if info.size.is_some_and(|s| s > max_bytes) {
            return Err(CloudError::TooLarge { what: info.name.clone(), limit: max_bytes });
        }
        let seg = url::segment(id)?;
        let address = if info.exported_as_pdf {
            format!("{}/files/{seg}/export?mimeType=application%2Fpdf", self.api_base)
        } else {
            format!("{}/files/{seg}?alt=media&supportsAllDrives=true", self.api_base)
        };
        cancel.check()?;
        let reply = self.api.call(&info.name, &Spec::new("GET", &address).transfer())?;
        if reply.is_redirect() {
            return Err(CloudError::BadResponse("Google redirected the download, which PdfCraft does not follow with a token".into()));
        }
        let bytes = reply.stream(max_bytes, &info.name, cancel, progress)?;
        let mime = if info.exported_as_pdf { Some("application/pdf") } else { info.mime.as_deref() };
        let file_name = local_name(&info.name, mime);
        Ok(Download { bytes, item: info, file_name })
    }

    fn upload_new(
        &self,
        folder: Option<&str>,
        name: &str,
        bytes: &[u8],
        cancel: &CancelToken,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<Item, CloudError> {
        let name = check_new_name(name)?;
        check_upload_size(bytes)?;
        let mime = mime_for(name);
        let mut metadata = json!({ "name": name, "mimeType": mime });
        if let Some(folder) = folder.filter(|f| !f.is_empty()) {
            metadata["parents"] = json!([folder]);
        }
        self.send_content(&metadata, "POST", &format!("{}/files", self.upload_base), mime, bytes, cancel, progress)
    }

    fn replace(&self, id: &str, bytes: &[u8], cancel: &CancelToken, progress: &mut dyn FnMut(Progress)) -> Result<Item, CloudError> {
        check_upload_size(bytes)?;
        let info = self.stat(id)?;
        if info.is_folder {
            return Err(CloudError::Invalid(format!("{} is a folder", info.name)));
        }
        if info.exported_as_pdf {
            return Err(CloudError::Invalid(format!(
                "{} is a Google document, which cannot take a PDF as its content: save a copy instead",
                info.name
            )));
        }
        if !info.can_write {
            return Err(CloudError::Forbidden(format!("you can only view {}", info.name)));
        }
        let mime = info.mime.clone().filter(|m| m != "application/octet-stream").unwrap_or_else(|| mime_for(&info.name).to_string());
        self.send_content(&json!({}), "PATCH", &format!("{}/files/{}", self.upload_base, url::segment(id)?), &mime, bytes, cancel, progress)
    }
}

impl GoogleDrive {
    /// Uploads `bytes` with `metadata` to `base` (a `files` or `files/<id>` upload address): in one
    /// multipart (create) or media (update) request, or in a resumable session when large.
    #[allow(clippy::too_many_arguments)]
    fn send_content(
        &self,
        metadata: &Value,
        method: &'static str,
        base: &str,
        mime: &str,
        bytes: &[u8],
        cancel: &CancelToken,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<Item, CloudError> {
        cancel.check()?;
        let total = bytes.len() as u64;
        progress(Progress { done: 0, total: Some(total) });
        let tail = format!("supportsAllDrives=true&fields={}", url::encode(FIELDS));
        let value = if bytes.len() <= SIMPLE_UPLOAD_BYTES {
            if method == "POST" {
                let boundary = boundary()?;
                let body = multipart(&boundary, metadata, mime, bytes)?;
                let address = format!("{base}?uploadType=multipart&{tail}");
                let content_type = format!("multipart/related; boundary={boundary}");
                self.api.json("the upload", &Spec::new("POST", &address).bytes(&body, &content_type).transfer())?
            } else {
                let address = format!("{base}?uploadType=media&{tail}");
                self.api.json("the upload", &Spec::new("PATCH", &address).bytes(bytes, mime).transfer())?
            }
        } else {
            let address = format!("{base}?uploadType=resumable&{tail}");
            let head = serde_json::to_vec(metadata).map_err(|e| CloudError::Invalid(e.to_string()))?;
            let reply = self.api.call(
                "the upload",
                &Spec::new(method, &address).header("X-Upload-Content-Type", mime).header("X-Upload-Content-Length", total.to_string()).json(&head),
            )?;
            let session =
                reply.header("location").map(str::to_string).ok_or_else(|| CloudError::BadResponse("Google gave no upload address".into()))?;
            url::check_endpoint(&session)?;
            self.put_chunks(&session, bytes, cancel, progress)?
        };
        progress(Progress { done: total, total: Some(total) });
        item(&value).ok_or_else(|| CloudError::BadResponse("the upload answer has no file".into()))
    }

    /// Sends `bytes` to a resumable session, a chunk at a time. The session address is
    /// pre-authorized, so no token goes with it.
    fn put_chunks(&self, session: &str, bytes: &[u8], cancel: &CancelToken, progress: &mut dyn FnMut(Progress)) -> Result<Value, CloudError> {
        let total = bytes.len();
        let mut start = 0usize;
        loop {
            cancel.check()?;
            let end = (start + CHUNK).min(total);
            let part = bytes.get(start..end).ok_or_else(|| CloudError::Invalid("upload range out of bounds".into()))?;
            let range = format!("bytes {start}-{}/{total}", end - 1);
            let reply = self
                .api
                .plain("the upload", &Spec::new("PUT", session).bytes(part, "application/octet-stream").header("Content-Range", range).transfer())?;
            match reply.status {
                200 | 201 => return reply.json(),
                308 => {
                    // `Range: bytes=0-N`: what the server holds. Absent: nothing yet.
                    let held =
                        reply.header("range").and_then(|r| r.rsplit('-').next()).and_then(|n| n.trim().parse::<usize>().ok()).map_or(0, |n| n + 1);
                    if held <= start || held > total {
                        return Err(CloudError::BadResponse("Google's upload session did not advance".into()));
                    }
                    start = held;
                    progress(Progress { done: start as u64, total: Some(total as u64) });
                }
                other => return Err(CloudError::Http { status: other, message: "unexpected answer while uploading".into() }),
            }
            if start >= total {
                return Err(CloudError::BadResponse("Google's upload session ended without a file".into()));
            }
        }
    }
}

fn boundary() -> Result<String, CloudError> {
    let mut random = [0u8; 12];
    getrandom::fill(&mut random).map_err(|e| CloudError::Invalid(format!("no randomness: {e}")))?;
    Ok(format!("pdfcraft{}", random.iter().map(|b| format!("{b:02x}")).collect::<String>()))
}

/// A `multipart/related` body: the metadata as JSON, then the bytes.
fn multipart(boundary: &str, metadata: &Value, mime: &str, bytes: &[u8]) -> Result<Vec<u8>, CloudError> {
    let meta = serde_json::to_string(metadata).map_err(|e| CloudError::Invalid(e.to_string()))?;
    let mut body =
        format!("--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{meta}\r\n--{boundary}\r\nContent-Type: {mime}\r\n\r\n")
            .into_bytes();
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--").as_bytes());
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_listing_is_scoped_to_the_folder_and_the_openable_types() {
        let q = query_string(&Query { folder: Some("abc".into()), ..Query::default() });
        assert!(
            q.starts_with(
                "trashed = false and 'abc' in parents and (mimeType = 'application/vnd.google-apps.folder' or mimeType = 'application/pdf'"
            ),
            "{q}"
        );
        assert!(q.contains("vnd.google-apps.document") && q.contains("wordprocessingml.document") && q.contains("'text/csv'"));
        assert!(query_string(&Query::default()).contains("'root' in parents"));
    }

    #[test]
    fn a_search_term_cannot_break_out_of_its_string() {
        let q = query_string(&Query { search: Some("it's \\ ' or trashed = true".into()), ..Query::default() });
        assert!(q.contains("name contains 'it\\'s \\\\ \\' or trashed = true'"), "{q}");
        assert!(!q.contains("in parents"), "a search covers the whole drive");
    }

    #[test]
    fn files_resources_become_items_and_odd_ones_are_filtered() {
        let doc = item(&json!({ "id": "1", "name": "Notes", "mimeType": "application/vnd.google-apps.document", "modifiedTime": "2026-01-01T00:00:00Z", "version": "7" })).unwrap();
        assert!(doc.exported_as_pdf && listed(&doc));
        assert_eq!(doc.version.as_deref(), Some("7"));
        let pdf = item(&json!({ "id": "2", "name": "a.pdf", "mimeType": "application/pdf", "size": "1234", "capabilities": { "canEdit": false } }))
            .unwrap();
        assert_eq!((pdf.size, pdf.can_write, listed(&pdf)), (Some(1234), false, true));
        let text = item(&json!({ "id": "3", "name": "readme.txt", "mimeType": "text/plain" })).unwrap();
        assert!(!listed(&text));
        let md = item(&json!({ "id": "4", "name": "readme.md", "mimeType": "text/plain" })).unwrap();
        assert!(listed(&md));
        assert!(item(&json!({ "name": "no id" })).is_none());
        let folder = item(&json!({ "id": "5", "name": "d", "mimeType": FOLDER })).unwrap();
        assert!(folder.is_folder && listed(&folder));
    }

    #[test]
    fn the_multipart_body_has_metadata_then_the_bytes() {
        let body = multipart("B", &json!({ "name": "a.pdf" }), "application/pdf", b"%PDF-1.7").unwrap();
        let text = String::from_utf8(body).unwrap();
        assert!(text.starts_with(
            "--B\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{\"name\":\"a.pdf\"}\r\n--B\r\nContent-Type: application/pdf\r\n\r\n%PDF-1.7"
        ));
        assert!(text.ends_with("\r\n--B--"));
    }
}
