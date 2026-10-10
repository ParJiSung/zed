use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Result;
use collections::HashSet;
use fs::Fs;
use gpui::{AsyncWindowContext, Entity, FocusHandle, Task, WeakEntity};
use project::Project;
use ui::{ListItem, ListItemSpacing, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{Workspace, dock::DockPosition};

use crate::{
    ToggleAgentsFocus, empty_message, local_project_roots, open_path, poll_task, read_dir_paths,
    section_header,
};

const MAX_DISPATCHES: usize = 50;
const MAX_SUMMARY_CHARS: usize = 200;
/// Written by agent-toolkit's `agent-dispatch.sh` under each repository root.
const DISPATCH_LOGS_DIR: &str = ".agent-dispatch/logs";
const DISPATCH_LAST_MESSAGE_SUFFIX: &str = ".last-message.txt";

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

pub struct AgentsPanel {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    fs: Arc<dyn Fs>,
    focus_handle: FocusHandle,
    position: DockPosition,
    active: bool,
    dispatches: Vec<DispatchEntry>,
    refresh_task: Task<()>,
    _poll_task: Task<()>,
}

impl AgentsPanel {
    pub(crate) async fn load(
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
            position: DockPosition::Right,
            active: false,
            dispatches: Vec::new(),
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
            cx.background_spawn(async move { scan_dispatches(fs.as_ref(), &project_roots).await });
        self.refresh_task = cx.spawn(async move |this, cx| {
            let dispatches = scan.await;
            this.update(cx, |panel, cx| {
                if panel.dispatches != dispatches {
                    panel.dispatches = dispatches;
                    cx.notify();
                }
            })
            .log_err();
        });
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
                open_path(&panel.workspace, path_to_open.clone(), window, cx);
            }))
    }
}

impl Render for AgentsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut content = v_flex()
            .id("agents-panel")
            .key_context("AgentsPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_y_scroll()
            .child(section_header("Dispatched Agents", IconName::ToolHammer));

        if self.dispatches.is_empty() {
            content = content.child(empty_message(format!(
                "No dispatches in {DISPATCH_LOGS_DIR}"
            )));
        }
        for (index, dispatch) in self.dispatches.iter().enumerate() {
            content = content.child(self.render_dispatch(index, dispatch, cx));
        }
        content
    }
}

impl_dock_panel!(
    AgentsPanel,
    name: "Agents Panel",
    key: "AgentsPanel",
    tooltip: "Spina di Rosula · Agents",
    icon: IconName::ToolHammer,
    toggle: ToggleAgentsFocus,
    priority: 9
);

async fn scan_dispatches(fs: &dyn Fs, project_roots: &[PathBuf]) -> Vec<DispatchEntry> {
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
            if dispatch.status == DispatchStatus::NoResult
                && is_still_being_written(fs, &open_files, &dispatch.log_path).await
            {
                dispatch.status = DispatchStatus::Running;
            }
        }
    }
    dispatches
}

async fn load_dispatches(fs: &dyn Fs, logs_dir: &Path, dispatches: &mut Vec<DispatchEntry>) {
    for log_path in read_dir_paths(fs, logs_dir).await {
        if log_path
            .extension()
            .is_none_or(|extension| extension != "log")
        {
            continue;
        }
        let Some(stem) = log_path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
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
    let first_line = content
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let mut summary = first_line
        .chars()
        .take(MAX_SUMMARY_CHARS)
        .collect::<String>();
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

/// How recently a log must have been written to count as a live worker on
/// platforms where open file descriptors cannot be listed.
#[cfg(not(target_os = "linux"))]
const RECENT_WRITE_WINDOW: std::time::Duration = std::time::Duration::from_secs(120);

#[cfg(target_os = "linux")]
async fn is_still_being_written(
    _fs: &dyn Fs,
    open_files: &HashSet<PathBuf>,
    log_path: &Path,
) -> bool {
    open_files.contains(log_path)
}

/// Without a portable way to see which files a process holds open, a log that
/// is still growing is the best sign that its worker is alive.
#[cfg(not(target_os = "linux"))]
async fn is_still_being_written(
    fs: &dyn Fs,
    _open_files: &HashSet<PathBuf>,
    log_path: &Path,
) -> bool {
    let Some(metadata) = fs.metadata(log_path).await.log_err().flatten() else {
        return false;
    };
    metadata
        .mtime
        .timestamp_for_user()
        .elapsed()
        .is_ok_and(|elapsed| elapsed < RECENT_WRITE_WINDOW)
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
    fn test_summarize_truncates_on_char_boundary() {
        let long_line = "é".repeat(MAX_SUMMARY_CHARS + 5);
        let summary = summarize(&format!("\n{long_line}\nsecond line")).unwrap_or_default();
        assert_eq!(summary.chars().count(), MAX_SUMMARY_CHARS + 1);
        assert!(summary.ends_with('…'));
    }
}
