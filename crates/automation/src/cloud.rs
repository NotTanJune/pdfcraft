//! Tools for Google Drive and OneDrive: `cloud_status`, `cloud_connect`, `cloud_disconnect`,
//! `cloud_list`, `cloud_download` and `cloud_upload`.
//!
//! Nothing here runs until called, and nothing reaches the network until an account is connected
//! (`cloud_status` and a refusal for a provider with no client ID do not). The owner's client IDs
//! come from the keychain (saved in the app's Cloud drives panel) or from
//! `PDFCRAFT_GOOGLE_CLIENT_ID`, `PDFCRAFT_GOOGLE_CLIENT_SECRET`, `PDFCRAFT_MICROSOFT_CLIENT_ID` and
//! `PDFCRAFT_MICROSOFT_AUTHORITY` (scopes: `PDFCRAFT_GOOGLE_SCOPES`, `PDFCRAFT_MICROSOFT_SCOPES`). Tokens live in the keychain and are never returned or logged.
//!
//! `cloud_download` opens the file into the session from memory (a PDF as it is; Word, Markdown,
//! LaTeX, CSV and Excel files through the in-process conversion; a Google Docs or Sheets file as its
//! PDF export), so with a root set nothing is written outside it: `path` also saves a copy inside the
//! root. A downloaded PDF remembers where it came from and which version, so `cloud_upload` with its
//! `id` refuses to replace a file that changed on the drive since, unless `overwrite: true`.

use std::sync::Arc;
use std::time::Duration;

use pdfcraft_clouddrive::{CancelToken, CloudError, Connection, Drive, Item, ProviderKind, ProviderSettings, Query, load_client, safe_file_name};
use pdfcraft_engine::DocId;
use serde_json::{Value, json};

use crate::{Args, Automation, Result, ToolError, failed, paths, summary};

/// Opens an address in a browser, or says why it could not.
pub type BrowserOpener = Arc<dyn Fn(&str) -> std::result::Result<(), String> + Send + Sync>;

/// How a host overrides what the tools would read from the keychain and the system: tests, and
/// programs that bring their own client IDs.
#[derive(Clone, Default)]
pub struct CloudSetup {
    pub google: Option<ProviderSettings>,
    pub microsoft: Option<ProviderSettings>,
    /// What opens the sign-in address. `None` opens the system browser.
    pub open_browser: Option<BrowserOpener>,
}

/// Where a document opened with `cloud_download` came from.
#[derive(Clone)]
struct RemoteLink {
    provider: ProviderKind,
    id: String,
    version: Option<String>,
}

/// The cloud state of one session.
#[derive(Default)]
pub(crate) struct CloudState {
    setup: CloudSetup,
    links: Vec<(DocId, RemoteLink)>,
}

fn bad(message: impl Into<String>) -> ToolError {
    ToolError::InvalidArgs(message.into())
}

/// A failure for the agent: sign-in problems say what to do.
fn cloud_failed(e: CloudError) -> ToolError {
    match &e {
        CloudError::NotConnected(_) | CloudError::Unauthenticated(_) => {
            failed(format!("{e}. Connect with the cloud_connect tool, or in PdfCraft's Cloud drives window"))
        }
        _ => failed(e),
    }
}

fn item_json(item: &Item) -> Value {
    json!({
        "id": item.id, "name": item.name, "folder": item.is_folder, "size": item.size, "modified": item.modified,
        "mime": item.mime, "version": item.version, "can_write": item.can_write, "pdf_export": item.exported_as_pdf, "url": item.web_url,
    })
}

impl Automation {
    /// Use these client settings and this browser opener instead of the keychain, the environment
    /// and the system browser.
    pub fn with_cloud(mut self, setup: CloudSetup) -> Self {
        self.cloud.setup = setup;
        self
    }

