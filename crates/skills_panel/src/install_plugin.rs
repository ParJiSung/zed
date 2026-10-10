use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use collections::{HashMap, HashSet};
use editor::Editor;
use futures::future::Shared;
use fuzzy::{StringMatch, StringMatchCandidate, match_strings};
use gpui::{DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Task, WeakEntity};
use picker::{Picker, PickerDelegate};
use project::Project;
use serde::Deserialize;
use theme::ActiveTheme;
use ui::{Button, ButtonStyle, HighlightedLabel, ListItem, ListItemSpacing, prelude::*};
use util::{
    ResultExt as _,
    command::{Stdio, new_command},
};
use workspace::{ModalView, Workspace};

use crate::{local_project_roots, plugins_panel::PluginHost};

const MAX_RESULTS: usize = 100;
const MAX_OUTPUT_CHARS: usize = 4000;

impl PluginHost {
    fn cli_name(self) -> &'static str {
        match self {
            PluginHost::Claude => "claude",
            PluginHost::Codex => "codex",
        }
    }

    fn marketplace_file_hint(self) -> &'static str {
        match self {
            PluginHost::Claude => ".claude-plugin/marketplace.json",
            PluginHost::Codex => ".agents/plugins/marketplace.json",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    Marketplace,
    GitHub,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scope {
    User,
    Project,
    Local,
}

impl Scope {
    fn flag(self) -> &'static str {
        match self {
            Scope::User => "user",
            Scope::Project => "project",
            Scope::Local => "local",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Scope::User => "User",
            Scope::Project => "Project",
            Scope::Local => "Local",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct AvailablePlugin {
    plugin_id: String,
    name: String,
    marketplace: String,
    description: String,
    is_installed: bool,
    install_count: u64,
}

enum CatalogState {
    Loading,
    Loaded,
    Failed(SharedString),
}

enum Step {
    Browse,
    Confirm(AvailablePlugin),
    Running(SharedString),
    Finished { success: bool, output: SharedString },
}

pub(crate) struct InstallPluginModal {
    project: Entity<Project>,
    focus_handle: FocusHandle,
    host: PluginHost,
    source: Source,
    scope: Scope,
    catalog: Vec<AvailablePlugin>,
    catalog_state: CatalogState,
    /// When set, only plugins from these marketplaces are listed. It holds the
    /// marketplaces a GitHub repository just added.
    marketplace_filter: Option<HashSet<String>>,
    /// Marketplaces that existed before a GitHub repository was added, so the
    /// new ones can be told apart once the catalog reloads.
    marketplaces_before_add: Option<HashSet<String>>,
    notice: Option<SharedString>,
    picker: Entity<Picker<PluginPickerDelegate>>,
    repository_editor: Entity<Editor>,
    step: Step,
    task: Task<()>,
}

impl InstallPluginModal {
    pub(crate) fn toggle(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let project = workspace.project().clone();
        workspace.toggle_modal(window, cx, move |window, cx| Self::new(project, window, cx));
    }

    fn new(project: Entity<Project>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let modal = cx.entity().downgrade();
        let picker = cx.new(|cx| {
            Picker::uniform_list(PluginPickerDelegate::new(modal), window, cx).embedded()
        });
        let repository_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("owner/repo or https://github.com/owner/repo", window, cx);
            editor
        });
        let mut this = Self {
            project,
            focus_handle: cx.focus_handle(),
            host: PluginHost::Claude,
            source: Source::Marketplace,
            scope: Scope::User,
            catalog: Vec::new(),
            catalog_state: CatalogState::Loading,
            marketplace_filter: None,
            marketplaces_before_add: None,
            notice: None,
            picker,
            repository_editor,
            step: Step::Browse,
            task: Task::ready(()),
        };
        this.reload_catalog(window, cx);
        this
    }

    fn environment(&self, cx: &mut Context<Self>) -> Shared<Task<Option<HashMap<String, String>>>> {
        self.project.update(cx, |project, cx| {
            project
                .environment()
                .update(cx, |environment, cx| environment.default_environment(cx))
        })
    }

    fn project_directory(&self, cx: &App) -> Option<PathBuf> {
        local_project_roots(&self.project, cx).into_iter().next()
    }

    fn set_host(&mut self, host: PluginHost, window: &mut Window, cx: &mut Context<Self>) {
        if self.host == host {
            return;
        }
        self.host = host;
        self.notice = None;
        self.reload_catalog(window, cx);
    }

    fn set_source(&mut self, source: Source, window: &mut Window, cx: &mut Context<Self>) {
        self.source = source;
        self.notice = None;
        window.focus(&Focusable::focus_handle(self, cx), cx);
        cx.notify();
    }

    fn reload_catalog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.catalog_state = CatalogState::Loading;
        self.catalog.clear();
        self.marketplace_filter = None;
        self.sync_picker(window, cx);
        cx.notify();

        let host = self.host;
        let environment = self.environment(cx);
        self.task = cx.spawn_in(window, async move |this, cx| {
            let environment = environment.await.unwrap_or_default();
            let result = cx
                .background_executor()
                .spawn(async move { load_catalog(host, &environment).await })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.finish_catalog_load(host, result, window, cx);
            })
            .log_err();
        });
    }

    fn finish_catalog_load(
        &mut self,
        host: PluginHost,
        result: Result<Vec<AvailablePlugin>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if host != self.host {
            return;
        }
        match result {
            Ok(catalog) => {
                if let Some(before) = self.marketplaces_before_add.take() {
                    let added = catalog
                        .iter()
                        .map(|plugin| plugin.marketplace.clone())
                        .filter(|marketplace| !before.contains(marketplace))
                        .collect::<HashSet<_>>();
                    if added.is_empty() {
                        self.notice =
                            Some("That marketplace was already added. Showing all plugins.".into());
                    } else {
                        self.notice = Some("Marketplace added. Pick a plugin to install.".into());
                        self.marketplace_filter = Some(added);
                    }
                }
                self.catalog = catalog;
                self.catalog_state = CatalogState::Loaded;
            }
            Err(error) => {
                self.catalog_state = CatalogState::Failed(format!("{error:#}").into());
            }
        }
        self.sync_picker(window, cx);
        cx.notify();
    }

    fn sync_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let plugins = self
            .catalog
            .iter()
            .filter(|plugin| {
                self.marketplace_filter
                    .as_ref()
                    .is_none_or(|filter| filter.contains(&plugin.marketplace))
            })
            .cloned()
            .collect::<Vec<_>>();
        self.picker.update(cx, |picker, cx| {
            picker.delegate.set_plugins(plugins);
            picker.refresh(window, cx);
        });
    }

    fn select_plugin(
        &mut self,
        plugin: AvailablePlugin,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.step = Step::Confirm(plugin);
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn back_to_browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.step = Step::Browse;
        window.focus(&Focusable::focus_handle(self, cx), cx);
        cx.notify();
    }

    fn run_command(
        &mut self,
        running_label: &'static str,
        arguments: Vec<String>,
        directory: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
        on_finished: impl FnOnce(&mut Self, Result<CommandOutput>, &mut Window, &mut Context<Self>)
        + 'static,
    ) {
        let host = self.host;
        self.step = Step::Running(running_label.into());
        window.focus(&self.focus_handle, cx);
        cx.notify();

        let environment = self.environment(cx);
        self.task = cx.spawn_in(window, async move |this, cx| {
            let environment = environment.await.unwrap_or_default();
            let result = cx
                .background_executor()
                .spawn(async move {
                    run_cli(host.cli_name(), &arguments, &environment, &directory).await
                })
                .await;
            this.update_in(cx, |this, window, cx| on_finished(this, result, window, cx))
                .log_err();
        });
    }

    fn add_marketplace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let source = self.repository_editor.read(cx).text(cx).trim().to_string();
        if !is_safe_argument(&source) {
            self.notice = Some(
                "Enter owner/repo or a full git URL, without spaces and not starting with “-”."
                    .into(),
            );
            cx.notify();
            return;
        }
        let before = self
            .catalog
            .iter()
            .map(|plugin| plugin.marketplace.clone())
            .collect::<HashSet<_>>();
        let arguments = marketplace_add_arguments(&source);
        let directory = util::paths::home_dir().to_path_buf();
        self.run_command(
            "Adding the marketplace…",
            arguments,
            directory,
            window,
            cx,
            move |this, result, window, cx| match result {
                Ok(output) if output.success => {
                    this.step = Step::Browse;
                    this.marketplaces_before_add = Some(before);
                    this.reload_catalog(window, cx);
                }
                other => this.finish_with_result(other, cx),
            },
        );
    }

    fn install(&mut self, plugin: AvailablePlugin, window: &mut Window, cx: &mut Context<Self>) {
        let directory = match self.scope {
            Scope::User => util::paths::home_dir().to_path_buf(),
            Scope::Project | Scope::Local => match self.project_directory(cx) {
                Some(directory) => directory,
                None => {
                    self.finish_with_result(
                        Err(anyhow::anyhow!(
                            "Open a local project to install at this scope."
                        )),
                        cx,
                    );
                    return;
                }
            },
        };
        let arguments = install_arguments(self.host, &plugin.plugin_id, self.scope);
        self.run_command(
            "Installing…",
            arguments,
            directory,
            window,
            cx,
            move |this, result, window, cx| {
                let succeeded = result.as_ref().is_ok_and(|output| output.success);
                this.finish_with_result(result, cx);
                if succeeded {
                    this.reload_catalog_keeping_step(window, cx);
                }
            },
        );
    }

    /// Refreshes the "installed" markers without leaving the result screen.
    fn reload_catalog_keeping_step(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let filter = self.marketplace_filter.take();
        let step = std::mem::replace(&mut self.step, Step::Browse);
        self.reload_catalog(window, cx);
        self.marketplace_filter = filter;
        self.step = step;
    }

    fn finish_with_result(&mut self, result: Result<CommandOutput>, cx: &mut Context<Self>) {
        let (success, output) = match result {
            Ok(output) => (output.success, output.text()),
            Err(error) => (false, format!("{error:#}")),
        };
        self.step = Step::Finished {
            success,
            output: truncate_output(&output).into(),
        };
        cx.notify();
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.step, Step::Running(_)) {
            cx.emit(DismissEvent);
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.step, Step::Browse) && self.source == Source::GitHub {
            self.add_marketplace(window, cx);
        }
    }

    fn render_toggle_button(
        &self,
        id: &'static str,
        label: &'static str,
        selected: bool,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        Button::new(id, label)
            .style(if selected {
                ButtonStyle::Tinted(ui::TintColor::Accent)
            } else {
                ButtonStyle::Subtle
            })
            .toggle_state(selected)
            .on_click(cx.listener(move |this, _, window, cx| on_click(this, window, cx)))
    }

    fn render_browse(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let host = self.host;
        let source = self.source;
        let github_has_results = self.marketplace_filter.is_some();

        let status = match &self.catalog_state {
            CatalogState::Loading => Some(
                Label::new("Loading available plugins…")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            ),
            CatalogState::Failed(message) => Some(
                Label::new(message.clone())
                    .size(LabelSize::Small)
                    .color(Color::Error),
            ),
            CatalogState::Loaded => None,
        };
        let notice = self.notice.clone().map(|notice| {
            Label::new(notice)
                .size(LabelSize::Small)
                .color(Color::Accent)
        });

        let github_controls = (source == Source::GitHub).then(|| {
            v_flex()
                .gap_1()
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .border_1()
                                .border_color(cx.theme().colors().border)
                                .child(self.repository_editor.clone()),
                        )
                        .child(
                            Button::new("add-marketplace", "Add marketplace")
                                .style(ButtonStyle::Filled)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.add_marketplace(window, cx);
                                })),
                        ),
                )
                .child(
                    Label::new(format!(
                        "The repository must contain a plugin marketplace ({}). \
                         Adding it only downloads the catalog; nothing is installed until \
                         you pick a plugin.",
                        host.marketplace_file_hint()
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
        });

        let show_picker = source == Source::Marketplace || github_has_results;

        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_1()
                    .child(self.render_toggle_button(
                        "host-claude",
                        "Claude Code",
                        host == PluginHost::Claude,
                        |this, window, cx| this.set_host(PluginHost::Claude, window, cx),
                        cx,
                    ))
                    .child(self.render_toggle_button(
                        "host-codex",
                        "Codex",
                        host == PluginHost::Codex,
                        |this, window, cx| this.set_host(PluginHost::Codex, window, cx),
                        cx,
                    ))
                    .child(div().flex_1())
                    .child(self.render_toggle_button(
                        "source-marketplace",
                        "Marketplace",
                        source == Source::Marketplace,
                        |this, window, cx| this.set_source(Source::Marketplace, window, cx),
                        cx,
                    ))
                    .child(self.render_toggle_button(
                        "source-github",
                        "GitHub repo",
                        source == Source::GitHub,
                        |this, window, cx| this.set_source(Source::GitHub, window, cx),
                        cx,
                    )),
            )
            .children(github_controls)
            .children(notice)
            .children(status)
            .when(show_picker, |this| {
                this.child(
                    div()
                        .h(rems(18.))
                        .overflow_hidden()
                        .child(self.picker.clone()),
                )
            })
    }

    fn render_confirm(
        &mut self,
        plugin: AvailablePlugin,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let host = self.host;
        let scope = self.scope;
        let command = format!(
            "{} {}",
            host.cli_name(),
            install_arguments(host, &plugin.plugin_id, scope).join(" ")
        );
        let project_directory = self.project_directory(cx);
        let scope_buttons = (host == PluginHost::Claude).then(|| {
            h_flex()
                .gap_1()
                .child(
                    Label::new("Install for:")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .children([Scope::User, Scope::Project, Scope::Local].map(|option| {
                    let available = option == Scope::User || project_directory.is_some();
                    Button::new(option.label(), option.label())
                        .style(if option == scope {
                            ButtonStyle::Tinted(ui::TintColor::Accent)
                        } else {
                            ButtonStyle::Subtle
                        })
                        .toggle_state(option == scope)
                        .disabled(!available)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.scope = option;
                            cx.notify();
                        }))
                }))
        });
        let install_plugin = plugin.clone();

        v_flex()
            .gap_2()
            .child(Label::new(plugin.name.clone()).size(LabelSize::Large))
            .child(
                Label::new(format!("from {}", plugin.marketplace))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .when(!plugin.description.is_empty(), |this| {
                this.child(Label::new(plugin.description.clone()).size(LabelSize::Small))
            })
            .children(scope_buttons)
            .child(
                v_flex()
                    .gap_1()
                    .p_2()
                    .rounded_md()
                    .bg(cx.theme().colors().editor_background)
                    .child(
                        Label::new("This will run")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(Label::new(command).size(LabelSize::Small)),
            )
            .child(
                Label::new(
                    "Plugins can run code on this computer through hooks, scripts and MCP \
                     servers. Only install plugins from sources you trust.",
                )
                .size(LabelSize::Small)
                .color(Color::Warning),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(Button::new("back", "Back").on_click(cx.listener(
                        |this, _, window, cx| {
                            this.back_to_browse(window, cx);
                        },
                    )))
                    .child(
                        Button::new("install", "Install")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.install(install_plugin.clone(), window, cx);
                            })),
                    ),
            )
    }

    fn render_finished(
        &mut self,
        success: bool,
        output: SharedString,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        v_flex()
            .gap_2()
            .child(if success {
                Label::new("Done")
                    .size(LabelSize::Large)
                    .color(Color::Success)
            } else {
                Label::new("That did not work")
                    .size(LabelSize::Large)
                    .color(Color::Error)
            })
            .child(
                div()
                    .id("install-output")
                    .max_h(rems(16.))
                    .overflow_y_scroll()
                    .p_2()
                    .rounded_md()
                    .bg(cx.theme().colors().editor_background)
                    .child(Label::new(output).size(LabelSize::Small)),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(Button::new("back-to-list", "Back to list").on_click(
                        cx.listener(|this, _, window, cx| this.back_to_browse(window, cx)),
                    ))
                    .child(
                        Button::new("close", "Close")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    ),
            )
    }
}

