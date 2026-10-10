/// Implements the dock plumbing shared by every panel in this crate. The panel
/// type must have `focus_handle`, `position` and `active` fields and a
/// `refresh` method.
macro_rules! impl_dock_panel {
    (
        $panel:ty,
        name: $name:literal,
        key: $key:literal,
        tooltip: $tooltip:literal,
        icon: $icon:expr,
        toggle: $toggle:expr,
        priority: $priority:literal
    ) => {
        impl gpui::Focusable for $panel {
            fn focus_handle(&self, _cx: &gpui::App) -> gpui::FocusHandle {
                self.focus_handle.clone()
            }
        }

        impl gpui::EventEmitter<workspace::dock::PanelEvent> for $panel {}

        impl workspace::dock::Panel for $panel {
            fn persistent_name() -> &'static str {
                $name
            }

            fn panel_key() -> &'static str {
                $key
            }

            fn position(
                &self,
                _window: &gpui::Window,
                _cx: &gpui::App,
            ) -> workspace::dock::DockPosition {
                self.position
            }

            fn position_is_valid(&self, position: workspace::dock::DockPosition) -> bool {
                matches!(
                    position,
                    workspace::dock::DockPosition::Left | workspace::dock::DockPosition::Right
                )
            }

            fn set_position(
                &mut self,
                position: workspace::dock::DockPosition,
                _window: &mut gpui::Window,
                cx: &mut gpui::Context<Self>,
            ) {
                self.position = position;
                cx.notify();
            }

            fn default_size(&self, _window: &gpui::Window, _cx: &gpui::App) -> gpui::Pixels {
                gpui::px(300.)
            }

            fn icon(&self, _window: &gpui::Window, _cx: &gpui::App) -> Option<ui::IconName> {
                Some($icon)
            }

            fn icon_tooltip(
                &self,
                _window: &gpui::Window,
                _cx: &gpui::App,
            ) -> Option<&'static str> {
                Some($tooltip)
            }

            fn toggle_action(&self) -> Box<dyn gpui::Action> {
                Box::new($toggle)
            }

            fn set_active(
                &mut self,
                active: bool,
                _window: &mut gpui::Window,
                cx: &mut gpui::Context<Self>,
            ) {
                self.active = active;
                if active {
                    self.refresh(cx);
                }
            }

            fn activation_priority(&self) -> u32 {
                $priority
            }
        }
    };
}

mod agents_panel;
mod install_plugin;
mod plugins_panel;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context as _, Result};
use fs::Fs;
use futures::StreamExt as _;
use gpui::{AsyncWindowContext, Entity, FocusHandle, Task, WeakEntity, actions};
use project::Project;
use serde::{Deserialize, de::DeserializeOwned};
use ui::{ListItem, ListItemSpacing, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{OpenOptions, Workspace, dock::DockPosition};

use agents_panel::AgentsPanel;
use install_plugin::InstallPluginModal;
use plugins_panel::PluginsPanel;

actions!(
    skills_panel,
    [
        /// Toggles focus on the skills panel.
        ToggleFocus,
        /// Toggles focus on the agents panel.
        ToggleAgentsFocus,
        /// Toggles focus on the plugins panel.
        TogglePluginsFocus,
        /// Opens the dialog for installing a plugin from a marketplace or a GitHub repository.
        InstallPlugin,
    ]
);

const REFRESH_INTERVAL: Duration = Duration::from_secs(3);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<SkillsPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &ToggleAgentsFocus, window, cx| {
            workspace.toggle_panel_focus::<AgentsPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &TogglePluginsFocus, window, cx| {
            workspace.toggle_panel_focus::<PluginsPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &InstallPlugin, window, cx| {
            InstallPluginModal::toggle(workspace, window, cx);
        });
    })
    .detach();
}