    fn cloud_kind(a: &Args) -> Result<ProviderKind> {
        let name = a.str("provider")?;
        ProviderKind::parse(name).ok_or_else(|| bad(format!("provider must be google or onedrive, not {name:?}")))
    }

    fn cloud_settings(&self, kind: ProviderKind) -> Result<ProviderSettings> {
        let preset = match kind {
            ProviderKind::Google => self.cloud.setup.google.clone(),
            ProviderKind::Microsoft => self.cloud.setup.microsoft.clone(),
        };
        match preset {
            Some(settings) => Ok(settings),
            None => load_client(kind).map_err(cloud_failed),
        }
    }

    fn cloud_connection(&self, kind: ProviderKind) -> Result<Connection> {
        let settings = self.cloud_settings(kind)?;
        if !settings.is_configured() {
            let hint = match kind {
                ProviderKind::Google => {
                    "enter its client ID in PdfCraft's Cloud drives window, or set PDFCRAFT_GOOGLE_CLIENT_ID (and PDFCRAFT_GOOGLE_CLIENT_SECRET)"
                }
                ProviderKind::Microsoft => "enter its client ID in PdfCraft's Cloud drives window, or set PDFCRAFT_MICROSOFT_CLIENT_ID",
            };
            return Err(failed(format!("{} is not set up: {hint}", kind.label())));
        }
        Connection::new(kind, &settings).map_err(cloud_failed)
    }

    fn cloud_drive(&self, kind: ProviderKind) -> Result<Box<dyn Drive>> {
        self.cloud_connection(kind)?.drive().map_err(cloud_failed)
    }

    /// `cloud_status`: see the module docs.
    pub(crate) fn cloud_status(&mut self, a: &Args) -> Result<Value> {
        let only = a
            .opt_str("provider")?
            .map(|n| ProviderKind::parse(n).ok_or_else(|| bad(format!("provider must be google or onedrive, not {n:?}"))))
            .transpose()?;
        let mut providers = Vec::new();
        for kind in ProviderKind::ALL.into_iter().filter(|k| only.is_none_or(|o| o == *k)) {
            let settings = self.cloud_settings(kind)?;
            let status = Connection::status(kind, &settings).map_err(cloud_failed)?;
            providers.push(json!({
                "provider": kind.id(),
                "label": kind.label(),
                "configured": status.configured,
                "connected": status.account.is_some(),
                "account": status.account.as_ref().map(|acc| json!({ "email": acc.email, "name": acc.name })),
            }));
        }
        Ok(json!({ "providers": providers }))
    }

    /// `cloud_connect`: see the module docs. Blocks until the person finishes in the browser.
    pub(crate) fn cloud_connect(&mut self, a: &Args) -> Result<Value> {
        let kind = Self::cloud_kind(a)?;
        let seconds = a.opt_int("timeout_secs")?.unwrap_or(300);
        if !(5..=900).contains(&seconds) {
            return Err(bad("timeout_secs must be between 5 and 900"));
        }
        let connection = self.cloud_connection(kind)?;
        let opener = self.cloud.setup.open_browser.clone();
        let open = move |address: &str| match &opener {
            Some(open) => open(address),
            None => pdfcraft_clouddrive::open_in_browser(address),
        };
        let account = connection.connect(&open, &CancelToken::new(), Duration::from_secs(seconds as u64)).map_err(cloud_failed)?;
        Ok(json!({ "provider": kind.id(), "connected": true, "account": { "email": account.email, "name": account.name } }))
    }

    /// `cloud_disconnect`.
    pub(crate) fn cloud_disconnect(&mut self, a: &Args) -> Result<Value> {
        let kind = Self::cloud_kind(a)?;
        let out = self.cloud_connection(kind)?.disconnect().map_err(cloud_failed)?;
        self.cloud.links.retain(|(_, l)| l.provider != kind);
        Ok(json!({
            "provider": kind.id(), "was_connected": out.was_connected, "revoked_at_provider": out.revoked,
            "note": if out.can_revoke { "the saved tokens were deleted" } else { "the saved tokens were deleted; Microsoft has no revoke call for desktop apps, so PdfCraft stays listed at account.microsoft.com until you remove it there" },
        }))
    }