impl Render for InstallPluginModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match &self.step {
            Step::Browse => self.render_browse(cx).into_any_element(),
            Step::Confirm(plugin) => {
                let plugin = plugin.clone();
                self.render_confirm(plugin, cx).into_any_element()
            }
            Step::Running(message) => Label::new(message.clone())
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
            Step::Finished { success, output } => {
                let (success, output) = (*success, output.clone());
                self.render_finished(success, output, cx).into_any_element()
            }
        };

        v_flex()
            .key_context("InstallPluginModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_3(cx)
            .w(rems(38.))
            .p_3()
            .gap_2()
            .child(Label::new("Install a plugin").size(LabelSize::Large))
            .child(content)
    }
}

impl Focusable for InstallPluginModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match (&self.step, self.source) {
            (Step::Browse, Source::Marketplace) => self.picker.focus_handle(cx),
            (Step::Browse, Source::GitHub) => self.repository_editor.focus_handle(cx),
            _ => self.focus_handle.clone(),
        }
    }
}

impl EventEmitter<DismissEvent> for InstallPluginModal {}
impl ModalView for InstallPluginModal {}

struct PluginPickerDelegate {
    modal: WeakEntity<InstallPluginModal>,
    plugins: Vec<AvailablePlugin>,
    matches: Vec<StringMatch>,
    selected_index: usize,
}