/// Loads the skills, agents and plugins panels and adds them to the workspace.
pub async fn add_panels(workspace: WeakEntity<Workspace>, mut cx: AsyncWindowContext) {
    let (skills_panel, agents_panel, plugins_panel) = futures::join!(
        SkillsPanel::load(workspace.clone(), cx.clone()),
        AgentsPanel::load(workspace.clone(), cx.clone()),
        PluginsPanel::load(workspace.clone(), cx.clone()),
    );
    workspace
        .update_in(&mut cx, |workspace, window, cx| {
            if let Some(panel) = skills_panel
                .context("failed to load skills panel")
                .log_err()
            {
                workspace.add_panel(panel, window, cx);
            }
            if let Some(panel) = agents_panel
                .context("failed to load agents panel")
                .log_err()
            {
                workspace.add_panel(panel, window, cx);
            }
            if let Some(panel) = plugins_panel
                .context("failed to load plugins panel")
                .log_err()
            {
                workspace.add_panel(panel, window, cx);
            }
        })
        .log_err();
}

fn poll_task<T: 'static>(cx: &mut Context<T>, refresh: fn(&mut T, &mut Context<T>)) -> Task<()> {
    cx.spawn(async move |this, cx| {
        loop {
            cx.background_executor().timer(REFRESH_INTERVAL).await;
            if this.update(cx, |panel, cx| refresh(panel, cx)).is_err() {
                break;
            }
        }
    })
}

fn local_project_roots(project: &Entity<Project>, cx: &App) -> Vec<PathBuf> {
    let project = project.read(cx);
    // Scanning goes through the local filesystem, which does not contain
    // a remote project's worktrees.
    if !project.is_local() {
        return Vec::new();
    }
    project
        .visible_worktrees(cx)
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
        .collect()
}

fn codex_home() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| util::paths::home_dir().join(".codex"))
}

fn open_path(workspace: &WeakEntity<Workspace>, path: PathBuf, window: &mut Window, cx: &mut App) {
    workspace
        .update(cx, |workspace, cx| {
            workspace
                .open_abs_path(
                    path,
                    OpenOptions {
                        focus: Some(true),
                        ..Default::default()
                    },
                    window,
                    cx,
                )
                .detach_and_log_err(cx);
        })
        .log_err();
}

