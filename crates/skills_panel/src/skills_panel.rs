use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::Result;
use collections::HashSet;
use fs::Fs;
use futures::StreamExt as _;
use gpui::{
    Action, AsyncWindowContext, Entity, EventEmitter, FocusHandle, Focusable, Task, WeakEntity,
    actions,
};
use project::Project;
use serde::Deserialize;
use ui::{ListItem, ListItemSpacing, ListSubHeader, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    OpenOptions, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

actions!(
    skills_panel,
    [
        /// Toggles focus on the skills panel.
        ToggleFocus,
    ]
);

const REFRESH_INTERVAL: Duration = Duration::from_secs(3);
const MAX_DISPATCHES: usize = 50;
const MAX_SUMMARY_CHARS: usize = 200;
/// Written by agent-toolkit's `agent-dispatch.sh` under each repository root.
const DISPATCH_LOGS_DIR: &str = ".agent-dispatch/logs";
const DISPATCH_LAST_MESSAGE_SUFFIX: &str = ".last-message.txt";

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<SkillsPanel>(window, cx);
        });
    })
    .detach();
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DispatchStatus {
    Running,
    Finished,
    /// The worker exited without writing a final message, or this platform
    /// cannot tell whether it is still running.
    NoResult,
}

#[derive(Clone, Debug, PartialEq)]
struct DispatchEntry {
    engine: SharedString,
    role_and_branch: SharedString,
    started_at: SharedString,
    status: DispatchStatus,
    summary: Option<SharedString>,
    log_path: PathBuf,
    last_message_path: Option<PathBuf>,
}

#[derive(Default)]
struct Snapshot {
    skills: Vec<SkillEntry>,
    dispatches: Vec<DispatchEntry>,
}

pub struct SkillsPanel {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    fs: Arc<dyn Fs>,
    focus_handle: FocusHandle,
    position: DockPosition,
    active: bool,
    snapshot: Snapshot,
    refresh_task: Task<()>,
    _poll_task: Task<()>,
}