impl PluginPickerDelegate {
    fn new(modal: WeakEntity<InstallPluginModal>) -> Self {
        Self {
            modal,
            plugins: Vec::new(),
            matches: Vec::new(),
            selected_index: 0,
        }
    }

    fn set_plugins(&mut self, plugins: Vec<AvailablePlugin>) {
        self.plugins = plugins;
        self.matches.clear();
        self.selected_index = 0;
    }
}

impl PickerDelegate for PluginPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "plugin installer"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> std::sync::Arc<str> {
        "Search plugins…".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(plugin) = self
            .matches
            .get(self.selected_index)
            .and_then(|mat| self.plugins.get(mat.candidate_id))
            .cloned()
        else {
            return;
        };
        self.modal
            .update(cx, |modal, cx| modal.select_plugin(plugin, window, cx))
            .log_err();
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.modal
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .log_err();
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let background = cx.background_executor().clone();
        let candidates = self
            .plugins
            .iter()
            .enumerate()
            .map(|(index, plugin)| {
                StringMatchCandidate::new(index, &format!("{} {}", plugin.name, plugin.marketplace))
            })
            .collect::<Vec<_>>();
        cx.spawn_in(window, async move |this, cx| {
            let matches = if query.is_empty() {
                candidates
                    .into_iter()
                    .take(MAX_RESULTS)
                    .map(|candidate| StringMatch {
                        candidate_id: candidate.id,
                        string: candidate.string,
                        positions: Vec::new(),
                        score: 0.0,
                    })
                    .collect()
            } else {
                match_strings(
                    &candidates,
                    &query,
                    false,
                    true,
                    MAX_RESULTS,
                    &Default::default(),
                    background,
                )
                .await
            };
            this.update_in(cx, |this, window, cx| {
                this.delegate.matches = matches;
                this.set_selected_index(0, None, false, window, cx);
                cx.notify();
            })
            .log_err();
        })
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let mat = self.matches.get(ix)?;
        let plugin = self.plugins.get(mat.candidate_id)?;
        // Match positions index into "{name} {marketplace}"; only the name is highlighted.
        let positions = mat
            .positions
            .iter()
            .copied()
            .filter(|position| *position < plugin.name.len())
            .collect::<Vec<_>>();
        let installed_label = plugin.is_installed.then(|| {
            Label::new("Installed")
                .size(LabelSize::Small)
                .color(Color::Success)
        });

        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    v_flex()
                        .min_w_0()
                        .child(
                            h_flex()
                                .gap_1()
                                .child(HighlightedLabel::new(plugin.name.clone(), positions))
                                .child(
                                    Label::new(plugin.marketplace.clone())
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                ),
                        )
                        .child(
                            Label::new(plugin.description.clone())
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                )
                .end_slot::<Label>(installed_label),
        )
    }
}