// Zed's `ListSubHeader` sets `flex_1`, so inside a full-height column it
// grows to absorb any space the content leaves empty.
fn section_header(label: impl Into<SharedString>, icon: IconName) -> impl IntoElement {
    h_flex()
        .flex_none()
        .w_full()
        .h_7()
        .px_2()
        .gap_1()
        .items_center()
        .child(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
        .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
}

fn empty_message(text: impl Into<SharedString>) -> impl IntoElement {
    div()
        .px_2()
        .pb_2()
        .child(Label::new(text).size(LabelSize::Small).color(Color::Muted))
}

fn file_name_string(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

async fn read_dir_paths(fs: &dyn Fs, directory: &Path) -> Vec<PathBuf> {
    if !fs.is_dir(directory).await {
        return Vec::new();
    }
    let Some(mut entries) = fs.read_dir(directory).await.log_err() else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    while let Some(entry) = entries.next().await {
        if let Some(path) = entry.log_err() {
            paths.push(path);
        }
    }
    paths.sort();
    paths
}

/// A missing file is not an error; an unreadable or malformed one is logged.
async fn load_json<T: DeserializeOwned>(fs: &dyn Fs, path: &Path) -> Option<T> {
    if !fs.is_file(path).await {
        return None;
    }
    let content = fs.load(path).await.log_err()?;
    serde_json::from_str(&content)
        .with_context(|| format!("parsing {}", path.display()))
        .log_err()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum SkillOrigin {
    ClaudeProject,
    ClaudeGlobal,
    CodexSkill,
    CodexPrompt,
}

impl SkillOrigin {
    fn label(self) -> &'static str {
        match self {
            SkillOrigin::ClaudeProject => "Claude Code · Project",
            SkillOrigin::ClaudeGlobal => "Claude Code · Global",
            SkillOrigin::CodexSkill => "Codex · Skills",
            SkillOrigin::CodexPrompt => "Codex · Prompts",
        }
    }

    fn icon(self) -> IconName {
        match self {
            SkillOrigin::ClaudeProject | SkillOrigin::ClaudeGlobal => IconName::AiClaude,
            SkillOrigin::CodexSkill | SkillOrigin::CodexPrompt => IconName::AiOpenAi,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct SkillEntry {
    name: SharedString,
    description: SharedString,
    origin: SkillOrigin,
    path: PathBuf,
    is_invalid: bool,
}

pub struct SkillsPanel {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    fs: Arc<dyn Fs>,
    focus_handle: FocusHandle,
    position: DockPosition,
    active: bool,
    skills: Vec<SkillEntry>,
    refresh_task: Task<()>,
    _poll_task: Task<()>,
}

impl SkillsPanel {
    async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, _window, cx| {
            let fs = workspace.app_state().fs.clone();
            let project = workspace.project().clone();
            let weak_workspace = workspace.weak_handle();
            cx.new(|cx| Self::new(weak_workspace, project, fs, cx))
        })
    }

    fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        fs: Arc<dyn Fs>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            workspace,
            project,
            fs,
            focus_handle: cx.focus_handle(),
            position: DockPosition::Left,
            active: false,
            skills: Vec::new(),
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
        let project_roots = local_project_roots(&self.project, cx);
        let scan =
            cx.background_spawn(async move { scan_skills(fs.as_ref(), &project_roots).await });
        self.refresh_task = cx.spawn(async move |this, cx| {
            let skills = scan.await;
            this.update(cx, |panel, cx| {
                if panel.skills != skills {
                    panel.skills = skills;
                    cx.notify();
                }
            })
            .log_err();
        });
    }

    fn render_skill(
        &self,
        index: usize,
        skill: &SkillEntry,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let path_to_open = skill.path.clone();
        let description_color = if skill.is_invalid {
            Color::Error
        } else {
            Color::Muted
        };

        ListItem::new(("skill", index))
            .spacing(ListItemSpacing::Sparse)
            .child(
                v_flex()
                    .min_w_0()
                    .child(Label::new(skill.name.clone()).truncate())
                    .child(
                        Label::new(skill.description.clone())
                            .size(LabelSize::Small)
                            .color(description_color)
                            .truncate(),
                    ),
            )
            .tooltip(Tooltip::text(skill.description.clone()))
            .on_click(cx.listener(move |panel, _, window, cx| {
                open_path(&panel.workspace, path_to_open.clone(), window, cx);
            }))
    }
}

impl Render for SkillsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut content = v_flex()
            .id("skills-panel")
            .key_context("SkillsPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_y_scroll();

        if self.skills.is_empty() {
            return content
                .child(section_header("Skills", IconName::Sparkle))
                .child(empty_message(
                    "No Claude Code skills or Codex prompts found",
                ));
        }

        let mut current_origin = None;
        for (index, skill) in self.skills.iter().enumerate() {
            if current_origin != Some(skill.origin) {
                current_origin = Some(skill.origin);
                content = content.child(section_header(skill.origin.label(), skill.origin.icon()));
            }
            content = content.child(self.render_skill(index, skill, cx));
        }
        content
    }
}

impl_dock_panel!(
    SkillsPanel,
    name: "Skills Panel",
    key: "SkillsPanel",
    tooltip: "Skills Panel",
    icon: IconName::Sparkle,
    toggle: ToggleFocus,
    priority: 8
);

async fn scan_skills(fs: &dyn Fs, project_roots: &[PathBuf]) -> Vec<SkillEntry> {
    let home = util::paths::home_dir();
    let codex_home = codex_home();

    let mut skills = Vec::new();
    for root in project_roots {
        load_skills_root(
            fs,
            &root.join(".claude/skills"),
            SkillOrigin::ClaudeProject,
            &mut skills,
        )
        .await;
    }
    let claude_skills_root = home.join(".claude/skills");
    load_skills_root(
        fs,
        &claude_skills_root,
        SkillOrigin::ClaudeGlobal,
        &mut skills,
    )
    .await;
    // Skills synced from claude.ai sit one level deeper, grouped per account.
    for synced_root in read_dir_paths(fs, &claude_skills_root.join("synced")).await {
        load_skills_root(fs, &synced_root, SkillOrigin::ClaudeGlobal, &mut skills).await;
    }
    load_skills_root(
        fs,
        &codex_home.join("skills"),
        SkillOrigin::CodexSkill,
        &mut skills,
    )
    .await;
    load_codex_prompts(fs, &codex_home.join("prompts"), &mut skills).await;
    skills.sort_by(|a, b| (a.origin, &a.name).cmp(&(b.origin, &b.name)));
    skills
}

async fn load_skills_root(
    fs: &dyn Fs,
    root: &Path,
    origin: SkillOrigin,
    skills: &mut Vec<SkillEntry>,
) {
    for directory in read_dir_paths(fs, root).await {
        let skill_file = directory.join(agent_skills::SKILL_FILE_NAME);
        if !fs.is_file(&skill_file).await {
            continue;
        }
        let fallback_name = file_name_string(&directory);
        let parsed = fs.load(&skill_file).await.and_then(|content| {
            agent_skills::extract_skill_frontmatter(&content).map(|(metadata, _body)| metadata)
        });
        skills.push(match parsed {
            Ok(metadata) => SkillEntry {
                name: metadata.name.into(),
                description: metadata.description.into(),
                origin,
                path: skill_file,
                is_invalid: false,
            },
            Err(error) => SkillEntry {
                name: fallback_name.into(),
                description: format!("Invalid SKILL.md: {error}").into(),
                origin,
                path: skill_file,
                is_invalid: true,
            },
        });
    }
}

async fn load_codex_prompts(fs: &dyn Fs, prompts_dir: &Path, skills: &mut Vec<SkillEntry>) {
    for path in read_dir_paths(fs, prompts_dir).await {
        if path.extension().is_none_or(|extension| extension != "md") {
            continue;
        }
        let Some(name) = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
        else {
            continue;
        };
        let (description, is_invalid) = match fs.load(&path).await {
            Ok(content) => (prompt_description(&content).unwrap_or_default(), false),
            Err(error) => (format!("Unreadable prompt: {error}"), true),
        };
        skills.push(SkillEntry {
            name: format!("/prompts:{name}").into(),
            description: description.into(),
            origin: SkillOrigin::CodexPrompt,
            path,
            is_invalid,
        });
    }
}

#[derive(Deserialize)]
struct PromptFrontmatter {
    description: Option<String>,
}

/// Codex prompts may carry YAML frontmatter with a `description`; prompts
/// without one are described by their first line of text.
fn prompt_description(content: &str) -> Option<String> {
    let mut body = content;
    if let Some(after_opening) = content.strip_prefix("---")
        && let Some((frontmatter, after_closing)) = after_opening.split_once("\n---")
    {
        let description = serde_yaml_ng::from_str::<PromptFrontmatter>(frontmatter)
            .ok()
            .and_then(|frontmatter| frontmatter.description);
        if description.is_some() {
            return description;
        }
        body = after_closing;
    }
    body.lines()
        .map(|line| line.trim().trim_start_matches('#').trim())
        .find(|line| !line.is_empty())
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_prompt_description() {
        assert_eq!(
            prompt_description("---\ndescription: Review a diff\n---\nBody"),
            Some("Review a diff".to_string())
        );
        assert_eq!(
            prompt_description("---\nargument-hint: FILE\n---\n\n# Explain FILE\n"),
            Some("Explain FILE".to_string())
        );
        assert_eq!(
            prompt_description("Summarize the changes"),
            Some("Summarize the changes".to_string())
        );
        assert_eq!(prompt_description("\n\n"), None);
    }
}