impl SkillsPanel {
    pub async fn load(
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
        let poll_task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REFRESH_INTERVAL).await;
                let still_alive = this.update(cx, |panel, cx| {
                    if panel.active {
                        panel.refresh(cx);
                    }
                });
                if still_alive.is_err() {
                    break;
                }
            }
        });

        Self {
            workspace,
            project,
            fs,
            focus_handle: cx.focus_handle(),
            position: DockPosition::Left,
            active: false,
            snapshot: Snapshot::default(),
            refresh_task: Task::ready(()),
            _poll_task: poll_task,
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let fs = self.fs.clone();
        // Scanning goes through the local filesystem, which does not contain
        // a remote project's worktrees.
        let project_roots = if self.project.read(cx).is_local() {
            self.project
                .read(cx)
                .visible_worktrees(cx)
                .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        let scan = cx.background_spawn(async move { scan(fs.as_ref(), &project_roots).await });
        self.refresh_task = cx.spawn(async move |this, cx| {
            let snapshot = scan.await;
            this.update(cx, |panel, cx| {
                if panel.snapshot.skills != snapshot.skills
                    || panel.snapshot.dispatches != snapshot.dispatches
                {
                    panel.snapshot = snapshot;
                    cx.notify();
                }
            })
            .log_err();
        });
    }

    fn open_path(&self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace
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

    fn render_dispatch(
        &self,
        index: usize,
        dispatch: &DispatchEntry,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (status_icon, status_color, status_label) = match dispatch.status {
            DispatchStatus::Running => (IconName::LoadCircle, Color::Accent, "running"),
            DispatchStatus::Finished => (IconName::Check, Color::Success, "finished"),
            DispatchStatus::NoResult => (IconName::Warning, Color::Warning, "no result"),
        };
        let path_to_open = dispatch
            .last_message_path
            .clone()
            .unwrap_or_else(|| dispatch.log_path.clone());
        let tooltip_text: SharedString = dispatch
            .summary
            .clone()
            .unwrap_or_else(|| dispatch.log_path.to_string_lossy().into_owned().into());

        ListItem::new(("dispatch", index))
            .spacing(ListItemSpacing::Sparse)
            .start_slot(
                Icon::new(status_icon)
                    .size(IconSize::Small)
                    .color(status_color),
            )
            .child(
                v_flex()
                    .min_w_0()
                    .child(Label::new(dispatch.role_and_branch.clone()).truncate())
                    .child(
                        Label::new(format!(
                            "{} · {} · {}",
                            dispatch.engine, status_label, dispatch.started_at
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .truncate(),
                    ),
            )
            .tooltip(Tooltip::text(tooltip_text))
            .on_click(cx.listener(move |panel, _, window, cx| {
                panel.open_path(path_to_open.clone(), window, cx);
            }))
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
                panel.open_path(path_to_open.clone(), window, cx);
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
            .overflow_y_scroll()
            .child(ListSubHeader::new("Dispatched Agents").left_icon(Some(IconName::ToolHammer)));

        if self.snapshot.dispatches.is_empty() {
            content = content.child(
                div().px_2().pb_2().child(
                    Label::new(format!("No dispatches in {DISPATCH_LOGS_DIR}"))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            );
        }
        for (index, dispatch) in self.snapshot.dispatches.iter().enumerate() {
            content = content.child(self.render_dispatch(index, dispatch, cx));
        }

        let mut current_origin = None;
        for (index, skill) in self.snapshot.skills.iter().enumerate() {
            if current_origin != Some(skill.origin) {
                current_origin = Some(skill.origin);
                content = content.child(
                    ListSubHeader::new(skill.origin.label()).left_icon(Some(skill.origin.icon())),
                );
            }
            content = content.child(self.render_skill(index, skill, cx));
        }
        if self.snapshot.skills.is_empty() {
            content = content.child(ListSubHeader::new("Skills")).child(
                div().px_2().child(
                    Label::new("No Claude Code skills or Codex prompts found")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            );
        }

        content
    }
}

impl Focusable for SkillsPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for SkillsPanel {}

impl Panel for SkillsPanel {
    fn persistent_name() -> &'static str {
        "Skills Panel"
    }

    fn panel_key() -> &'static str {
        "SkillsPanel"
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, position: DockPosition, _window: &mut Window, cx: &mut Context<Self>) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(300.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::Sparkle)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Skills Panel")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn set_active(&mut self, active: bool, _window: &mut Window, cx: &mut Context<Self>) {
        self.active = active;
        if active {
            self.refresh(cx);
        }
    }

    fn activation_priority(&self) -> u32 {
        8
    }
}

async fn scan(fs: &dyn Fs, project_roots: &[PathBuf]) -> Snapshot {
    let home = util::paths::home_dir();
    let codex_home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".codex"));

    let mut skills = Vec::new();
    for root in project_roots {
        load_skills_root(fs, &root.join(".claude/skills"), SkillOrigin::ClaudeProject, &mut skills)
            .await;
    }
    let claude_skills_root = home.join(".claude/skills");
    load_skills_root(fs, &claude_skills_root, SkillOrigin::ClaudeGlobal, &mut skills).await;
    // Skills synced from claude.ai sit one level deeper, grouped per account.
    for synced_root in read_dir_paths(fs, &claude_skills_root.join("synced")).await {
        load_skills_root(fs, &synced_root, SkillOrigin::ClaudeGlobal, &mut skills).await;
    }
    load_skills_root(fs, &codex_home.join("skills"), SkillOrigin::CodexSkill, &mut skills).await;
    load_codex_prompts(fs, &codex_home.join("prompts"), &mut skills).await;
    skills.sort_by(|a, b| (a.origin, &a.name).cmp(&(b.origin, &b.name)));

    let mut dispatches = Vec::new();
    for root in project_roots {
        load_dispatches(fs, &root.join(DISPATCH_LOGS_DIR), &mut dispatches).await;
    }
    dispatches.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    dispatches.truncate(MAX_DISPATCHES);

    if dispatches
        .iter()
        .any(|dispatch| dispatch.status == DispatchStatus::NoResult)
    {
        let open_files = files_open_by_any_process();
        for dispatch in &mut dispatches {
            if open_files.contains(&dispatch.log_path) {
                dispatch.status = DispatchStatus::Running;
            }
        }
    }

    Snapshot { skills, dispatches }
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
        let fallback_name = directory
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
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
        let Some(name) = path.file_stem().map(|stem| stem.to_string_lossy().into_owned()) else {
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

async fn load_dispatches(fs: &dyn Fs, logs_dir: &Path, dispatches: &mut Vec<DispatchEntry>) {
    for log_path in read_dir_paths(fs, logs_dir).await {
        if log_path.extension().is_none_or(|extension| extension != "log") {
            continue;
        }
        let Some(stem) = log_path.file_stem().map(|stem| stem.to_string_lossy().into_owned())
        else {
            continue;
        };
        let Some(parsed) = parse_dispatch_log_stem(&stem) else {
            continue;
        };
        let last_message_path = logs_dir.join(format!("{stem}{DISPATCH_LAST_MESSAGE_SUFFIX}"));
        let (status, summary, last_message_path) = if fs.is_file(&last_message_path).await {
            let summary = fs
                .load(&last_message_path)
                .await
                .log_err()
                .and_then(|content| summarize(&content));
            (DispatchStatus::Finished, summary, Some(last_message_path))
        } else {
            (DispatchStatus::NoResult, None, None)
        };
        dispatches.push(DispatchEntry {
            engine: parsed.engine.into(),
            role_and_branch: parsed.role_and_branch.into(),
            started_at: parsed.started_at.into(),
            status,
            summary: summary.map(Into::into),
            log_path,
            last_message_path,
        });
    }
}

#[derive(Debug, PartialEq)]
struct ParsedDispatchLog {
    engine: String,
    role_and_branch: String,
    started_at: String,
}

/// agent-dispatch.sh names logs `{engine}-{role}-{branch}-{stamp}`. Roles and
/// branches may both contain `-`, so they are kept together.
fn parse_dispatch_log_stem(stem: &str) -> Option<ParsedDispatchLog> {
    let (engine, rest) = stem.split_once('-')?;
    let (role_and_branch, stamp) = rest.rsplit_once('-')?;
    if engine.is_empty() || role_and_branch.is_empty() {
        return None;
    }
    Some(ParsedDispatchLog {
        engine: engine.to_string(),
        role_and_branch: role_and_branch.to_string(),
        started_at: format_stamp(stamp),
    })
}

/// Formats `20260918T120707Z` as `2026-09-18 12:07 UTC`, which also keeps
/// the newest-first sort working on the formatted value.
fn format_stamp(stamp: &str) -> String {
    match (
        stamp.get(0..4),
        stamp.get(4..6),
        stamp.get(6..8),
        stamp.get(9..11),
        stamp.get(11..13),
    ) {
        (Some(year), Some(month), Some(day), Some(hour), Some(minute)) => {
            format!("{year}-{month}-{day} {hour}:{minute} UTC")
        }
        _ => stamp.to_string(),
    }
}

fn summarize(content: &str) -> Option<String> {
    let first_line = content.lines().map(str::trim).find(|line| !line.is_empty())?;
    let mut summary = first_line.chars().take(MAX_SUMMARY_CHARS).collect::<String>();
    if first_line.chars().count() > MAX_SUMMARY_CHARS {
        summary.push('…');
    }
    Some(summary)
}

/// A dispatched worker writes its log through an open file descriptor until it
/// exits, so an open descriptor is how a quiet-but-alive worker is told apart
/// from one that died without a final message.
#[cfg(target_os = "linux")]
fn files_open_by_any_process() -> HashSet<PathBuf> {
    let mut open_files = HashSet::default();
    let Some(processes) = std::fs::read_dir("/proc").log_err() else {
        return open_files;
    };
    for process in processes.flatten() {
        let is_pid = process
            .file_name()
            .to_str()
            .is_some_and(|name| name.bytes().all(|byte| byte.is_ascii_digit()));
        if !is_pid {
            continue;
        }
        // Other users' processes and ones that exit mid-scan are unreadable;
        // that is expected and not worth logging.
        let Ok(descriptors) = std::fs::read_dir(process.path().join("fd")) else {
            continue;
        };
        for descriptor in descriptors.flatten() {
            if let Ok(target) = std::fs::read_link(descriptor.path()) {
                open_files.insert(target);
            }
        }
    }
    open_files
}

#[cfg(not(target_os = "linux"))]
fn files_open_by_any_process() -> HashSet<PathBuf> {
    HashSet::default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_parse_dispatch_log_stem() {
        assert_eq!(
            parse_dispatch_log_stem("codex-bug-verify-agent-fix-login-20260918T120707Z"),
            Some(ParsedDispatchLog {
                engine: "codex".to_string(),
                role_and_branch: "bug-verify-agent-fix-login".to_string(),
                started_at: "2026-09-18 12:07 UTC".to_string(),
            })
        );
        assert_eq!(parse_dispatch_log_stem("codex"), None);
        assert_eq!(parse_dispatch_log_stem("codex--20260918T120707Z"), None);
    }

    #[test]
    fn test_format_stamp_keeps_unrecognized_input() {
        assert_eq!(format_stamp("short"), "short");
    }

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

    #[test]
    fn test_summarize_truncates_on_char_boundary() {
        let long_line = "é".repeat(MAX_SUMMARY_CHARS + 5);
        let summary = summarize(&format!("\n{long_line}\nsecond line")).unwrap_or_default();
        assert_eq!(summary.chars().count(), MAX_SUMMARY_CHARS + 1);
        assert!(summary.ends_with('…'));
    }
}