struct CommandOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

impl CommandOutput {
    fn text(&self) -> String {
        let mut text = self.stdout.trim_end().to_string();
        let stderr = self.stderr.trim_end();
        if !stderr.is_empty() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(stderr);
        }
        if text.is_empty() {
            text.push_str(if self.success {
                "The command finished without any output."
            } else {
                "The command failed without any output."
            });
        }
        text
    }
}

fn truncate_output(output: &str) -> String {
    if output.chars().count() <= MAX_OUTPUT_CHARS {
        return output.to_string();
    }
    let mut truncated = output.chars().take(MAX_OUTPUT_CHARS).collect::<String>();
    truncated.push('…');
    truncated
}

fn find_program(program: &str, path_variable: &str) -> Option<PathBuf> {
    let names = program_file_names(program);
    std::env::split_paths(path_variable)
        .flat_map(|directory| names.iter().map(move |name| directory.join(name)))
        .find(|candidate| candidate.is_file())
}

/// On Windows, command line tools installed through npm are `.cmd` shims and
/// others are `.exe` files, so the bare name is never the file on disk.
#[cfg(windows)]
fn program_file_names(program: &str) -> Vec<String> {
    let extensions = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    extensions
        .split(';')
        .filter(|extension| !extension.is_empty())
        .map(|extension| format!("{program}{}", extension.to_lowercase()))
        .chain([program.to_string()])
        .collect()
}

