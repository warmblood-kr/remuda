//! Native HTML preview backed by a document-scoped, read-only Drive origin.

use gpui::{App, AppContext, Entity, Window};
use gpui_webview::WebView;
use percent_encoding::percent_decode_str;
use remuda_core::vfs::{validate_path, VfsUri};
use remuda_native::cluster::vfs::MAX_BLOB_BYTES;
use std::{
    borrow::Cow,
    sync::{
        atomic::{AtomicU64, AtomicU8, Ordering},
        mpsc, Arc,
    },
};
use url::Url;
#[cfg(windows)]
use wry::WebViewBuilderExtWindows;
use wry::{
    http::{Method, Response, StatusCode},
    NewWindowResponse, PageLoadEvent, RequestAsyncResponder, WebViewBuilder,
};

const SCHEME: &str = "remuda-preview";
const QUEUE_DEPTH: usize = 16;

pub(crate) fn supported() -> bool {
    // Keep the installed reader as the default until both native platforms
    // pass foreground and end-to-end document delivery checks.
    if !env_enabled("REMUDA_NATIVE_DOCUMENT_WEBVIEW") {
        return false;
    }
    // GPUI's Windows DirectComposition target can cover child HWNDs. Respect
    // its startup setting without changing the renderer for other windows.
    #[cfg(windows)]
    return env_enabled("GPUI_DISABLE_DIRECT_COMPOSITION");

    #[cfg(target_os = "macos")]
    true
}

fn env_enabled(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| value == "true" || value == "1")
}

pub(crate) type ReadAsset = Arc<dyn Fn(&str) -> Option<Vec<u8>> + Send + Sync>;

struct Origin {
    document: VfsUri,
    host: String,
}

impl Origin {
    fn new(uri: &str) -> Option<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Some(Self {
            document: VfsUri::parse(uri).ok()?,
            host: format!("document-{}.invalid", NEXT.fetch_add(1, Ordering::Relaxed)),
        })
    }

    fn url(&self) -> Url {
        let mut url = Url::parse(&format!("{SCHEME}://{}/", self.host)).unwrap();
        url.path_segments_mut()
            .unwrap()
            .pop_if_empty()
            .extend(self.document.path.split('/'));
        url
    }

    // Wry translates custom origins to synthetic HTTPS origins on Windows.
    // Its navigation event may still use that form, while resource requests
    // are mapped back to the custom scheme before reaching the handler.
    fn path(&self, url: &str) -> Option<String> {
        let url = Url::parse(url).ok()?;
        let host = url.host_str()?;
        let matches = (url.scheme() == SCHEME && host == self.host)
            || (cfg!(windows)
                && url.scheme() == "https"
                && host == format!("{SCHEME}.{}", self.host));
        if !matches
            || !url.username().is_empty()
            || url.password().is_some()
            || url.port().is_some()
        {
            return None;
        }
        let mut path = String::new();
        for segment in url.path_segments()? {
            let segment = percent_decode_str(segment).decode_utf8().ok()?;
            if segment.is_empty() || segment.contains('/') {
                return None;
            }
            if !path.is_empty() {
                path.push('/');
            }
            path.push_str(&segment);
        }
        validate_path(&path).ok()?;
        let directory = self.document.path.rsplit_once('/').map_or("", |x| x.0);
        if !directory.is_empty()
            && !path
                .strip_prefix(directory)
                .is_some_and(|suffix| suffix.starts_with('/'))
        {
            return None;
        }
        Some(path)
    }

    fn read(&self, url: &str, main: &[u8], asset: &ReadAsset) -> Option<(&'static str, Vec<u8>)> {
        let path = self.path(url)?;
        if path == self.document.path {
            return Some(("text/html; charset=utf-8", main.to_vec()));
        }

        let extension = path.rsplit_once('.')?.1.to_ascii_lowercase();
        let content_type = match extension.as_str() {
            "css" => "text/css; charset=utf-8",
            "js" | "mjs" => "text/javascript; charset=utf-8",
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "avif" => "image/avif",
            "woff" => "font/woff",
            "woff2" => "font/woff2",
            _ => return None,
        };
        let uri = VfsUri::format(self.document.cluster.as_deref().unwrap_or(""), &path);
        let bytes = asset(&uri)?;
        if bytes.len() as u64 > MAX_BLOB_BYTES {
            return None;
        }
        Some((content_type, bytes))
    }

    fn policy(&self) -> String {
        let custom = format!("{SCHEME}://{}", self.host);
        let origin = if cfg!(windows) {
            format!("https://{SCHEME}.{}", self.host)
        } else {
            custom
        };
        format!(
            "default-src 'none'; script-src {origin} 'unsafe-inline'; \
            style-src {origin} 'unsafe-inline'; img-src {origin}; font-src {origin}; \
            base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
        )
    }
}

type Reply = Response<Cow<'static, [u8]>>;

fn error(status: StatusCode) -> Reply {
    Response::builder()
        .status(status)
        .body(Cow::Borrowed(&[][..]))
        .unwrap()
}

pub(crate) struct Preview {
    view: Entity<WebView>,
    loaded: std::rc::Rc<std::cell::Cell<bool>>,
    observation: Arc<AtomicU8>,
}

