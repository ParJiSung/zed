use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Result;
use collections::HashMap;
use fs::Fs;
use gpui::{AsyncWindowContext, Entity, FocusHandle, Task, WeakEntity};
use serde::Deserialize;
use ui::{ListItem, ListItemSpacing, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{Workspace, dock::DockPosition};

use crate::{
    TogglePluginsFocus, codex_home, empty_message, file_name_string, load_json, open_path,
    poll_task, read_dir_paths, section_header,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum PluginHost {
    Claude,
    Codex,
}

impl PluginHost {
    fn label(self) -> &'static str {
        match self {
            PluginHost::Claude => "Claude Code",
            PluginHost::Codex => "Codex",
        }
    }

    fn icon(self) -> IconName {
        match self {
            PluginHost::Claude => IconName::AiClaude,
            PluginHost::Codex => IconName::AiOpenAi,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct PluginEntry {
    host: PluginHost,
    name: SharedString,
    marketplace: SharedString,
    version: SharedString,
    description: SharedString,
    /// Where the plugin was installed: `user`, or `project`/`local` with the
    /// project's directory name.
    scope: Option<SharedString>,
    /// `None` when the host's own configuration does not say.
    enabled: Option<bool>,
    path_to_open: Option<PathBuf>,
}

pub struct PluginsPanel {
    workspace: WeakEntity<Workspace>,
    fs: Arc<dyn Fs>,
    focus_handle: FocusHandle,
    position: DockPosition,
    active: bool,
    plugins: Vec<PluginEntry>,
    refresh_task: Task<()>,
    _poll_task: Task<()>,
}

impl PluginsPanel {
    pub(crate) async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, _window, cx| {
            let fs = workspace.app_state().fs.clone();
            let weak_workspace = workspace.weak_handle();
            cx.new(|cx| Self::new(weak_workspace, fs, cx))
        })
    }

    fn new(workspace: WeakEntity<Workspace>, fs: Arc<dyn Fs>, cx: &mut Context<Self>) -> Self {
        Self {
            workspace,
            fs,
            focus_handle: cx.focus_handle(),
            position: DockPosition::Left,
            active: false,
            plugins: Vec::new(),
            refresh_task: Task::ready(()),
            _poll_task: poll_task(cx, |panel: &mut Self, cx| {
                if panel.active {
                    panel.refresh(cx);
                }
            }),
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let fs = self.fs.clone();
        let scan = cx.background_spawn(async move { scan_plugins(fs.as_ref()).await });
        self.refresh_task = cx.spawn(async move |this, cx| {
            let plugins = scan.await;
            this.update(cx, |panel, cx| {
                if panel.plugins != plugins {
                    panel.plugins = plugins;
                    cx.notify();
                }
            })
            .log_err();
        });
    }

    fn render_plugin(
        &self,
        index: usize,
        plugin: &PluginEntry,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let detail = [
            Some(&plugin.marketplace),
            Some(&plugin.version),
            plugin.scope.as_ref(),
        ]
        .into_iter()
        .flatten()
        .filter(|part| !part.is_empty())
        .map(|part| part.to_string())
        .collect::<Vec<_>>()
        .join(" · ");
        let status_icon: Option<Icon> = plugin.enabled.map(|enabled| {
            if enabled {
                Icon::new(IconName::Check)
                    .size(IconSize::Small)
                    .color(Color::Success)
            } else {
                Icon::new(IconName::Close)
                    .size(IconSize::Small)
                    .color(Color::Muted)
            }
        });
        let tooltip_text = if plugin.description.is_empty() {
            plugin.name.clone()
        } else {
            plugin.description.clone()
        };
        let path_to_open = plugin.path_to_open.clone();

        ListItem::new(("plugin", index))
            .spacing(ListItemSpacing::Sparse)
            .start_slot::<Icon>(status_icon)
            .child(
                v_flex()
                    .min_w_0()
                    .child(Label::new(plugin.name.clone()).truncate())
                    .child(
                        Label::new(detail)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                    ),
            )
            .tooltip(Tooltip::text(tooltip_text))
            .on_click(cx.listener(move |panel, _, window, cx| {
                if let Some(path) = &path_to_open {
                    open_path(&panel.workspace, path.clone(), window, cx);
                }
            }))
    }
}

impl Render for PluginsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut content = v_flex()
            .id("plugins-panel")
            .key_context("PluginsPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_y_scroll();

        if self.plugins.is_empty() {
            return content
                .child(section_header("Plugins", IconName::Blocks))
                .child(empty_message("No Claude Code or Codex plugins found"));
        }

        let mut current_host = None;
        for (index, plugin) in self.plugins.iter().enumerate() {
            if current_host != Some(plugin.host) {
                current_host = Some(plugin.host);
                let count = self
                    .plugins
                    .iter()
                    .filter(|other| other.host == plugin.host)
                    .count();
                content = content.child(section_header(
                    format!("{} · {count}", plugin.host.label()),
                    plugin.host.icon(),
                ));
            }
            content = content.child(self.render_plugin(index, plugin, cx));
        }
        content
    }
}

impl_dock_panel!(
    PluginsPanel,
    name: "Plugins Panel",
    key: "PluginsPanel",
    tooltip: "Plugins Panel",
    icon: IconName::Blocks,
    toggle: TogglePluginsFocus,
    priority: 10
);

async fn scan_plugins(fs: &dyn Fs) -> Vec<PluginEntry> {
    let mut plugins = Vec::new();
    load_claude_plugins(fs, &util::paths::home_dir(), &mut plugins).await;
    load_codex_plugins(fs, &codex_home(), &mut plugins).await;
    plugins.sort_by(|a, b| (a.host, &a.name, &a.scope).cmp(&(b.host, &b.name, &b.scope)));
    plugins
}

/// `.claude-plugin/plugin.json` for Claude Code, `.codex-plugin/plugin.json`
/// for Codex.
#[derive(Deserialize)]
struct PluginManifest {
    name: Option<String>,
    version: Option<String>,
    description: Option<String>,
}

#[derive(Deserialize)]
struct ClaudeInstalledPlugins {
    #[serde(default)]
    plugins: HashMap<String, Vec<ClaudeInstall>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeInstall {
    scope: Option<String>,
    project_path: Option<PathBuf>,
    install_path: PathBuf,
    version: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeSettings {
    #[serde(default)]
    enabled_plugins: HashMap<String, bool>,
}

async fn load_claude_plugins(fs: &dyn Fs, home: &Path, plugins: &mut Vec<PluginEntry>) {
    let Some(installed) = load_json::<ClaudeInstalledPlugins>(
        fs,
        &home.join(".claude/plugins/installed_plugins.json"),
    )
    .await
    else {
        return;
    };

    let mut enabled_by_settings_path: HashMap<PathBuf, HashMap<String, bool>> = HashMap::default();
    for (plugin_id, installs) in installed.plugins {
        let (name, marketplace) = split_plugin_id(&plugin_id);
        for install in installs {
            let settings_path = claude_settings_path(
                home,
                install.scope.as_deref(),
                install.project_path.as_deref(),
            );
            let enabled = match settings_path {
                Some(settings_path) => {
                    if !enabled_by_settings_path.contains_key(&settings_path) {
                        let settings = load_json::<ClaudeSettings>(fs, &settings_path)
                            .await
                            .unwrap_or_default();
                        enabled_by_settings_path
                            .insert(settings_path.clone(), settings.enabled_plugins);
                    }
                    enabled_by_settings_path
                        .get(&settings_path)
                        .and_then(|enabled_plugins| enabled_plugins.get(&plugin_id))
                        .copied()
                }
                None => None,
            };

            let manifest_path = install.install_path.join(".claude-plugin/plugin.json");
            let manifest = load_json::<PluginManifest>(fs, &manifest_path).await;
            let path_to_open =
                first_existing_file(fs, [manifest_path, install.install_path.join("README.md")])
                    .await;
            let version = install.version.or_else(|| {
                manifest
                    .as_ref()
                    .and_then(|manifest| manifest.version.clone())
            });
            plugins.push(PluginEntry {
                host: PluginHost::Claude,
                name: name.to_string().into(),
                marketplace: marketplace.to_string().into(),
                version: version.unwrap_or_default().into(),
                description: manifest
                    .and_then(|manifest| manifest.description)
                    .unwrap_or_default()
                    .into(),
                scope: claude_scope_label(
                    install.scope.as_deref(),
                    install.project_path.as_deref(),
                ),
                enabled,
                path_to_open,
            });
        }
    }
}

/// Codex keeps each installed plugin at
/// `plugins/cache/{marketplace}/{plugin}/{version}`. Its configuration does
/// not record whether a plugin is enabled, so no status is shown.
async fn load_codex_plugins(fs: &dyn Fs, codex_home: &Path, plugins: &mut Vec<PluginEntry>) {
    let cache_root = codex_home.join("plugins/cache");
    for marketplace_dir in read_dir_paths(fs, &cache_root).await {
        let marketplace = file_name_string(&marketplace_dir);
        for plugin_dir in read_dir_paths(fs, &marketplace_dir).await {
            let directory_name = file_name_string(&plugin_dir);
            for version_dir in read_dir_paths(fs, &plugin_dir).await {
                let manifest_path = version_dir.join(".codex-plugin/plugin.json");
                let Some(manifest) = load_json::<PluginManifest>(fs, &manifest_path).await else {
                    continue;
                };
                plugins.push(PluginEntry {
                    host: PluginHost::Codex,
                    name: manifest
                        .name
                        .unwrap_or_else(|| directory_name.clone())
                        .into(),
                    marketplace: marketplace.clone().into(),
                    version: manifest
                        .version
                        .unwrap_or_else(|| file_name_string(&version_dir))
                        .into(),
                    description: manifest.description.unwrap_or_default().into(),
                    scope: None,
                    enabled: None,
                    path_to_open: Some(manifest_path),
                });
            }
        }
    }
}

async fn first_existing_file(fs: &dyn Fs, candidates: [PathBuf; 2]) -> Option<PathBuf> {
    for candidate in candidates {
        if fs.is_file(&candidate).await {
            return Some(candidate);
        }
    }
    None
}

/// Claude Code identifies plugins as `{name}@{marketplace}`.
fn split_plugin_id(plugin_id: &str) -> (&str, &str) {
    plugin_id.split_once('@').unwrap_or((plugin_id, ""))
}

/// The settings file that holds the `enabledPlugins` entry for an install.
fn claude_settings_path(
    home: &Path,
    scope: Option<&str>,
    project_path: Option<&Path>,
) -> Option<PathBuf> {
    match scope? {
        "user" => Some(home.join(".claude/settings.json")),
        "project" => Some(project_path?.join(".claude/settings.json")),
        "local" => Some(project_path?.join(".claude/settings.local.json")),
        _ => None,
    }
}

fn claude_scope_label(scope: Option<&str>, project_path: Option<&Path>) -> Option<SharedString> {
    let scope = scope?;
    let project_name = project_path
        .map(file_name_string)
        .filter(|name| !name.is_empty());
    Some(match project_name {
        Some(project_name) if scope != "user" => format!("{scope} · {project_name}").into(),
        _ => scope.to_string().into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_parse_claude_installed_plugins() -> Result<()> {
        let installed: ClaudeInstalledPlugins = serde_json::from_str(
            r#"{
                "version": 2,
                "plugins": {
                    "context7@claude-plugins-official": [{
                        "scope": "local",
                        "projectPath": "/work/app",
                        "installPath": "/cache/context7",
                        "version": "1.0.0",
                        "installedAt": "2026-10-01T00:28:50.722Z"
                    }]
                }
            }"#,
        )?;
        let installs = installed
            .plugins
            .get("context7@claude-plugins-official")
            .map(Vec::as_slice)
            .unwrap_or_default();
        assert_eq!(installs.len(), 1);
        assert_eq!(installs[0].scope.as_deref(), Some("local"));
        assert_eq!(installs[0].project_path, Some(PathBuf::from("/work/app")));
        assert_eq!(installs[0].install_path, PathBuf::from("/cache/context7"));
        assert_eq!(installs[0].version.as_deref(), Some("1.0.0"));
        Ok(())
    }

    #[test]
    fn test_parse_claude_settings_without_enabled_plugins() -> Result<()> {
        let settings: ClaudeSettings = serde_json::from_str(r#"{"model": "opus"}"#)?;
        assert!(settings.enabled_plugins.is_empty());
        let settings: ClaudeSettings =
            serde_json::from_str(r#"{"enabledPlugins": {"a@b": true, "c@d": false}}"#)?;
        assert_eq!(settings.enabled_plugins.get("a@b"), Some(&true));
        assert_eq!(settings.enabled_plugins.get("c@d"), Some(&false));
        Ok(())
    }

    #[test]
    fn test_split_plugin_id() {
        assert_eq!(
            split_plugin_id("context7@official"),
            ("context7", "official")
        );
        assert_eq!(split_plugin_id("standalone"), ("standalone", ""));
    }

    #[test]
    fn test_claude_settings_path() {
        let home = Path::new("/home/me");
        let project = Path::new("/work/app");
        assert_eq!(
            claude_settings_path(home, Some("user"), Some(project)),
            Some(PathBuf::from("/home/me/.claude/settings.json"))
        );
        assert_eq!(
            claude_settings_path(home, Some("project"), Some(project)),
            Some(PathBuf::from("/work/app/.claude/settings.json"))
        );
        assert_eq!(
            claude_settings_path(home, Some("local"), Some(project)),
            Some(PathBuf::from("/work/app/.claude/settings.local.json"))
        );
        assert_eq!(claude_settings_path(home, Some("local"), None), None);
        assert_eq!(
            claude_settings_path(home, Some("managed"), Some(project)),
            None
        );
        assert_eq!(claude_settings_path(home, None, Some(project)), None);
    }

    #[test]
    fn test_claude_scope_label() {
        assert_eq!(
            claude_scope_label(Some("user"), Some(Path::new("/work/app"))),
            Some("user".into())
        );
        assert_eq!(
            claude_scope_label(Some("local"), Some(Path::new("/work/app"))),
            Some("local · app".into())
        );
        assert_eq!(
            claude_scope_label(Some("local"), None),
            Some("local".into())
        );
        assert_eq!(claude_scope_label(None, None), None);
    }
}
