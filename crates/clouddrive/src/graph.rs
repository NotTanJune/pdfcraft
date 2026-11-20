//! OneDrive through Microsoft Graph (`/me/drive`).
//!
//! - List: `root/children` and `items/{id}/children` with `@odata.nextLink` paging (a link is
//!   followed only when it is on the Graph host the token is for), `root/search(q='...')`.
//! - Download: `items/{id}/content` answers with a redirect to a pre-authorized address, which is
//!   fetched without the token.
//! - Upload: `PUT .../content` up to [`SIMPLE_UPLOAD_BYTES`] (Graph's simple limit is 4 MB), an
//!   upload session in 320 KiB-multiple chunks above.
//! - Scopes: `Files.ReadWrite offline_access User.Read` unless Settings names others.

use serde_json::{Value, json};

use crate::api::Api;
use crate::drive::{DEFAULT_PAGE_SIZE, Drive, MAX_PAGE_SIZE, SIMPLE_UPLOAD_BYTES, check_new_name, check_upload_size, local_name, mime_for};
use crate::error::CloudError;
use crate::http::Spec;
use crate::settings::Endpoints;
use crate::types::{Account, CancelToken, Download, Item, ListPage, Progress, ProviderKind, Query, is_openable};
use crate::url;

const SELECT: &str = "id,name,size,folder,file,eTag,lastModifiedDateTime,parentReference,webUrl,package,remoteItem";
/// 320 KiB x 12: an upload session chunk must be a multiple of 320 KiB.
const CHUNK: usize = 12 * 320 * 1024;
/// A redirect chain from `/content` is a hop or two; this stops a loop.
const MAX_HOPS: usize = 3;

pub(crate) struct OneDrive {
    api: Api,
    base: String,
}

impl OneDrive {
    pub fn new(api: Api, endpoints: Endpoints) -> OneDrive {
        OneDrive { api, base: endpoints.api.trim_end_matches('/').to_string() }
    }
}

fn text(v: &Value, name: &str) -> Option<String> {
    v.get(name).and_then(Value::as_str).map(str::to_string)
}

/// A `driveItem` as an [`Item`]. Shared-in items (`remoteItem`) and OneNote packages are not
/// listed.
fn item(v: &Value) -> Option<Item> {
    let id = text(v, "id")?;
    Some(Item {
        id,
        name: text(v, "name").unwrap_or_default(),
        is_folder: v.get("folder").is_some(),
        size: v.get("size").and_then(Value::as_u64),
        modified: text(v, "lastModifiedDateTime"),
        mime: v.pointer("/file/mimeType").and_then(Value::as_str).map(str::to_string),
        version: text(v, "eTag"),
        parent: v.pointer("/parentReference/id").and_then(Value::as_str).map(str::to_string),
        // Graph does not say whether a shared file is writable without another call; a refusal
        // surfaces as Forbidden when saving.
        can_write: true,
        exported_as_pdf: false,
        web_url: text(v, "webUrl"),
    })
}

fn listed(v: &Value, item: &Item) -> bool {
    if v.get("remoteItem").is_some() || v.get("package").is_some() {
        return false;
    }
    item.is_folder || is_openable(&item.name)
}