#[cfg(not(windows))]
fn program_file_names(program: &str) -> Vec<String> {
    vec![program.to_string()]
}

/// Runs the host's command line tool. The program is looked up on the PATH of
/// the user's shell environment, because Zed started from a launcher does not
/// inherit it (`claude` and `codex` are typically installed under nvm).
async fn run_cli(
    program: &str,
    arguments: &[String],
    environment: &HashMap<String, String>,
    directory: &Path,
) -> Result<CommandOutput> {
    let path_variable = environment
        .get("PATH")
        .map(String::as_str)
        .unwrap_or_default();
    let program_path = find_program(program, path_variable).with_context(|| {
        format!(
            "`{program}` was not found on your PATH. Install it, or start Zed from a terminal \
             where it is available."
        )
    })?;
    let mut command = new_command(&program_path);
    command
        .args(arguments)
        .envs(environment)
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = command
        .spawn()
        .with_context(|| format!("failed to start `{program}`"))?
        .output()
        .await
        .with_context(|| format!("failed to run `{program}`"))?;
    Ok(CommandOutput {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

async fn load_catalog(
    host: PluginHost,
    environment: &HashMap<String, String>,
) -> Result<Vec<AvailablePlugin>> {
    let directory = util::paths::home_dir();
    let arguments: &[&str] = match host {
        PluginHost::Claude => &["plugin", "list", "--available", "--json"],
        PluginHost::Codex => &["plugin", "list"],
    };
    let arguments = arguments
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let output = run_cli(host.cli_name(), &arguments, environment, directory).await?;
    if !output.success {
        bail!("{}", output.text());
    }
    match host {
        PluginHost::Claude => parse_claude_catalog(&output.stdout),
        PluginHost::Codex => Ok(parse_codex_plugin_list(&output.stdout)),
    }
}

#[derive(Deserialize)]
struct ClaudeCatalog {
    #[serde(default)]
    installed: Vec<ClaudeInstalledEntry>,
    #[serde(default)]
    available: Vec<ClaudeAvailableEntry>,
}

#[derive(Deserialize)]
struct ClaudeInstalledEntry {
    id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeAvailableEntry {
    plugin_id: String,
    name: String,
    description: Option<String>,
    marketplace_name: String,
    install_count: Option<u64>,
}

fn parse_claude_catalog(json: &str) -> Result<Vec<AvailablePlugin>> {
    let catalog: ClaudeCatalog =
        serde_json::from_str(json).context("could not read the plugin list from `claude`")?;
    let installed = catalog
        .installed
        .into_iter()
        .map(|entry| entry.id)
        .collect::<HashSet<_>>();
    let mut plugins = catalog
        .available
        .into_iter()
        .map(|entry| AvailablePlugin {
            is_installed: installed.contains(&entry.plugin_id),
            plugin_id: entry.plugin_id,
            name: entry.name,
            marketplace: entry.marketplace_name,
            description: entry.description.unwrap_or_default(),
            install_count: entry.install_count.unwrap_or_default(),
        })
        .collect::<Vec<_>>();
    plugins.sort_by(|a, b| {
        b.install_count
            .cmp(&a.install_count)
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(plugins)
}

/// `codex plugin list` prints a table per marketplace. Rows start with a
/// `plugin@marketplace` id followed by a status such as `not installed`.
fn parse_codex_plugin_list(output: &str) -> Vec<AvailablePlugin> {
    let mut plugins = Vec::new();
    for line in output.lines() {
        let mut tokens = line.split_whitespace();
        let Some(plugin_id) = tokens.next() else {
            continue;
        };
        if plugin_id.contains('/') {
            continue;
        }
        let Some((name, marketplace)) = plugin_id.split_once('@') else {
            continue;
        };
        if name.is_empty() || marketplace.is_empty() {
            continue;
        }
        let status = tokens.collect::<Vec<_>>().join(" ");
        plugins.push(AvailablePlugin {
            plugin_id: plugin_id.to_string(),
            name: name.to_string(),
            marketplace: marketplace.to_string(),
            description: String::new(),
            is_installed: !(status.starts_with("not installed") || status.starts_with("available")),
            install_count: 0,
        });
    }
    plugins
}

/// Everything typed in the dialog reaches the command line tools as a single
/// argument without a shell, so the only thing to rule out is a value that
/// would be read as an option.
fn is_safe_argument(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && !value
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
}

fn install_arguments(host: PluginHost, plugin_id: &str, scope: Scope) -> Vec<String> {
    match host {
        PluginHost::Claude => ["plugin", "install", plugin_id, "--scope", scope.flag()]
            .map(ToString::to_string)
            .to_vec(),
        PluginHost::Codex => ["plugin", "add", plugin_id]
            .map(ToString::to_string)
            .to_vec(),
    }
}

fn marketplace_add_arguments(source: &str) -> Vec<String> {
    ["plugin", "marketplace", "add", source]
        .map(ToString::to_string)
        .to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_parse_claude_catalog() -> Result<()> {
        let plugins = parse_claude_catalog(
            r#"{
                "installed": [{"id": "b@official", "scope": "user"}],
                "available": [
                    {"pluginId": "a@official", "name": "a", "description": "Alpha",
                     "marketplaceName": "official", "installCount": 5, "source": {"source": "git"}},
                    {"pluginId": "b@official", "name": "b", "marketplaceName": "official",
                     "installCount": 90, "source": "./b"},
                    {"pluginId": "c@other", "name": "c", "marketplaceName": "other"}
                ]
            }"#,
        )?;
        assert_eq!(
            plugins
                .iter()
                .map(|plugin| (plugin.plugin_id.as_str(), plugin.is_installed))
                .collect::<Vec<_>>(),
            vec![
                ("b@official", true),
                ("a@official", false),
                ("c@other", false)
            ]
        );
        assert_eq!(plugins[1].description, "Alpha");
        assert_eq!(plugins[0].description, "");
        Ok(())
    }

    #[test]
    fn test_parse_claude_catalog_rejects_garbage() {
        assert!(parse_claude_catalog("not json").is_err());
    }

    #[test]
    fn test_parse_codex_plugin_list() {
        let plugins = parse_codex_plugin_list(
            "Marketplace `openai-curated`\n\
             /home/me/.codex/.tmp/plugins/.agents/plugins/marketplace.json\n\
             \n\
             PLUGIN                  STATUS         VERSION  PATH\n\
             linear@openai-curated   not installed          /home/me/x/linear\n\
             gmail@openai-curated    installed      1.2.0    /home/me/x/gmail\n",
        );
        assert_eq!(
            plugins
                .iter()
                .map(|plugin| (
                    plugin.name.as_str(),
                    plugin.marketplace.as_str(),
                    plugin.is_installed
                ))
                .collect::<Vec<_>>(),
            vec![
                ("linear", "openai-curated", false),
                ("gmail", "openai-curated", true)
            ]
        );
    }

    #[test]
    fn test_is_safe_argument() {
        assert!(is_safe_argument("owner/repo"));
        assert!(is_safe_argument("https://github.com/owner/repo.git"));
        assert!(is_safe_argument("owner/repo@main"));
        assert!(!is_safe_argument(""));
        assert!(!is_safe_argument("--help"));
        assert!(!is_safe_argument("owner/repo --scope local"));
        assert!(!is_safe_argument("owner/repo\n"));
    }

    #[test]
    fn test_install_arguments() {
        assert_eq!(
            install_arguments(PluginHost::Claude, "a@b", Scope::Local),
            vec!["plugin", "install", "a@b", "--scope", "local"]
        );
        assert_eq!(
            install_arguments(PluginHost::Codex, "a@b", Scope::User),
            vec!["plugin", "add", "a@b"]
        );
        assert_eq!(
            marketplace_add_arguments("owner/repo"),
            vec!["plugin", "marketplace", "add", "owner/repo"]
        );
    }

    #[test]
    fn test_find_program() {
        assert_eq!(
            find_program("definitely-not-a-real-program", "/nonexistent"),
            None
        );
        assert_eq!(find_program("sh", ""), None);
    }

    #[test]
    fn test_truncate_output() {
        assert_eq!(truncate_output("short"), "short");
        let long = "x".repeat(MAX_OUTPUT_CHARS + 10);
        assert_eq!(truncate_output(&long).chars().count(), MAX_OUTPUT_CHARS + 1);
    }
}