impl Preview {
    pub(crate) fn open(
        window: &mut Window,
        cx: &mut App,
        uri: &str,
        html: Arc<[u8]>,
        asset: ReadAsset,
    ) -> Result<Self, String> {
        let origin = Arc::new(Origin::new(uri).ok_or("Invalid Drive document URI")?);
        let (send, receive) =
            mpsc::sync_channel::<(String, bool, RequestAsyncResponder)>(QUEUE_DEPTH);
        let worker_origin = origin.clone();
        std::thread::Builder::new()
            .name("remuda-webview-assets".into())
            .spawn(move || {
                let policy = worker_origin.policy();
                while let Ok((url, head, responder)) = receive.recv() {
                    let reply = match worker_origin.read(&url, &html, &asset) {
                        Some((mime, bytes)) => Response::builder()
                            .header("Content-Type", mime)
                            .header("Content-Security-Policy", &policy)
                            .header("X-Content-Type-Options", "nosniff")
                            .header("Cache-Control", "no-store")
                            .header("X-DNS-Prefetch-Control", "off")
                            .body(Cow::Owned(if head { Vec::new() } else { bytes }))
                            .unwrap(),
                        None => error(StatusCode::NOT_FOUND),
                    };
                    responder.respond(reply);
                }
            })
            .map_err(|err| err.to_string())?;
        let navigation = origin.clone();
        let original = origin.clone();
        let loaded = std::rc::Rc::new(std::cell::Cell::new(false));
        let page_load = loaded.clone();
        let builder = WebViewBuilder::new()
            .with_incognito(true)
            .with_focused(false)
            .with_navigation_handler(move |url| {
                navigation.path(&url).as_deref() == Some(navigation.document.path.as_str())
            })
            .with_on_page_load_handler(move |event, url| {
                page_load.set(
                    matches!(event, PageLoadEvent::Finished)
                        && original.path(&url).as_deref() == Some(original.document.path.as_str()),
                );
            })
            .with_new_window_req_handler(|_, _| NewWindowResponse::Deny)
            .with_download_started_handler(|_, _| false)
            .with_asynchronous_custom_protocol(SCHEME.to_owned(), move |_, request, response| {
                let head = request.method() == Method::HEAD;
                if !head && request.method() != Method::GET {
                    return response.respond(error(StatusCode::METHOD_NOT_ALLOWED));
                }
                if let Err(err) = send.try_send((request.uri().to_string(), head, response)) {
                    let (mpsc::TrySendError::Full((_, _, responder))
                    | mpsc::TrySendError::Disconnected((_, _, responder))) = err;
                    responder.respond(error(StatusCode::SERVICE_UNAVAILABLE));
                }
            })
            .with_url(origin.url().as_str());
        #[cfg(windows)]
        let builder = builder.with_https_scheme(true);
        let view = WebView::build(builder, window, cx).map_err(|err| err.to_string())?;
        let view = cx.new(|_| view);
        Ok(Self {
            view,
            loaded,
            observation: Arc::new(AtomicU8::new(0)),
        })
    }

    /// Called after the Document canvas was painted with a visible child.
    /// WebView2's completion event can also represent a navigation error, so
    /// require a live, visible DOM before reporting completion to the bus.
    pub(crate) fn observe_presentation(&self, cx: &App) -> bool {
        let view = self.view.read(cx);
        if !view.visible() || view.bounds().is_empty() || !self.loaded.get() {
            return false;
        }
        match self.observation.load(Ordering::Acquire) {
            2 => return true,
            1 => return false,
            _ => {}
        }
        let observation = self.observation.clone();
        observation.store(1, Ordering::Release);
        if view.raw().evaluate_script_with_callback(
            "document.readyState === 'complete' && document.visibilityState === 'visible' && document.body !== null",
            move |result| { observation.store(if result.trim() == "true" { 2 } else { 0 }, Ordering::Release); },
        ).is_err() {
            self.observation.store(0, Ordering::Release);
        }
        false
    }

    pub(crate) fn element(&self, cx: &App) -> Entity<WebView> {
        let view = self.view.read(cx);
        if !view.visible() {
            let _ = view.set_visible(true);
        }
        self.view.clone()
    }

    pub(crate) fn hide(&mut self, cx: &App) {
        let view = self.view.read(cx);
        if view.visible() {
            let _ = view.set_visible(false);
            // Discard any callback started before the view was hidden.
            self.observation = Arc::new(AtomicU8::new(0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_same_directory_drive_resources_are_passed_to_the_granted_reader() {
        let origin = Origin::new("remuda://cluster/reports/한 글/index.html").unwrap();
        let mut url = origin.url();
        let read: ReadAsset = Arc::new(|uri| {
            assert_eq!(uri, "remuda://cluster/reports/한 글/app.js");
            Some(b"console.log('ready')".to_vec())
        });
        assert_eq!(
            origin.read(url.as_str(), b"<html>", &read).unwrap().1,
            b"<html>"
        );
        url = url.join("app.js?revision=3").unwrap();
        assert_eq!(
            origin.read(url.as_str(), b"", &read).unwrap().0,
            "text/javascript; charset=utf-8"
        );
        for invalid in [
            url.join("../secret.js").unwrap(),
            url.join("a%2Fb.js").unwrap(),
            url.join("nested.html").unwrap(),
            Url::parse("https://example.invalid/app.js").unwrap(),
        ] {
            assert!(origin.read(invalid.as_str(), b"", &read).is_none());
        }
    }
}
