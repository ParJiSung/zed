use std::path::PathBuf;

use anyhow::{Context as _, Result, anyhow};
use gpui::{Bounds, Pixels, Window};
use wry::{
    Rect, WebContext, WebView, WebViewBuilder,
    dpi::{LogicalPosition, LogicalSize},
};

/// Instagram moves between reels by scrolling its feed container one screen
/// at a time. Synthetic key presses are ignored by the page, so the feed is
/// scrolled directly.
const NAVIGATE_SCRIPT: &str = r#"
(() => {
    const direction = __DIRECTION__;
    const scrollers = [document.scrollingElement, ...document.querySelectorAll("main, main *")]
        .filter((element) => {
            if (!element || element.scrollHeight <= element.clientHeight + 10) {
                return false;
            }
            const overflow = getComputedStyle(element).overflowY;
            return element === document.scrollingElement || overflow === "auto" || overflow === "scroll";
        });
    const feed = scrollers.sort((a, b) => b.clientHeight - a.clientHeight)[0];
    if (feed) {
        feed.scrollBy({ top: direction * feed.clientHeight, behavior: "smooth" });
    }
})();
"#;

/// The Reels page shown in the platform's own web view, as a child of Zed's
/// window: WKWebView on macOS and WebView2 on Windows.
pub struct NativeReelsView {
    webview: WebView,
    bounds: Bounds<Pixels>,
    visible: bool,
    // The web view stores cookies here, which keeps the Instagram login.
    _web_context: WebContext,
}

impl NativeReelsView {
    pub fn new(
        url: &str,
        data_directory: PathBuf,
        bounds: Bounds<Pixels>,
        window: &Window,
    ) -> Result<Self> {
        std::fs::create_dir_all(&data_directory)
            .with_context(|| format!("creating {}", data_directory.display()))?;
        let mut web_context = WebContext::new(Some(data_directory));
        let webview = WebViewBuilder::new_with_web_context(&mut web_context)
            .with_url(url)
            .with_bounds(rect(bounds))
            .build_as_child(window)
            .map_err(|error| anyhow!("creating the Reels web view: {error}"))?;
        Ok(Self {
            webview,
            bounds,
            visible: true,
            _web_context: web_context,
        })
    }

    pub fn show_at(&mut self, bounds: Bounds<Pixels>) -> Result<()> {
        if bounds != self.bounds {
            self.webview
                .set_bounds(rect(bounds))
                .map_err(|error| anyhow!("moving the Reels web view: {error}"))?;
            self.bounds = bounds;
        }
        if !self.visible {
            self.webview
                .set_visible(true)
                .map_err(|error| anyhow!("showing the Reels web view: {error}"))?;
            self.visible = true;
        }
        Ok(())
    }

    pub fn hide(&mut self) -> Result<()> {
        if self.visible {
            self.webview
                .set_visible(false)
                .map_err(|error| anyhow!("hiding the Reels web view: {error}"))?;
            self.visible = false;
        }
        Ok(())
    }

    pub fn navigate(&self, next: bool) -> Result<()> {
        let script = NAVIGATE_SCRIPT.replace("__DIRECTION__", if next { "1" } else { "-1" });
        self.webview
            .evaluate_script(&script)
            .map_err(|error| anyhow!("switching reels: {error}"))
    }
}

/// GPUI and wry both measure in logical pixels relative to the window's
/// content area.
fn rect(bounds: Bounds<Pixels>) -> Rect {
    Rect {
        position: LogicalPosition::new(
            f64::from(f32::from(bounds.origin.x)),
            f64::from(f32::from(bounds.origin.y)),
        )
        .into(),
        size: LogicalSize::new(
            f64::from(f32::from(bounds.size.width)),
            f64::from(f32::from(bounds.size.height)),
        )
        .into(),
    }
}