/// Folders first, then by name, as a page reads best.
fn sort(items: &mut [Item]) {
    items.sort_by(|a, b| b.is_folder.cmp(&a.is_folder).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
}

/// `'text'` as an OData string literal inside a path: quotes doubled, then percent-encoded.
fn odata_literal(text: &str) -> String {
    format!("%27{}%27", url::encode(&text.replace('\'', "''")))
}

impl OneDrive {
    fn children_address(&self, query: &Query, size: usize) -> Result<String, CloudError> {
        let tail = format!("$top={size}&$select={SELECT}");
        if let Some(term) = query.search.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            return Ok(format!("{}/me/drive/root/search(q={})?{tail}", self.base, odata_literal(term)));
        }
        Ok(match query.folder.as_deref().filter(|f| !f.is_empty()) {
            Some(id) => format!("{}/me/drive/items/{}/children?{tail}", self.base, url::segment(id)?),
            None => format!("{}/me/drive/root/children?{tail}", self.base),
        })
    }

    /// A next-page link is followed only on the host the token was issued for.
    fn next_link(&self, link: &str) -> Result<String, CloudError> {
        let own = url::check_endpoint(&self.base)?;
        let next = url::check_endpoint(link).map_err(|_| CloudError::BadResponse("the next page is not on the service's address".into()))?;
        if !url::same_origin(&own, &next) {
            return Err(CloudError::BadResponse("the next page is not on the service's address".into()));
        }
        Ok(link.to_string())
    }

    fn put_chunks(&self, session: &str, bytes: &[u8], cancel: &CancelToken, progress: &mut dyn FnMut(Progress)) -> Result<Value, CloudError> {
        let total = bytes.len();
        let mut start = 0usize;
        while start < total {
            cancel.check()?;
            let end = (start + CHUNK).min(total);
            let part = bytes.get(start..end).ok_or_else(|| CloudError::Invalid("upload range out of bounds".into()))?;
            let range = format!("bytes {start}-{}/{total}", end - 1);
            // The upload address is pre-authorized: Graph says not to send the token to it.
            let reply = self
                .api
                .plain("the upload", &Spec::new("PUT", session).bytes(part, "application/octet-stream").header("Content-Range", range).transfer())?;
            match reply.status {
                200 | 201 => return reply.json(),
                202 => {
                    start = end;
                    progress(Progress { done: start as u64, total: Some(total as u64) });
                }
                other => return Err(CloudError::Http { status: other, message: "unexpected answer while uploading".into() }),
            }
        }
        Err(CloudError::BadResponse("the upload session ended without a file".into()))
    }

    /// Uploads through a session: `create` is the `createUploadSession` address.
    fn session_upload(
        &self,
        create: &str,
        conflict: &str,
        bytes: &[u8],
        cancel: &CancelToken,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<Value, CloudError> {
        let body = serde_json::to_vec(&json!({ "item": { "@microsoft.graph.conflictBehavior": conflict } }))
            .map_err(|e| CloudError::Invalid(e.to_string()))?;
        let made = self.api.json("the upload", &Spec::new("POST", create).json(&body))?;
        let session = text(&made, "uploadUrl").ok_or_else(|| CloudError::BadResponse("OneDrive gave no upload address".into()))?;
        url::check_endpoint(&session)?;
        self.put_chunks(&session, bytes, cancel, progress)
    }
}

impl Drive for OneDrive {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Microsoft
    }

    fn account(&self) -> Result<Account, CloudError> {
        let address = format!("{}/me?$select=displayName,mail,userPrincipalName", self.base);
        let me = self.api.json("your account", &Spec::new("GET", &address))?;
        Ok(Account {
            provider: ProviderKind::Microsoft,
            email: text(&me, "mail").or_else(|| text(&me, "userPrincipalName")),
            name: text(&me, "displayName"),
        })
    }

    fn list(&self, query: &Query) -> Result<ListPage, CloudError> {
        let size = if query.limit == 0 { DEFAULT_PAGE_SIZE } else { query.limit.min(MAX_PAGE_SIZE) };
        let address = match query.page.as_deref().filter(|p| !p.is_empty()) {
            Some(link) => self.next_link(link)?,
            None => self.children_address(query, size)?,
        };
        let page = self.api.json("the folder", &Spec::new("GET", &address))?;
        let mut items: Vec<Item> = page
            .get("value")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|v| item(v).filter(|i| listed(v, i))).collect())
            .unwrap_or_default();
        sort(&mut items);
        Ok(ListPage { items, next: text(&page, "@odata.nextLink").filter(|l| !l.is_empty()) })
    }

    fn stat(&self, id: &str) -> Result<Item, CloudError> {
        let address = format!("{}/me/drive/items/{}?$select={SELECT}", self.base, url::segment(id)?);
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
        let address = format!("{}/me/drive/items/{}/content", self.base, url::segment(id)?);
        cancel.check()?;
        let mut reply = self.api.call(&info.name, &Spec::new("GET", &address))?;
        // The content address is a redirect to a pre-authorized link on another host; it is
        // fetched without the token, and each further hop is checked the same way.
        let mut hops = 0;
        while reply.is_redirect() {
            hops += 1;
            let next = reply.header("location").map(str::to_string).filter(|_| hops <= MAX_HOPS);
            let next = next.ok_or_else(|| CloudError::BadResponse("OneDrive redirected the download too many times".into()))?;
            url::check_endpoint(&next)?;
            reply = self.api.plain(&info.name, &Spec::new("GET", &next).transfer())?;
        }
        let bytes = reply.stream(max_bytes, &info.name, cancel, progress)?;
        let file_name = local_name(&info.name, info.mime.as_deref());
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
        cancel.check()?;
        let total = bytes.len() as u64;
        progress(Progress { done: 0, total: Some(total) });
        let parent = match folder.filter(|f| !f.is_empty()) {
            Some(id) => format!("{}/me/drive/items/{}", self.base, url::segment(id)?),
            None => format!("{}/me/drive/root", self.base),
        };
        let file = url::segment(name)?;
        let value = if bytes.len() <= SIMPLE_UPLOAD_BYTES {
            let address = format!("{parent}:/{file}:/content?@microsoft.graph.conflictBehavior=rename");
            self.api.json("the upload", &Spec::new("PUT", &address).bytes(bytes, mime_for(name)).transfer())?
        } else {
            self.session_upload(&format!("{parent}:/{file}:/createUploadSession"), "rename", bytes, cancel, progress)?
        };
        progress(Progress { done: total, total: Some(total) });
        item(&value).ok_or_else(|| CloudError::BadResponse("the upload answer has no file".into()))
    }

    fn replace(&self, id: &str, bytes: &[u8], cancel: &CancelToken, progress: &mut dyn FnMut(Progress)) -> Result<Item, CloudError> {
        check_upload_size(bytes)?;
        cancel.check()?;
        let seg = url::segment(id)?;
        let total = bytes.len() as u64;
        progress(Progress { done: 0, total: Some(total) });
        let value = if bytes.len() <= SIMPLE_UPLOAD_BYTES {
            let address = format!("{}/me/drive/items/{seg}/content", self.base);
            self.api.json("the upload", &Spec::new("PUT", &address).bytes(bytes, "application/octet-stream").transfer())?
        } else {
            self.session_upload(&format!("{}/me/drive/items/{seg}/createUploadSession", self.base), "replace", bytes, cancel, progress)?
        };
        progress(Progress { done: total, total: Some(total) });
        item(&value).ok_or_else(|| CloudError::BadResponse("the upload answer has no file".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_items_become_items_and_shared_in_ones_are_not_listed() {
        let file = json!({ "id": "F1", "name": "Plan.PDF", "size": 10, "eTag": "\"{A},2\"", "file": { "mimeType": "application/pdf" },
                           "lastModifiedDateTime": "2026-01-01T00:00:00Z", "parentReference": { "id": "P1" } });
        let i = item(&file).unwrap();
        assert_eq!((i.version.as_deref(), i.parent.as_deref(), i.size, i.is_folder), (Some("\"{A},2\""), Some("P1"), Some(10), false));
        assert!(listed(&file, &i));
        let folder = json!({ "id": "D", "name": "Docs", "folder": { "childCount": 2 } });
        assert!(item(&folder).is_some_and(|i| i.is_folder && listed(&folder, &i)));
        let exe = json!({ "id": "E", "name": "setup.exe", "file": {} });
        assert!(!item(&exe).is_some_and(|i| listed(&exe, &i)));
        let remote = json!({ "id": "R", "name": "shared.pdf", "remoteItem": { "id": "x" }, "file": {} });
        assert!(!item(&remote).is_some_and(|i| listed(&remote, &i)));
    }

    #[test]
    fn a_search_term_is_an_odata_literal_with_quotes_doubled() {
        assert_eq!(odata_literal("it's a/b"), "%27it%27%27s%20a%2Fb%27");
    }

    #[test]
    fn folders_sort_first_then_by_name() {
        let mut items: Vec<Item> =
            ["b.pdf", "A.pdf", "z"].iter().map(|n| Item { name: (*n).into(), is_folder: *n == "z", ..Item::default() }).collect();
        sort(&mut items);
        assert_eq!(items.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["z", "A.pdf", "b.pdf"]);
    }
}