    /// `cloud_list`.
    pub(crate) fn cloud_list(&mut self, a: &Args) -> Result<Value> {
        let kind = Self::cloud_kind(a)?;
        let drive = self.cloud_drive(kind)?;
        let limit = a.opt_int("limit")?.map(|l| usize::try_from(l).ok().filter(|l| (1..=pdfcraft_clouddrive::MAX_PAGE_SIZE).contains(l)));
        let limit = match limit {
            Some(Some(l)) => l,
            Some(None) => return Err(bad(format!("limit must be between 1 and {}", pdfcraft_clouddrive::MAX_PAGE_SIZE))),
            None => 0,
        };
        let query = Query {
            folder: a.opt_str("folder")?.map(str::to_string),
            search: a.opt_str("search")?.map(str::to_string),
            page: a.opt_str("page")?.map(str::to_string),
            limit,
        };
        if a.opt_bool("all")?.unwrap_or(false) {
            let (items, truncated) = pdfcraft_clouddrive::list_all(drive.as_ref(), &query).map_err(cloud_failed)?;
            return Ok(json!({ "provider": kind.id(), "items": items.iter().map(item_json).collect::<Vec<_>>(), "truncated": truncated }));
        }
        let page = drive.list(&query).map_err(cloud_failed)?;
        Ok(json!({ "provider": kind.id(), "items": page.items.iter().map(item_json).collect::<Vec<_>>(), "next": page.next }))
    }

    /// `cloud_download`.
    pub(crate) fn cloud_download(&mut self, a: &Args) -> Result<Value> {
        let kind = Self::cloud_kind(a)?;
        let id = a.str("id")?.to_string();
        let max = match a.opt_int("max_bytes")? {
            Some(m) => u64::try_from(m)
                .ok()
                .filter(|m| *m > 0 && *m <= pdfcraft_clouddrive::MAX_DOWNLOAD_BYTES)
                .ok_or_else(|| bad("max_bytes must be between 1 and 268435456"))?,
            None => pdfcraft_clouddrive::MAX_DOWNLOAD_BYTES,
        };
        // Resolve the copy's path first: a path outside the root is refused before any download.
        let copy = a.opt_str("path")?.map(|p| self.resolve(p, true)).transpose()?;
        if let Some(existing) = copy.as_ref().filter(|p| p.exists()) {
            return Err(failed(format!("{} already exists: choose another path, nothing was downloaded", existing.display())));
        }
        let drive = self.cloud_drive(kind)?;
        let got = drive.download(&id, max, &CancelToken::new(), &mut |_| {}).map_err(cloud_failed)?;
        let name = safe_file_name(&got.file_name);
        let bytes = Arc::new(got.bytes);
        if let Some(copy) = &copy {
            paths::write_atomic(copy, &bytes)?;
        }
        let password = a.opt_str("password")?;
        let lower = name.to_ascii_lowercase();
        let (doc, converted, warnings) = if lower.ends_with(".pdf") {
            let path = copy.as_ref().map(|p| p.to_string_lossy().into_owned());
            (self.session.open(name.clone(), path, bytes, password).map_err(failed)?, false, Vec::new())
        } else if pdfcraft_engine::DocumentFormat::of_name(&name).is_some() {
            let done = pdfcraft_engine::convert_document(&name, &bytes, &pdfcraft_engine::ConvertSettings::default()).map_err(failed)?;
            let stem = name.rsplit_once('.').map_or(name.as_str(), |(s, _)| s);
            (self.session.open_new(format!("{stem}.pdf"), done.pdf).map_err(failed)?, true, done.warnings)
        } else {
            return Err(failed(format!(
                "{name} was downloaded but PdfCraft cannot open that kind of file; it opens PDF, Word, Markdown, LaTeX, CSV and Excel files"
            )));
        };
        if !converted {
            self.cloud.links.retain(|(d, _)| *d != doc);
            self.cloud.links.push((doc, RemoteLink { provider: kind, id: got.item.id.clone(), version: got.item.version.clone() }));
        }
        let mut out = summary(self.session.get(doc).ok_or_else(|| failed("the document vanished"))?);
        out["remote"] =
            json!({ "provider": kind.id(), "id": got.item.id, "name": got.item.name, "version": got.item.version, "modified": got.item.modified });
        out["converted"] = json!(converted);
        out["warnings"] = json!(warnings);
        out["saved_copy"] = json!(copy.map(|p| p.to_string_lossy().into_owned()));
        Ok(out)
    }

