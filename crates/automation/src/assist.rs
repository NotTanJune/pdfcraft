//! Tools that reach the network, only when called: `ai_ask` (a question about selected text,
//! answered by the person's own OpenAI-compatible endpoint) and `link_preview` (what hovering a
//! link shows: the destination page region, or a web address's host and, with `fetch`, its title
//! and description).
//!
//! `ai_ask` sends the selected text, its page number and, with `context`, the text of that page.
//! `dry_run` returns that request without sending it. A key stored in the keychain or named by
//! `PDFCRAFT_AI_API_KEY` goes only to the endpoint the environment configures (OpenAI's by
//! default): a call that names another `base_url` must bring its own `api_key`, so a tool
//! argument cannot send the stored key somewhere else.

use pdfcraft_assist::chat::{AiConfig, CancelToken, Question};
use pdfcraft_assist::{cite, preview, secret};
use pdfcraft_render::LinkTarget;
use serde_json::{Value, json};

use crate::dispatch::Content;
use crate::{Args, Automation, Result, ToolError, failed};

/// The page text sent with `context: true`, at most; `pdfcraft_assist::chat` cuts it again.
const CONTEXT_PAGES: usize = 1;

/// Pixel width of a `link_preview` picture.
const PREVIEW_PX: u32 = 480;

fn bad(message: impl Into<String>) -> ToolError {
    ToolError::InvalidArgs(message.into())
}

impl Automation {
    /// `ai_ask`: see the module docs.
    pub(crate) fn ai_ask(&mut self, a: &Args) -> Result<Value> {
        let page = self.page(a)?;
        let selected = a.str("text")?.to_string();
        if selected.trim().is_empty() {
            return Err(bad("text is empty: pass the selected text to ask about"));
        }
        // Sending text to a model is copying it out of the document.
        let doc = self.doc_to_copy(a)?;
        let (id, page_count) = (doc.id, doc.info.pages.len());
        let context = if a.opt_bool("context")?.unwrap_or(false) {
            let texts = self.page_texts(id, &[page])?;
            texts.into_iter().take(CONTEXT_PAGES).next().map(|t| t.plain_text())
        } else {
            None
        };
        let question =
            Question { question: a.opt_str("question")?.unwrap_or_default().to_string(), selected, page: page + 1, context, history: Vec::new() };
        let mut config = AiConfig::from_env();
        let explicit_key = a.opt_str("api_key")?;
        if let Some(base) = a.opt_str("base_url")?.map(str::trim).filter(|b| !b.is_empty()) {
            if explicit_key.is_none() && base != config.base_url {
                return Err(bad(
                    "base_url names a different endpoint than the configured one, so it needs its own api_key: the stored key is only sent to the configured endpoint",
                ));
            }
            config.base_url = base.to_string();
        }
        if let Some(model) = a.opt_str("model")?.map(str::trim).filter(|m| !m.is_empty()) {
            config.model = model.to_string();
        }
        let endpoint = config.endpoint().map_err(|e| bad(e.to_string()))?;
        if a.opt_bool("dry_run")?.unwrap_or(false) {
            return Ok(json!({ "sent": false, "endpoint": endpoint, "request": question.request_body(&config) }));
        }
        let key = secret::resolve(explicit_key)
            .map_err(|e| bad(e.to_string()))?
            .ok_or_else(|| failed(format!("AI is off: no API key is set. Save one in PdfCraft's Ask AI panel, or set {}.", secret::KEY_ENV)))?;
        let answer = pdfcraft_assist::ask(&config, &key, &question, &CancelToken::new()).map_err(|e| failed(e.to_string()))?;
        let citations: Vec<Value> = cite::find(&answer.text, Some(page_count))
            .into_iter()
            .map(|c| json!({ "text": answer.text.get(c.range).unwrap_or_default(), "pages": c.pages }))
            .collect();
        Ok(json!({ "sent": true, "answer": answer.text, "model": answer.model, "page": page + 1, "endpoint": endpoint, "citations": citations }))
    }

    /// `link_preview`: see the module docs.
    pub(crate) fn link_preview(&mut self, a: &Args) -> Result<Vec<Content>> {
        let fetch = a.opt_bool("fetch")?.unwrap_or(false);
        let target = match (a.opt_str("url")?, a.opt_int("index")?) {
            (Some(_), Some(_)) => return Err(bad("pass url, or page and index of a link, not both")),
            (Some(url), None) => LinkTarget::Uri(url.to_string()),
            (None, Some(index)) => {
                let page = self.page(a)?;
                let doc = self.doc(a)?;
                let index = usize::try_from(index).ok().and_then(|i| i.checked_sub(1)).ok_or_else(|| bad("index starts at 1"))?;
                let link = doc
                    .info
                    .links
                    .iter()
                    .find(|l| l.page == page && l.index == index)
                    .ok_or_else(|| failed(format!("page {} has no followable link {} (see link_list)", page + 1, index + 1)))?;
                link.target.clone()
            }
            (None, None) => return Err(bad("pass url, or doc, page and index of a link (see link_list)")),
        };
        match target {
            LinkTarget::Page(dest, view) => {
                let doc = self.doc(a)?;
                let page_info = doc.info.pages.get(dest).ok_or_else(|| failed("the link goes to a page the document does not have"))?;
                let rect = pdfcraft_engine::link_preview::region(page_info, view);
                let out = pdfcraft_engine::link_preview::render_region(doc.bytes.clone(), doc.password.as_deref(), dest, rect, PREVIEW_PX, false);
                if let Some(e) = out.error {
                    return Err(failed(format!("page {} could not be drawn: {e}", dest + 1)));
                }
                let info = json!({ "kind": "page", "page": dest + 1, "label": page_info.label, "region": rect });
                let png = crate::pages::encode_png(out.width, out.height, &out.rgba)?;
                Ok(vec![Content::Json(info), Content::Png { data: png, width: out.width, height: out.height }])
            }
            LinkTarget::Uri(url) => {
                let Some(web) = pdfcraft_engine::link_preview::web_target(&url) else {
                    return Ok(vec![Content::Json(json!({ "kind": "other", "url": url, "note": "only http and https addresses have a preview" }))]);
                };
                let mut info = json!({
                    "kind": "web",
                    "url": web.url,
                    "host": web.host.ascii,
                    "international_host": web.host.international,
                    "mixed_scripts": web.host.mixed_scripts,
                    "fetched": false,
                });
                if fetch {
                    info["fetched"] = json!(true);
                    match preview::fetch(&web.url, &self.link_fetch) {
                        Ok(card) => {
                            info["final_url"] = json!(card.final_url);
                            info["title"] = json!(card.meta.title);
                            info["description"] = json!(card.meta.description);
                            info["site_name"] = json!(card.meta.site_name);
                        }
                        Err(e) => info["fetch_error"] = json!(e.to_string()),
                    }
                }
                Ok(vec![Content::Json(info)])
            }
            other => Ok(vec![Content::Json(json!({ "kind": "other", "note": format!("{other:?}").chars().take(80).collect::<String>() }))]),
        }
    }
}
