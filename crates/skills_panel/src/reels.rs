use anyhow::{Context as _, Result};
use futures::{AsyncWriteExt as _, StreamExt as _, channel::mpsc};
use gpui::{
    App, AsyncWindowContext, Entity, FocusHandle, Focusable, Pixels, Task, WeakEntity, Window,
    canvas, prelude::*,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use ui::{Button, ButtonStyle, prelude::*};
use util::ResultExt as _;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    notifications::DetachAndPromptErr as _,
};

use crate::ToggleReelsFocus;

const REELS_WINDOW_SCRIPT: &str = include_str!("../assets/reels_window.py");

type DeviceBounds = (i32, i32, i32, i32);

struct ReelsWindow {
    process: smol::process::Child,
    commands: Option<mpsc::UnboundedSender<String>>,
    _writer: Option<Task<()>>,
}

impl ReelsWindow {
    fn send(&self, command: String) {
        if let Some(commands) = &self.commands {
            commands.unbounded_send(command).log_err();
        }
    }

    fn is_running(&mut self) -> bool {
        matches!(self.process.try_status(), Ok(None))
    }
}

pub struct ReelsPanel {
    focus_handle: FocusHandle,
    position: DockPosition,
    active: bool,
    reels_window: Option<ReelsWindow>,
    embedded_bounds: Option<DeviceBounds>,
}

impl ReelsPanel {
    pub(crate) async fn load(
        _workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        cx.update(|_window, cx| cx.new(Self::new))
    }

    fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            position: DockPosition::Left,
            active: false,
            reels_window: None,
            embedded_bounds: None,
        }
    }

    fn embedding_parent(window: &Window) -> Option<u32> {
        match HasWindowHandle::window_handle(window).ok()?.as_raw() {
            RawWindowHandle::Xcb(handle) => Some(handle.window.get()),
            RawWindowHandle::Xlib(handle) => u32::try_from(handle.window).ok(),
            _ => None,
        }
    }

    fn sync_embedded_bounds(
        &mut self,
        bounds: gpui::Bounds<Pixels>,
        scale_factor: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.active {
            return;
        }
        let Some(parent) = Self::embedding_parent(window) else {
            return;
        };
        let device_bounds = (
            (f32::from(bounds.origin.x) * scale_factor).round() as i32,
            (f32::from(bounds.origin.y) * scale_factor).round() as i32,
            (f32::from(bounds.size.width) * scale_factor).round() as i32,
            (f32::from(bounds.size.height) * scale_factor).round() as i32,
        );

        let window_is_running = self
            .reels_window
            .as_mut()
            .is_some_and(|reels_window| reels_window.is_running());
        if !window_is_running {
            self.reels_window = None;
            self.embedded_bounds = None;
            self.start_window(Some(parent), window, cx);
        }

        if self.embedded_bounds != Some(device_bounds) {
            self.embedded_bounds = Some(device_bounds);
            if let Some(reels_window) = &self.reels_window {
                let (x, y, width, height) = device_bounds;
                reels_window.send(format!("bounds {x} {y} {width} {height}\n"));
                reels_window.send("show\n".into());
            }
        }
    }

    fn start_window(&mut self, parent: Option<u32>, window: &mut Window, cx: &mut Context<Self>) {
        let embedded = parent.is_some();
        cx.spawn(async move |this, cx| {
            let mut reels_window = cx.background_spawn(launch_reels_window(parent)).await?;
            let stdin = reels_window.process.stdin.take();
            if let Some(mut stdin) = stdin {
                let (sender, mut receiver) = mpsc::unbounded::<String>();
                reels_window.commands = Some(sender);
                reels_window._writer = Some(cx.background_spawn(async move {
                    while let Some(command) = receiver.next().await {
                        if stdin.write_all(command.as_bytes()).await.is_err()
                            || stdin.flush().await.is_err()
                        {
                            break;
                        }
                    }
                }));
            }
            this.update(cx, |this, cx| {
                this.reels_window = Some(reels_window);
                if embedded {
                    this.embedded_bounds = None;
                }
                cx.notify();
            })?;
            anyhow::Ok(())
        })
        .detach_and_prompt_err(
            "Failed to open Instagram Reels",
            window,
            cx,
            |error, _, _| Some(format!("{error:#}")),
        );
    }

    fn hide_embedded_window(&mut self) {
        if let Some(reels_window) = &self.reels_window {
            reels_window.send("hide\n".into());
        }
        self.embedded_bounds = None;
    }
}

async fn launch_reels_window(parent: Option<u32>) -> Result<ReelsWindow> {
    let directory = paths::data_dir().join("reels");
    smol::fs::create_dir_all(&directory)
        .await
        .context("creating the reels data directory")?;
    let script_path = directory.join("reels_window.py");
    smol::fs::write(&script_path, REELS_WINDOW_SCRIPT)
        .await
        .context("writing the reels window script")?;

    let mut command = util::command::new_command("python3");
    command
        .arg(&script_path)
        .arg(directory.join("profile"))
        .kill_on_drop(true);
    if let Some(parent) = parent {
        command
            .arg(parent.to_string())
            .stdin(smol::process::Stdio::piped());
    }
    let process = command
        .spawn()
        .context("starting python3 (needs python3-gi and gir1.2-webkit2-4.1)")?;
    Ok(ReelsWindow {
        process,
        commands: None,
        _writer: None,
    })
}

impl Focusable for ReelsPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl gpui::EventEmitter<PanelEvent> for ReelsPanel {}

impl Panel for ReelsPanel {
    fn persistent_name() -> &'static str {
        "Reels Panel"
    }

    fn panel_key() -> &'static str {
        "ReelsPanel"
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(
        &mut self,
        position: DockPosition,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        gpui::px(460.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::PlayOutlined)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Instagram Reels")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleReelsFocus)
    }

    fn set_active(&mut self, active: bool, _window: &mut Window, cx: &mut Context<Self>) {
        self.active = active;
        if !active {
            self.hide_embedded_window();
        }
        cx.notify();
    }

    fn activation_priority(&self) -> u32 {
        10
    }
}

impl Render for ReelsPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let container = v_flex()
            .key_context("ReelsPanel")
            .track_focus(&self.focus_handle)
            .size_full();

        if Self::embedding_parent(window).is_some() {
            let panel = cx.entity().downgrade();
            return container.child(
                canvas(
                    move |bounds, window, cx| {
                        let scale_factor = window.scale_factor();
                        panel
                            .update(cx, |panel, cx| {
                                panel.sync_embedded_bounds(bounds, scale_factor, window, cx)
                            })
                            .log_err();
                    },
                    |_, _, _, _| {},
                )
                .size_full(),
            );
        }

        container
            .p_4()
            .gap_3()
            .child(
                Label::new(
                    "Reels can only be embedded when Zed runs on X11. \
                     Start Zed with WAYLAND_DISPLAY unset to embed them here.",
                )
                .color(Color::Muted),
            )
            .child(
                Button::new("open-reels-window", "Open in a separate window")
                    .style(ButtonStyle::Filled)
                    .on_click(cx.listener(|this, _, window, cx| {
                        let window_is_running = this
                            .reels_window
                            .as_mut()
                            .is_some_and(|reels_window| reels_window.is_running());
                        if !window_is_running {
                            this.start_window(None, window, cx);
                        }
                    })),
            )
    }
}