    /// `cloud_upload`.
    pub(crate) fn cloud_upload(&mut self, a: &Args) -> Result<Value> {
        let kind = Self::cloud_kind(a)?;
        let (bytes, default_name, doc) = match (a.get("doc"), a.opt_str("path")?) {
            (Some(_), Some(_)) => return Err(bad("pass doc or path, not both")),
            (None, None) => return Err(bad("pass doc (an open document) or path (a file to upload)")),
            (Some(_), None) => {
                let d = self.doc(a)?;
                let id = d.id;
                let name = d.name.clone();
                // The document as it would be saved, unsaved edits included; the open document is unchanged.
                let bytes = self.session.save_bytes(id).map_err(failed)?;
                let name = if name.to_ascii_lowercase().ends_with(".pdf") { name } else { format!("{name}.pdf") };
                (bytes, name, Some(id))
            }
            (None, Some(p)) => {
                let path = self.resolve(p, false)?;
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
                (Arc::new(crate::read_input(&path)?), name, None)
            }
        };
        let drive = self.cloud_drive(kind)?;
        let cancel = CancelToken::new();
        let item = match a.opt_str("id")? {
            Some(id) => {
                if a.opt_str("folder")?.is_some() || a.opt_str("name")?.is_some() {
                    return Err(bad("id replaces an existing file's content; folder and name are for a new file"));
                }
                let overwrite = a.opt_bool("overwrite")?.unwrap_or(false);
                let linked = doc
                    .and_then(|d| self.cloud.links.iter().find(|(x, l)| *x == d && l.provider == kind && l.id == id).map(|(_, l)| l.version.clone()));
                let expected = a.opt_str("expected_version")?.map(str::to_string).or(linked.flatten());
                let check = if overwrite {
                    None
                } else {
                    Some(expected.ok_or_else(|| {
                        failed("cannot tell whether the file changed on the drive: download it with cloud_download first, pass expected_version, or pass overwrite: true to replace it anyway")
                    })?)
                };
                drive.update(id, &bytes, check.as_deref(), &cancel, &mut |_| {}).map_err(|e| match e {
                    CloudError::Conflict { name, .. } => failed(format!("{name} changed on the drive since you opened it; nothing was uploaded. Pass overwrite: true to replace it anyway, or upload a copy without id")),
                    other => cloud_failed(other),
                })?
            }
            None => {
                let name = a.opt_str("name")?.map(str::to_string).unwrap_or(default_name);
                let folder = a.opt_str("folder")?;
                drive.upload_new(folder, &name, &bytes, &cancel, &mut |_| {}).map_err(cloud_failed)?
            }
        };
        let created = a.opt_str("id")?.is_none();
        if let Some(d) = doc
            && !created
        {
            self.cloud.links.retain(|(x, _)| *x != d);
            self.cloud.links.push((d, RemoteLink { provider: kind, id: item.id.clone(), version: item.version.clone() }));
        }
        Ok(
            json!({ "provider": kind.id(), "created": created, "id": item.id, "name": item.name, "version": item.version, "bytes": bytes.len(), "url": item.web_url }),
        )
    }
}
