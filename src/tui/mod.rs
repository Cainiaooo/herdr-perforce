//! Terminal pane: workspace File Explorer and Submit review views.

mod actions;
mod content;
mod description_editor;
mod diff;
mod display;
mod explorer;
mod icons;
mod syntax;
mod wrap;

use std::{
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind,
    },
    execute, queue,
    style::{
        Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
    },
    terminal::{
        self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode,
    },
};

use crate::{
    app::{SubmitOutcomeCertainty, SubmitOverlay, SubmitOverlayRequest, SubmitOverlayState},
    domain::{
        Changelist, ChangelistId, ChangelistStatus, ExplorerDecoration, FileAction,
        WorkspaceIdentity,
    },
    p4::{
        DescriptionApplyError, DescriptionApplyIntent, DescriptionApplyResult, DomainMappingError,
        ExplorerError, LoadedDirectory, P4Client, P4Error, P4Query, P4Transport, P4WriteService,
        StdProcessTransport, SubmitError, SubmitIntent, SubmitPreview, SubmitReconciliationResult,
        SubmitResult, WorkspaceCwdError, cwd_is_in_client_view, load_explorer_directory,
        pending_changelists_from_changes, workspace_owning_cwd,
    },
    panel_restore,
    submit_provider::{ExternalLaunchError, SubmitProvider},
};

use self::actions::{
    ExplorerMenuTarget, MenuAction, MenuEntry, copy_to_clipboard, explorer_menu_entries,
    first_action_index, is_same_path, menu_window, relative_path_text, review_menu_entries,
    step_action_index, validate_name,
};
use self::content::{
    ContentPaneClient, apply_own_navigation_share, persist_navigator_share_from_host,
};
use self::description_editor::{DescriptionEditor, DescriptionTextLayout, layout_description_text};
use self::display::{display_width, pad_display, slice_display, splice_display};
use self::explorer::{
    ExplorerAction, ExplorerLoadState, ExplorerModel, connection_message, open_with_default_app,
};

pub use self::content::{
    navigation_resize_args_for_layout, navigation_resize_args_for_share,
    navigation_share_from_layout, restore_content_pane, rightmost_pane_id, run_content_pane,
    viewer_process_is_active,
};

const MAX_VISIBLE_CHANGELISTS: u16 = 4_096;
const EVENT_POLL: Duration = Duration::from_millis(50);

pub fn run_pane(cwd: PathBuf) -> Result<(), String> {
    let service = Arc::new(P4WriteService::new(P4Client::new(
        StdProcessTransport,
        "p4",
        &cwd,
    )));
    let (sender, receiver) = mpsc::channel();
    let provider = Arc::new(SubmitProvider::load_from_environment());
    let mut pane = PaneModel::new_with_provider(cwd, provider);
    request_overview(pane.cwd.clone(), pane.overview_generation, sender.clone());

    let mut terminal = TerminalGuard::enter().map_err(|error| error.to_string())?;
    let (width, height) = terminal::size().map_err(|error| error.to_string())?;
    pane.set_nav_size(width, height);
    let mut rendered = render_frame(&pane, width, height);
    terminal
        .draw(&rendered)
        .map_err(|error| error.to_string())?;
    let mut dirty = false;
    let persist_armed_at = Instant::now() + Duration::from_secs(2);
    let mut persist_layout_after: Option<Instant> = None;
    let share_cwd = pane.cwd.clone();
    thread::spawn(move || {
        for attempt in 0..10 {
            thread::sleep(Duration::from_millis(50 + attempt * 40));
            if apply_own_navigation_share(&share_cwd) {
                break;
            }
        }
    });
    loop {
        while let Ok(message) = receiver.try_recv() {
            let effect = pane.handle_message(message);
            dirty = true;
            apply_message_effect(&mut pane, effect, &sender);
        }

        if dirty {
            let (width, height) = terminal::size().map_err(|error| error.to_string())?;
            pane.set_nav_size(width, height);
            rendered = render_frame(&pane, width, height);
            terminal
                .draw(&rendered)
                .map_err(|error| error.to_string())?;
            dirty = false;
        }

        if persist_layout_after.is_some_and(|deadline| Instant::now() >= deadline)
            && Instant::now() >= persist_armed_at
        {
            persist_layout_after = None;
            persist_navigator_share_from_host(&pane.cwd);
        }

        if !event::poll(EVENT_POLL).map_err(|error| error.to_string())? {
            continue;
        }
        let event = event::read().map_err(|error| error.to_string())?;
        let (should_close, event_changed) = match event {
            Event::Key(key) if key.kind != KeyEventKind::Press => (false, false),
            Event::Key(key) => (pane.handle_key(key, &service, &sender), true),
            Event::Mouse(mouse) => (
                false,
                pane.handle_mouse(mouse, &rendered.hits, &service, &sender),
            ),
            Event::Resize(_, _) => {
                persist_layout_after = Some(Instant::now() + Duration::from_millis(400));
                (false, true)
            }
            Event::Paste(value) => (false, pane.handle_paste(&value)),
            Event::FocusGained | Event::FocusLost => (false, false),
        };
        if should_close {
            if Instant::now() >= persist_armed_at {
                persist_navigator_share_from_host(&pane.cwd);
            }
            break;
        }
        dirty |= event_changed;
    }
    Ok(())
}

struct TerminalGuard {
    stdout: io::Stdout,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste,
            Hide,
            Clear(ClearType::All)
        ) {
            let _ = execute!(
                stdout,
                Show,
                DisableBracketedPaste,
                DisableMouseCapture,
                LeaveAlternateScreen
            );
            let _ = disable_raw_mode();
            return Err(error);
        }
        Ok(Self { stdout })
    }

    fn draw(&mut self, frame: &RenderedFrame) -> io::Result<()> {
        queue!(self.stdout, MoveTo(0, 0), Clear(ClearType::All))?;
        for (row, line) in frame.lines.iter().enumerate() {
            queue!(
                self.stdout,
                MoveTo(0, row as u16),
                ResetColor,
                SetAttribute(Attribute::Reset)
            )?;
            if let Some(style) = frame.row_styles.get(row).copied() {
                if let Some(foreground) = style.foreground {
                    queue!(self.stdout, SetForegroundColor(foreground))?;
                }
                if let Some(background) = style.background {
                    queue!(self.stdout, SetBackgroundColor(background))?;
                }
                if style.bold {
                    queue!(self.stdout, SetAttribute(Attribute::Bold))?;
                }
            }
            queue!(self.stdout, Print(line))?;
        }
        queue!(self.stdout, ResetColor, SetAttribute(Attribute::Reset))?;
        if let Some((column, row)) = frame.cursor {
            queue!(self.stdout, MoveTo(column, row), Show)?;
        } else {
            queue!(self.stdout, Hide)?;
        }
        self.stdout.flush()
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(
            self.stdout,
            Show,
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = disable_raw_mode();
    }
}

#[derive(Debug)]
enum OverviewFailure {
    Query(P4Error),
    Mapping(DomainMappingError),
}

impl OverviewFailure {
    fn message(&self) -> String {
        match self {
            Self::Query(error) => error.to_string(),
            Self::Mapping(error) => {
                format!("Perforce returned an incomplete workspace or changelist response: {error}")
            }
        }
    }
}

#[derive(Debug)]
struct WorkspaceOverview {
    identity: WorkspaceIdentity,
    changes: Vec<Changelist>,
}

#[derive(Debug)]
enum PaneMessage {
    Overview {
        generation: u64,
        result: Result<WorkspaceOverview, OverviewFailure>,
    },
    Preflight {
        change: u64,
        request_id: u64,
        result: Box<Result<SubmitPreview, SubmitError>>,
    },
    Submit {
        change: u64,
        result: Result<SubmitResult, SubmitError>,
    },
    ExternalHandoff {
        change: u64,
        result: ExternalHandoffResult,
    },
    Reconcile {
        change: u64,
        request_id: u64,
        result: Result<SubmitReconciliationResult, SubmitError>,
    },
    DescriptionLoaded {
        change: u64,
        request_id: u64,
        result: Result<String, DescriptionApplyError>,
    },
    DescriptionApplied {
        change: u64,
        request_id: u64,
        result: Result<DescriptionApplyResult, DescriptionApplyError>,
    },
    ExplorerRoot {
        generation: u64,
        result: Result<LoadedDirectory, ExplorerRootFailure>,
    },
    ExplorerDirectory {
        generation: u64,
        path: PathBuf,
        result: Result<LoadedDirectory, ExplorerError>,
    },
}

#[derive(Debug)]
enum ExplorerRootFailure {
    NotInView,
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneView {
    Explorer,
    Review,
}

fn env_navigation_view(cwd: &Path) -> PaneView {
    let state_dir = std::env::var_os("HERDR_PLUGIN_STATE_DIR").map(PathBuf::from);
    match panel_restore::load_navigation_view(state_dir.as_deref(), cwd).as_deref() {
        Some("review") => PaneView::Review,
        Some("explorer") | None | Some(_) => PaneView::Explorer,
    }
}

const NAV_CHROME_ROWS: usize = 5;
const WHEEL_SCROLL_STEP: isize = 3;
const REVIEW_DESCRIPTION_FIRST_ROW: usize = 3;
const REVIEW_DESCRIPTION_ROWS: usize = 3;
const REVIEW_DESCRIPTION_ACTION_ROW: usize = 7;
const REVIEW_SUBMIT_ROW: usize = 8;
const REVIEW_SECTION_ROW: usize = 9;
const REVIEW_LIST_TOP: usize = 10;
const REVIEW_BOTTOM_ROWS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DragTarget {
    Explorer,
    Review,
}

#[derive(Debug, Clone, Copy)]
struct NavDrag {
    start_x: u16,
    start_y: u16,
    origin_scroll_x: usize,
    origin_scroll_y: usize,
    moved: bool,
    target: DragTarget,
    pending_index: Option<usize>,
}

#[derive(Debug, Clone)]
enum PromptKind {
    NewFile(PathBuf),
    NewFolder(PathBuf),
    Rename { path: PathBuf },
}

#[derive(Debug, Clone)]
enum MenuKind {
    Explorer {
        target: Option<(PathBuf, bool, bool)>,
    },
    Review {
        change_index: usize,
    },
}

#[derive(Debug, Clone)]
enum NavOverlay {
    Closed,
    Menu {
        x: u16,
        y: u16,
        kind: MenuKind,
        entries: Vec<MenuEntry>,
        selected: usize,
    },
    Prompt {
        title: String,
        input: String,
        kind: PromptKind,
    },
    ConfirmDelete {
        path: PathBuf,
        is_dir: bool,
    },
}

impl NavOverlay {
    fn is_open(&self) -> bool {
        !matches!(self, Self::Closed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MessageEffect {
    None,
    ReloadOverview,
    LoadExplorerRoot,
    LoadExplorerExpanded,
    LoadExplorerDirectory,
}

#[derive(Debug)]
enum ExternalHandoffResult {
    Launched(String),
    ValidationFailed(SubmitError),
    LaunchFailed(String),
}

#[derive(Debug)]
enum OverviewState {
    Loading,
    Ready(WorkspaceOverview),
    Failed(String),
}

#[derive(Debug)]
struct PaneModel {
    cwd: PathBuf,
    view: PaneView,
    overview: OverviewState,
    overview_generation: u64,
    selected: usize,
    explorer: ExplorerModel,
    content: ContentPaneClient,
    overlay: SubmitOverlay,
    submit_provider: Arc<SubmitProvider>,
    reload_provider_from_environment: bool,
    status: String,
    nav_width: u16,
    nav_height: u16,
    nav_overlay: NavOverlay,
    drag: Option<NavDrag>,
    review_scroll_y: usize,
    review_scroll_x: usize,
    review_follow: bool,
    description_editor: DescriptionEditor,
    description_request_id: u64,
}

impl PaneModel {
    #[cfg(test)]
    fn new(cwd: PathBuf) -> Self {
        let mut pane = Self::new_with_provider(cwd, Arc::new(SubmitProvider::Native));
        pane.reload_provider_from_environment = false;
        pane.view = PaneView::Explorer;
        pane
    }

    fn new_with_provider(cwd: PathBuf, submit_provider: Arc<SubmitProvider>) -> Self {
        let view = env_navigation_view(&cwd);
        Self {
            cwd: cwd.clone(),
            view,
            overview: OverviewState::Loading,
            overview_generation: 1,
            selected: 0,
            explorer: ExplorerModel::new(cwd.clone()),
            content: ContentPaneClient::new(cwd.clone()),
            overlay: SubmitOverlay::default(),
            submit_provider,
            reload_provider_from_environment: true,
            status: "Loading workspace files...".to_owned(),
            nav_width: 80,
            nav_height: 24,
            nav_overlay: NavOverlay::Closed,
            drag: None,
            review_scroll_y: 0,
            review_scroll_x: 0,
            review_follow: true,
            description_editor: DescriptionEditor::Idle,
            description_request_id: 0,
        }
    }

    fn set_view(&mut self, view: PaneView) {
        if self.view == view {
            return;
        }
        self.view = view;
        let Some(state_dir) = std::env::var_os("HERDR_PLUGIN_STATE_DIR").map(PathBuf::from) else {
            return;
        };
        let value = match view {
            PaneView::Explorer => "explorer",
            PaneView::Review => "review",
        };
        let _ = panel_restore::save_navigation_view(&state_dir, &self.cwd, value);
    }

    fn set_nav_size(&mut self, width: u16, height: u16) {
        self.nav_width = width.max(1);
        self.nav_height = height.max(1);
    }

    fn body_height(&self) -> usize {
        (self.nav_height as usize).saturating_sub(NAV_CHROME_ROWS)
    }

    fn body_width(&self) -> usize {
        (self.nav_width as usize).saturating_sub(1)
    }

    fn review_list_height(&self) -> usize {
        (self.nav_height as usize).saturating_sub(REVIEW_LIST_TOP + REVIEW_BOTTOM_ROWS)
    }

    fn reload_submit_provider(&mut self) {
        if self.reload_provider_from_environment {
            self.submit_provider = Arc::new(SubmitProvider::load_from_environment());
        }
    }

    fn issue_overlay_request<T: P4Transport + 'static>(
        &mut self,
        request: SubmitOverlayRequest,
        service: &Arc<P4WriteService<T>>,
        sender: &mpsc::Sender<PaneMessage>,
    ) {
        self.reload_submit_provider();
        dispatch_overlay(request, service, &self.cwd, sender);
    }

    fn selected_editable_description(&self) -> Result<(u64, String), String> {
        let OverviewState::Ready(overview) = &self.overview else {
            return Err("Changelist details are still loading".to_owned());
        };
        let Some(change) = overview.changes.get(self.selected) else {
            return Err("Select a numbered pending changelist first".to_owned());
        };
        let ChangelistId::Numbered(change_id) = change.id else {
            return Err("The default changelist description cannot be edited here".to_owned());
        };
        if change.status != ChangelistStatus::Pending {
            return Err("Only pending changelist descriptions can be edited".to_owned());
        }
        if change.owner != overview.identity.user {
            return Err("This changelist belongs to another user".to_owned());
        }
        if change.client != overview.identity.client {
            return Err("This changelist belongs to another client".to_owned());
        }
        Ok((change_id, change.description.clone()))
    }

    fn begin_description_edit<T: P4Transport + 'static>(
        &mut self,
        service: &Arc<P4WriteService<T>>,
        sender: &mpsc::Sender<PaneMessage>,
    ) {
        match self.selected_editable_description() {
            Ok((change, _)) => {
                self.description_request_id = self.description_request_id.wrapping_add(1);
                let request_id = self.description_request_id;
                self.description_editor = DescriptionEditor::Loading { change, request_id };
                self.status = format!("Loading full CL {change} description...");
                dispatch_description_load(Arc::clone(service), sender.clone(), change, request_id);
            }
            Err(message) => self.status = message,
        }
    }

    fn cancel_description_edit(&mut self) {
        if self.description_editor.cancel() {
            self.status = "Description edit cancelled".to_owned();
        } else {
            self.status = "Description Apply is already running and cannot be cancelled".to_owned();
        }
    }

    fn start_description_apply<T: P4Transport + 'static>(
        &mut self,
        service: &Arc<P4WriteService<T>>,
        sender: &mpsc::Sender<PaneMessage>,
        intent: DescriptionApplyIntent,
    ) {
        let state = std::mem::take(&mut self.description_editor);
        let DescriptionEditor::Editing {
            change,
            input,
            cursor,
        } = state
        else {
            self.description_editor = state;
            return;
        };
        if input.trim().is_empty() || input.contains('\0') {
            self.description_editor = DescriptionEditor::Editing {
                change,
                input,
                cursor,
            };
            self.status = "Description must contain non-whitespace text".to_owned();
            return;
        }
        self.description_request_id = self.description_request_id.wrapping_add(1);
        let request_id = self.description_request_id;
        self.description_editor = DescriptionEditor::Applying {
            change,
            input: input.clone(),
            cursor,
            request_id,
        };
        self.status = format!("Applying CL {change} description...");
        dispatch_description_apply(
            Arc::clone(service),
            sender.clone(),
            change,
            input,
            request_id,
            intent,
        );
    }

    fn complete_description_load(
        &mut self,
        change: u64,
        request_id: u64,
        result: Result<String, DescriptionApplyError>,
    ) {
        let state = std::mem::take(&mut self.description_editor);
        let DescriptionEditor::Loading {
            change: expected_change,
            request_id: expected_request,
        } = state
        else {
            self.description_editor = state;
            return;
        };
        if change != expected_change || request_id != expected_request {
            self.description_editor = DescriptionEditor::Loading {
                change: expected_change,
                request_id: expected_request,
            };
            return;
        }
        match result {
            Ok(description) => {
                self.description_editor.begin(change, &description);
                self.status =
                    format!("Editing CL {change} description · Ctrl+Enter applies · Esc cancels");
            }
            Err(error) => {
                self.description_editor = DescriptionEditor::Idle;
                self.status = error.to_string();
            }
        }
    }

    fn complete_description_apply(
        &mut self,
        change: u64,
        request_id: u64,
        result: Result<DescriptionApplyResult, DescriptionApplyError>,
    ) -> MessageEffect {
        let state = std::mem::take(&mut self.description_editor);
        let DescriptionEditor::Applying {
            change: expected_change,
            input,
            cursor,
            request_id: expected_request,
        } = state
        else {
            self.description_editor = state;
            return MessageEffect::None;
        };
        if change != expected_change || request_id != expected_request {
            self.description_editor = DescriptionEditor::Applying {
                change: expected_change,
                input,
                cursor,
                request_id: expected_request,
            };
            return MessageEffect::None;
        }
        match result {
            Ok(_) => {
                if let OverviewState::Ready(overview) = &mut self.overview
                    && let Some(changelist) = overview
                        .changes
                        .iter_mut()
                        .find(|candidate| candidate.id == ChangelistId::Numbered(change))
                {
                    changelist.description = input;
                }
                self.status = format!("CL {change} description updated and verified");
                MessageEffect::ReloadOverview
            }
            Err(error) => {
                self.description_editor = DescriptionEditor::Editing {
                    change,
                    input,
                    cursor,
                };
                self.status = error.to_string();
                MessageEffect::None
            }
        }
    }

    fn handle_paste(&mut self, value: &str) -> bool {
        if !matches!(self.description_editor, DescriptionEditor::Editing { .. }) {
            return false;
        }
        self.insert_description_text(
            value,
            "Description paste was rejected by the size or text safety limit",
        );
        true
    }

    fn insert_description_text(&mut self, value: &str, error_message: &'static str) {
        if self.description_editor.insert(value).is_err() {
            self.status = error_message.to_owned();
        }
    }

    fn handle_description_key<T: P4Transport + 'static>(
        &mut self,
        key: KeyEvent,
        service: &Arc<P4WriteService<T>>,
        sender: &mpsc::Sender<PaneMessage>,
    ) {
        if matches!(self.description_editor, DescriptionEditor::Applying { .. }) {
            self.status = "Description Apply is running; wait for verification".to_owned();
            return;
        }
        if matches!(self.description_editor, DescriptionEditor::Loading { .. }) {
            if key.code == KeyCode::Esc {
                self.cancel_description_edit();
            }
            return;
        }
        match key.code {
            KeyCode::Esc => self.cancel_description_edit(),
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.start_description_apply(
                    service,
                    sender,
                    DescriptionApplyIntent::ApplyShortcut,
                );
            }
            KeyCode::Enter => {
                self.insert_description_text("\n", "Description reached its input limit");
            }
            KeyCode::Tab => {
                self.insert_description_text("\t", "Description reached its input limit");
            }
            KeyCode::Backspace => self.description_editor.backspace(),
            KeyCode::Delete => self.description_editor.delete(),
            KeyCode::Left => self.description_editor.move_left(),
            KeyCode::Right => self.description_editor.move_right(),
            KeyCode::Up => self
                .description_editor
                .move_vertical(-1, (self.nav_width as usize).saturating_sub(4).max(1)),
            KeyCode::Down => self
                .description_editor
                .move_vertical(1, (self.nav_width as usize).saturating_sub(4).max(1)),
            KeyCode::Home => self.description_editor.move_home(),
            KeyCode::End => self.description_editor.move_end(),
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.insert_description_text(
                    &character.to_string(),
                    "Description reached its input limit",
                );
            }
            _ => {}
        }
    }

    fn reload_overview(&mut self, sender: &mpsc::Sender<PaneMessage>) {
        self.overview_generation = self.overview_generation.wrapping_add(1);
        request_overview(self.cwd.clone(), self.overview_generation, sender.clone());
    }

    fn begin_active_refresh(&mut self) -> MessageEffect {
        if self.view == PaneView::Review {
            self.overview = OverviewState::Loading;
            self.status = "Refreshing current Perforce workspace...".to_owned();
            return MessageEffect::ReloadOverview;
        }
        let OverviewState::Ready(overview) = &self.overview else {
            self.status = "Explorer refresh is waiting for workspace information".to_owned();
            return MessageEffect::None;
        };
        self.explorer
            .begin_workspace_load(overview.identity.clone());
        self.status = "Refreshing Explorer files and Perforce status...".to_owned();
        MessageEffect::LoadExplorerRoot
    }

    fn handle_message(&mut self, message: PaneMessage) -> MessageEffect {
        match message {
            PaneMessage::Overview { generation, result } => {
                if generation == self.overview_generation {
                    self.install_overview(result);
                    if matches!(self.overview, OverviewState::Ready(_)) {
                        return MessageEffect::LoadExplorerRoot;
                    }
                }
                MessageEffect::None
            }
            PaneMessage::Preflight {
                change,
                request_id,
                result,
            } => {
                self.overlay.complete_preflight(change, request_id, *result);
                MessageEffect::None
            }
            PaneMessage::Submit { change, result } => {
                let succeeded = result.is_ok();
                self.overlay.complete_submit(change, result);
                if succeeded {
                    MessageEffect::ReloadOverview
                } else {
                    MessageEffect::None
                }
            }
            PaneMessage::ExternalHandoff { change, result } => {
                match result {
                    ExternalHandoffResult::Launched(provider) => {
                        self.overlay.complete_external_handoff(change, Ok(provider))
                    }
                    ExternalHandoffResult::ValidationFailed(error) => {
                        self.overlay.complete_submit(change, Err(error));
                    }
                    ExternalHandoffResult::LaunchFailed(detail) => {
                        self.overlay.complete_external_handoff(change, Err(detail))
                    }
                }
                MessageEffect::None
            }
            PaneMessage::Reconcile {
                change,
                request_id,
                result,
            } => {
                let submitted = matches!(
                    result,
                    Ok(SubmitReconciliationResult::ConfirmedSubmitted(_))
                );
                self.overlay
                    .complete_reconciliation(change, request_id, result);
                if submitted {
                    MessageEffect::ReloadOverview
                } else {
                    MessageEffect::None
                }
            }
            PaneMessage::DescriptionLoaded {
                change,
                request_id,
                result,
            } => {
                self.complete_description_load(change, request_id, result);
                MessageEffect::None
            }
            PaneMessage::DescriptionApplied {
                change,
                request_id,
                result,
            } => self.complete_description_apply(change, request_id, result),
            PaneMessage::ExplorerRoot { generation, result } => {
                match result {
                    Ok(listing) => self.explorer.install_root(generation, Ok(listing)),
                    Err(ExplorerRootFailure::NotInView) => {
                        self.explorer.install_not_in_view(generation);
                    }
                    Err(ExplorerRootFailure::Failed(message)) => {
                        self.explorer.install_failure(generation, message);
                    }
                }
                if matches!(self.explorer.load_state(), ExplorerLoadState::Ready)
                    && !self.explorer.remaining_expanded_directories().is_empty()
                {
                    MessageEffect::LoadExplorerExpanded
                } else {
                    MessageEffect::None
                }
            }
            PaneMessage::ExplorerDirectory {
                generation,
                path,
                result,
            } => {
                self.explorer.install_directory(generation, path, result);
                MessageEffect::None
            }
        }
    }

    fn install_overview(&mut self, result: Result<WorkspaceOverview, OverviewFailure>) {
        let selected_id = self.selected_changelist().map(|change| change.id);
        match result {
            Ok(overview) => {
                self.selected = selected_id
                    .and_then(|id| overview.changes.iter().position(|change| change.id == id))
                    .or_else(|| {
                        overview
                            .changes
                            .iter()
                            .position(|change| matches!(change.id, ChangelistId::Numbered(_)))
                    })
                    .unwrap_or(0);
                self.status = format!("Loaded {} changelist(s)", overview.changes.len());
                self.explorer
                    .begin_workspace_load(overview.identity.clone());
                self.overview = OverviewState::Ready(overview);
            }
            Err(error) => {
                self.status = "Workspace refresh failed".to_owned();
                self.explorer.on_overview_failed(error.message());
                self.overview = OverviewState::Failed(error.message());
            }
        }
    }

    fn open_selected_diff(&mut self) -> bool {
        let Some((change, path, action)) = self.explorer.jump_target() else {
            self.status = "Selected file is not opened in a changelist".to_owned();
            return false;
        };
        self.status = self
            .content
            .show_diff(change, path, Some(action))
            .unwrap_or_else(|error| error);
        true
    }

    fn open_selected_file(&mut self) -> bool {
        let Some(path) = self.explorer.selected_file_path() else {
            return false;
        };
        self.status = self.content.show_file(path).unwrap_or_else(|error| error);
        true
    }

    fn show_selected_explorer_status(&mut self) {
        if let Some((badge, label)) = self.explorer.selected_status() {
            let badge = if badge.is_empty() {
                "✓"
            } else {
                badge.as_str()
            };
            self.status = format!("{badge}  {label}");
        }
    }

    fn open_selected_changelist(&mut self) -> bool {
        let Some(change) = self.selected_changelist().map(|change| change.id) else {
            return false;
        };
        self.status = self
            .content
            .show_changelist(change)
            .unwrap_or_else(|error| error);
        true
    }

    fn selected_changelist(&self) -> Option<&Changelist> {
        let OverviewState::Ready(overview) = &self.overview else {
            return None;
        };
        overview.changes.get(self.selected)
    }

    fn open_selected_submit<T: P4Transport + 'static>(
        &mut self,
        service: &Arc<P4WriteService<T>>,
        sender: &mpsc::Sender<PaneMessage>,
    ) {
        if self.view != PaneView::Review {
            self.status = "Switch to Review (2) to submit".to_owned();
            return;
        }
        let Some(change) = self
            .selected_changelist()
            .and_then(|change| match change.id {
                ChangelistId::Numbered(change) => Some(change),
                ChangelistId::Default => None,
            })
        else {
            self.status = "Default changelist Submit is disabled".to_owned();
            return;
        };
        if let Some(request) = self.overlay.open(change) {
            self.issue_overlay_request(request, service, sender);
        }
    }

    fn handle_key<T: P4Transport + 'static>(
        &mut self,
        key: KeyEvent,
        service: &Arc<P4WriteService<T>>,
        sender: &mpsc::Sender<PaneMessage>,
    ) -> bool {
        if key.kind != KeyEventKind::Press {
            return false;
        }
        if !matches!(self.overlay.state(), SubmitOverlayState::Closed) {
            match key.code {
                KeyCode::Esc => {
                    self.overlay.handle_intent(SubmitIntent::Escape);
                }
                KeyCode::Enter if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(request) = self.overlay.handle_intent(SubmitIntent::CtrlEnter) {
                        self.issue_overlay_request(request, service, sender);
                    }
                }
                KeyCode::Enter => {
                    self.overlay.handle_intent(SubmitIntent::Enter);
                }
                KeyCode::Char('r') | KeyCode::Char('R') => {
                    if let Some(request) = self.overlay.refresh() {
                        self.issue_overlay_request(request, service, sender);
                    }
                }
                _ => {}
            }
            return false;
        }

        if self.nav_overlay.is_open() {
            return self.handle_nav_overlay_key(key, service, sender);
        }

        if self.view == PaneView::Review && self.description_editor.is_active() {
            self.handle_description_key(key, service, sender);
            return false;
        }

        match key.code {
            KeyCode::Char('q') => true,
            KeyCode::Char('1') => {
                self.set_view(PaneView::Explorer);
                false
            }
            KeyCode::Char('2') => {
                self.set_view(PaneView::Review);
                false
            }
            KeyCode::Char('m') => {
                self.open_menu_for_selection();
                false
            }
            KeyCode::Char('r') | KeyCode::Char('R') => {
                let effect = self.begin_active_refresh();
                apply_message_effect(self, effect, sender);
                false
            }
            KeyCode::Char('s') | KeyCode::Char('S') => {
                self.open_selected_submit(service, sender);
                false
            }
            KeyCode::Char('e') | KeyCode::Char('E') if self.view == PaneView::Review => {
                self.begin_description_edit(service, sender);
                false
            }
            _ if self.view == PaneView::Explorer => self.handle_explorer_key(key, sender),
            KeyCode::Enter => {
                self.open_selected_changelist();
                false
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                self.review_follow = true;
                false
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let count = match &self.overview {
                    OverviewState::Ready(overview) => overview.changes.len(),
                    OverviewState::Loading | OverviewState::Failed(_) => 0,
                };
                self.selected = self.selected.saturating_add(1).min(count.saturating_sub(1));
                self.review_follow = true;
                false
            }
            KeyCode::PageUp => {
                self.selected = self
                    .selected
                    .saturating_sub(self.review_list_height().max(1));
                self.review_follow = true;
                false
            }
            KeyCode::PageDown => {
                let count = match &self.overview {
                    OverviewState::Ready(overview) => overview.changes.len(),
                    OverviewState::Loading | OverviewState::Failed(_) => 0,
                };
                self.selected = self
                    .selected
                    .saturating_add(self.review_list_height().max(1))
                    .min(count.saturating_sub(1));
                self.review_follow = true;
                false
            }
            _ => false,
        }
    }

    fn handle_explorer_key(&mut self, key: KeyEvent, sender: &mpsc::Sender<PaneMessage>) -> bool {
        match key.code {
            KeyCode::Char('o') | KeyCode::Char('O') => {
                if let Some(path) = self.explorer.selected_path() {
                    match open_with_default_app(path) {
                        Ok(()) => self.status = format!("Opened {}", path.display()),
                        Err(error) => self.status = error,
                    }
                }
                false
            }
            KeyCode::Left => {
                let action = self.explorer.expand_or_collapse(false);
                self.apply_explorer_action(action, sender);
                false
            }
            KeyCode::Right => {
                let action = self.explorer.expand_or_collapse(true);
                self.apply_explorer_action(action, sender);
                false
            }
            KeyCode::Enter => {
                let action = self.explorer.activate_selection();
                self.apply_explorer_action(action, sender);
                false
            }
            KeyCode::Char('d') | KeyCode::Char('D') => {
                self.open_selected_diff();
                false
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.explorer.move_selection(-1);
                self.show_selected_explorer_status();
                false
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.explorer.move_selection(1);
                self.show_selected_explorer_status();
                false
            }
            KeyCode::PageUp => {
                self.explorer
                    .move_selection(-(self.body_height().max(1) as isize));
                self.show_selected_explorer_status();
                false
            }
            KeyCode::PageDown => {
                self.explorer
                    .move_selection(self.body_height().max(1) as isize);
                self.show_selected_explorer_status();
                false
            }
            _ => false,
        }
    }

    fn apply_explorer_action(
        &mut self,
        action: ExplorerAction,
        sender: &mpsc::Sender<PaneMessage>,
    ) {
        match action {
            ExplorerAction::None => {}
            ExplorerAction::LoadDirectory => {
                apply_message_effect(self, MessageEffect::LoadExplorerDirectory, sender);
            }
            ExplorerAction::OpenFile => {
                self.open_selected_file();
            }
        }
    }

    fn handle_mouse<T: P4Transport + 'static>(
        &mut self,
        mouse: MouseEvent,
        hits: &HitTargets,
        service: &Arc<P4WriteService<T>>,
        sender: &mpsc::Sender<PaneMessage>,
    ) -> bool {
        if !matches!(self.overlay.state(), SubmitOverlayState::Closed) {
            if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                return false;
            }
            if hits
                .cancel
                .is_some_and(|target| target.contains(mouse.column, mouse.row))
            {
                self.overlay.handle_intent(SubmitIntent::Cancel);
            } else if hits
                .submit
                .is_some_and(|target| target.contains(mouse.column, mouse.row))
            {
                if let Some(request) = self.overlay.handle_intent(SubmitIntent::SubmitButton) {
                    self.issue_overlay_request(request, service, sender);
                }
            } else if hits
                .refresh
                .is_some_and(|target| target.contains(mouse.column, mouse.row))
            {
                if let Some(request) = self.overlay.refresh() {
                    self.issue_overlay_request(request, service, sender);
                }
            }
            return true;
        }

        if self.nav_overlay.is_open() {
            return self.handle_nav_overlay_mouse(mouse, hits, service, sender);
        }

        if self.handle_description_mouse(mouse, hits, service, sender) {
            return true;
        }

        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && hits
                .review_submit
                .is_some_and(|target| target.contains(mouse.column, mouse.row))
        {
            self.drag = None;
            self.open_selected_submit(service, sender);
            return true;
        }

        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll_active_view(-WHEEL_SCROLL_STEP),
            MouseEventKind::ScrollDown => self.scroll_active_view(WHEEL_SCROLL_STEP),
            MouseEventKind::ScrollLeft => self.pan_active_view(-4),
            MouseEventKind::ScrollRight => self.pan_active_view(4),
            MouseEventKind::Down(MouseButton::Right) => {
                self.open_context_menu(mouse.column, mouse.row, hits);
                true
            }
            MouseEventKind::Down(MouseButton::Left) => self.handle_left_down(mouse, hits),
            MouseEventKind::Drag(MouseButton::Left) => self.handle_left_drag(mouse),
            MouseEventKind::Up(MouseButton::Left) => self.handle_left_up(sender),
            _ => false,
        }
    }

    fn handle_description_mouse<T: P4Transport + 'static>(
        &mut self,
        mouse: MouseEvent,
        hits: &HitTargets,
        service: &Arc<P4WriteService<T>>,
        sender: &mpsc::Sender<PaneMessage>,
    ) -> bool {
        if self.view != PaneView::Review {
            return false;
        }
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            if self.description_editor.is_active() {
                self.drag = None;
                self.status =
                    "Finish or cancel the description edit before using the CL list".to_owned();
                return true;
            }
            return false;
        }
        if hits
            .description_review
            .is_some_and(|target| target.contains(mouse.column, mouse.row))
        {
            self.start_description_apply(service, sender, DescriptionApplyIntent::ApplyButton);
            self.drag = None;
            return true;
        }
        if hits
            .description_cancel
            .is_some_and(|target| target.contains(mouse.column, mouse.row))
        {
            self.cancel_description_edit();
            self.drag = None;
            return true;
        }
        if hits
            .description_apply
            .is_some_and(|target| target.contains(mouse.column, mouse.row))
        {
            self.start_description_apply(service, sender, DescriptionApplyIntent::ApplyButton);
            self.drag = None;
            return true;
        }
        if hits
            .description_edit
            .is_some_and(|target| target.contains(mouse.column, mouse.row))
        {
            let was_active = self.description_editor.is_active();
            if !was_active {
                self.begin_description_edit(service, sender);
            }
            if matches!(self.description_editor, DescriptionEditor::Editing { .. })
                && (REVIEW_DESCRIPTION_FIRST_ROW as u16
                    ..(REVIEW_DESCRIPTION_FIRST_ROW + REVIEW_DESCRIPTION_ROWS) as u16)
                    .contains(&mouse.row)
            {
                let content_width = (self.nav_width as usize).saturating_sub(4).max(1);
                let first_line = if was_active {
                    self.description_editor
                        .first_visible_line(content_width, REVIEW_DESCRIPTION_ROWS)
                } else {
                    0
                };
                self.description_editor.set_cursor_from_visual_position(
                    first_line,
                    mouse.row as usize - REVIEW_DESCRIPTION_FIRST_ROW,
                    (mouse.column as usize).saturating_sub(2),
                    content_width,
                );
            }
            self.drag = None;
            return true;
        }
        if self.description_editor.is_active() {
            self.status =
                "Finish or cancel the description edit before changing selection".to_owned();
            return true;
        }
        false
    }

    fn handle_left_down(&mut self, mouse: MouseEvent, hits: &HitTargets) -> bool {
        if hits
            .view_explorer
            .is_some_and(|target| target.contains(mouse.column, mouse.row))
        {
            self.set_view(PaneView::Explorer);
            self.drag = None;
            return true;
        }
        if hits
            .view_review
            .is_some_and(|target| target.contains(mouse.column, mouse.row))
        {
            self.set_view(PaneView::Review);
            self.drag = None;
            return true;
        }
        if self.view == PaneView::Explorer {
            let pending = hits
                .explorer_rows
                .iter()
                .position(|target| target.contains(mouse.column, mouse.row))
                .map(|index| hits.explorer_offset.saturating_add(index));
            if let Some(index) = pending {
                self.explorer.select_index(index);
                self.show_selected_explorer_status();
            }
            let (offset, _, _) = self.explorer.tree_window(self.body_height());
            self.drag = Some(NavDrag {
                start_x: mouse.column,
                start_y: mouse.row,
                origin_scroll_x: self.explorer.scroll_x(),
                origin_scroll_y: offset,
                moved: false,
                target: DragTarget::Explorer,
                pending_index: pending,
            });
            return true;
        }
        let pending = hits
            .changelists
            .iter()
            .position(|target| target.contains(mouse.column, mouse.row))
            .map(|index| hits.changelist_offset.saturating_add(index));
        if let Some(index) = pending {
            self.selected = index;
        }
        self.drag = Some(NavDrag {
            start_x: mouse.column,
            start_y: mouse.row,
            origin_scroll_x: self.review_scroll_x,
            origin_scroll_y: self.review_offset(self.review_list_height()),
            moved: false,
            target: DragTarget::Review,
            pending_index: pending,
        });
        true
    }

    fn handle_left_drag(&mut self, mouse: MouseEvent) -> bool {
        let Some(drag) = self.drag else {
            return false;
        };
        let dx = drag.start_x as isize - mouse.column as isize;
        let dy = drag.start_y as isize - mouse.row as isize;
        let moved = dx.abs() >= 1 || dy.abs() >= 1;
        match drag.target {
            DragTarget::Explorer => {
                self.explorer.set_scroll_x(
                    drag.origin_scroll_x.saturating_add_signed(dx),
                    self.body_width(),
                    self.explorer_content_width(),
                );
                self.explorer.set_scroll_y(
                    drag.origin_scroll_y.saturating_add_signed(dy),
                    self.body_height(),
                );
            }
            DragTarget::Review => {
                self.review_follow = false;
                self.set_review_scroll(
                    drag.origin_scroll_y.saturating_add_signed(dy),
                    drag.origin_scroll_x.saturating_add_signed(dx),
                );
            }
        }
        if let Some(drag) = self.drag.as_mut() {
            drag.moved = drag.moved || moved;
        }
        moved
    }

    fn handle_left_up(&mut self, sender: &mpsc::Sender<PaneMessage>) -> bool {
        let Some(drag) = self.drag.take() else {
            return false;
        };
        if drag.moved {
            return true;
        }
        match drag.target {
            DragTarget::Explorer => {
                if let Some(index) = drag.pending_index {
                    let action = self.explorer.activate_index(index);
                    self.apply_explorer_action(action, sender);
                }
            }
            DragTarget::Review => {
                if drag.pending_index.is_some() {
                    self.open_selected_changelist();
                }
            }
        }
        true
    }

    fn scroll_active_view(&mut self, delta: isize) -> bool {
        match self.view {
            PaneView::Explorer => {
                self.explorer.scroll_vertical(delta, self.body_height());
            }
            PaneView::Review => {
                let height = self.review_list_height();
                let offset = self.review_offset(height);
                self.review_follow = false;
                self.set_review_scroll(offset.saturating_add_signed(delta), self.review_scroll_x);
            }
        }
        true
    }

    fn pan_active_view(&mut self, delta: isize) -> bool {
        match self.view {
            PaneView::Explorer => {
                self.explorer.set_scroll_x(
                    self.explorer.scroll_x().saturating_add_signed(delta),
                    self.body_width(),
                    self.explorer_content_width(),
                );
            }
            PaneView::Review => {
                self.set_review_scroll(
                    self.review_scroll_y,
                    self.review_scroll_x.saturating_add_signed(delta),
                );
            }
        }
        true
    }

    fn review_count(&self) -> usize {
        match &self.overview {
            OverviewState::Ready(overview) => overview.changes.len(),
            OverviewState::Loading | OverviewState::Failed(_) => 0,
        }
    }

    fn review_offset(&self, visible_rows: usize) -> usize {
        let count = self.review_count();
        if visible_rows == 0 || count == 0 {
            return 0;
        }
        let height = visible_rows.min(count);
        let max_offset = count.saturating_sub(height);
        if self.review_follow {
            self.selected
                .saturating_add(1)
                .saturating_sub(height)
                .min(max_offset)
        } else {
            self.review_scroll_y.min(max_offset)
        }
    }

    fn set_review_scroll(&mut self, y: usize, x: usize) {
        let height = self.review_list_height();
        let count = self.review_count();
        let max_y = count.saturating_sub(height.min(count.max(1)));
        self.review_scroll_y = y.min(max_y);
        self.review_scroll_x = x.min(
            self.review_content_width()
                .saturating_sub(self.body_width()),
        );
    }

    fn explorer_content_width(&self) -> usize {
        let selected = self.explorer.selected_path();
        let tree_width = self
            .explorer
            .visible_rows()
            .iter()
            .map(|row| {
                display_width(&ExplorerModel::format_row(
                    row,
                    selected.is_some_and(|path| path == row.path),
                ))
            })
            .max()
            .unwrap_or(0);
        tree_width.max(display_width(explorer_legend()))
    }

    fn review_content_width(&self) -> usize {
        let OverviewState::Ready(overview) = &self.overview else {
            return 0;
        };
        overview
            .changes
            .iter()
            .enumerate()
            .map(|(index, change)| {
                display_width(&format_changelist_row(index == self.selected, change))
            })
            .max()
            .unwrap_or(0)
    }

    fn open_menu_for_selection(&mut self) {
        match self.view {
            PaneView::Explorer => {
                let target = self
                    .explorer
                    .selected_row_info()
                    .map(|(path, kind, opened)| {
                        (
                            path,
                            kind == crate::domain::ExplorerEntryKind::Directory,
                            opened,
                        )
                    });
                let (x, y) = self.selection_anchor();
                self.show_explorer_menu(x, y, target);
            }
            PaneView::Review => {
                if self.review_count() == 0 {
                    return;
                }
                let (x, y) = self.selection_anchor();
                self.show_review_menu(x, y, self.selected);
            }
        }
    }

    fn open_context_menu(&mut self, x: u16, y: u16, hits: &HitTargets) {
        match self.view {
            PaneView::Explorer => {
                let target = hits
                    .explorer_rows
                    .iter()
                    .position(|target| target.contains(x, y))
                    .map(|index| hits.explorer_offset.saturating_add(index))
                    .and_then(|index| {
                        self.explorer.select_index(index);
                        self.explorer
                            .selected_row_info()
                            .map(|(path, kind, opened)| {
                                (
                                    path,
                                    kind == crate::domain::ExplorerEntryKind::Directory,
                                    opened,
                                )
                            })
                    });
                self.show_explorer_menu(x, y, target);
            }
            PaneView::Review => {
                if let Some(index) = hits
                    .changelists
                    .iter()
                    .position(|target| target.contains(x, y))
                {
                    self.selected = hits.changelist_offset.saturating_add(index);
                    self.show_review_menu(x, y, self.selected);
                } else if self.review_count() > 0 {
                    self.show_review_menu(x, y, self.selected);
                }
            }
        }
    }

    fn show_explorer_menu(&mut self, x: u16, y: u16, target: Option<(PathBuf, bool, bool)>) {
        let root = self.explorer.cwd();
        let entries = explorer_menu_entries(target.as_ref().map(|(path, is_dir, opened)| {
            ExplorerMenuTarget {
                is_dir: *is_dir,
                opened: *opened,
                is_root: is_same_path(path, root),
            }
        }));
        self.nav_overlay = NavOverlay::Menu {
            x,
            y,
            kind: MenuKind::Explorer { target },
            selected: first_action_index(&entries),
            entries,
        };
    }

    fn show_review_menu(&mut self, x: u16, y: u16, change_index: usize) {
        let can_submit = self
            .selected_changelist()
            .is_some_and(|change| matches!(change.id, ChangelistId::Numbered(_)));
        let entries = review_menu_entries(can_submit);
        self.nav_overlay = NavOverlay::Menu {
            x,
            y,
            kind: MenuKind::Review { change_index },
            selected: first_action_index(&entries),
            entries,
        };
    }

    fn selection_anchor(&self) -> (u16, u16) {
        (2, 2)
    }

    fn handle_nav_overlay_key<T: P4Transport + 'static>(
        &mut self,
        key: KeyEvent,
        service: &Arc<P4WriteService<T>>,
        sender: &mpsc::Sender<PaneMessage>,
    ) -> bool {
        if matches!(key.code, KeyCode::Esc) && self.nav_overlay.is_open() {
            self.nav_overlay = NavOverlay::Closed;
            return false;
        }
        if matches!(self.nav_overlay, NavOverlay::Menu { .. }) {
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    if let NavOverlay::Menu {
                        selected, entries, ..
                    } = &mut self.nav_overlay
                    {
                        *selected = step_action_index(entries, *selected, -1);
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if let NavOverlay::Menu {
                        selected, entries, ..
                    } = &mut self.nav_overlay
                    {
                        *selected = step_action_index(entries, *selected, 1);
                    }
                }
                KeyCode::Enter => self.activate_menu_entry(service, sender),
                _ => {}
            }
            return false;
        }
        if matches!(self.nav_overlay, NavOverlay::Prompt { .. }) {
            match key.code {
                KeyCode::Enter => self.confirm_prompt(sender),
                KeyCode::Backspace => {
                    if let NavOverlay::Prompt { input, .. } = &mut self.nav_overlay {
                        input.pop();
                    }
                }
                KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let NavOverlay::Prompt { input, .. } = &mut self.nav_overlay {
                        input.push(character);
                    }
                }
                _ => {}
            }
            return false;
        }
        if matches!(self.nav_overlay, NavOverlay::ConfirmDelete { .. }) {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    let (path, is_dir) = match &self.nav_overlay {
                        NavOverlay::ConfirmDelete { path, is_dir } => (path.clone(), *is_dir),
                        _ => return false,
                    };
                    self.nav_overlay = NavOverlay::Closed;
                    self.apply_delete(&path, is_dir, sender);
                }
                KeyCode::Char('n') | KeyCode::Char('N') => {
                    self.nav_overlay = NavOverlay::Closed;
                }
                _ => {}
            }
            return false;
        }
        false
    }

    fn handle_nav_overlay_mouse<T: P4Transport + 'static>(
        &mut self,
        mouse: MouseEvent,
        hits: &HitTargets,
        service: &Arc<P4WriteService<T>>,
        sender: &mpsc::Sender<PaneMessage>,
    ) -> bool {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Right) => {
                self.nav_overlay = NavOverlay::Closed;
                self.open_context_menu(mouse.column, mouse.row, hits);
                true
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let NavOverlay::Menu {
                    x,
                    y,
                    entries,
                    selected,
                    ..
                } = &self.nav_overlay
                else {
                    return false;
                };
                let rect = menu_popup_rect(*x, *y, entries, self.nav_width, self.nav_height);
                let inner_height = usize::from(rect.height.saturating_sub(2));
                let offset = menu_window(*selected, entries.len(), inner_height);
                let hit = menu_index_at(rect, mouse.column, mouse.row, entries.len(), offset);
                let actionable =
                    hit.is_some_and(|index| matches!(entries[index], MenuEntry::Action(..)));
                if let Some(index) = hit.filter(|_| actionable) {
                    if let NavOverlay::Menu { selected, .. } = &mut self.nav_overlay {
                        *selected = index;
                    }
                    self.activate_menu_entry(service, sender);
                    return true;
                }
                if hit.is_none() {
                    self.nav_overlay = NavOverlay::Closed;
                }
                true
            }
            _ => false,
        }
    }

    fn activate_menu_entry<T: P4Transport + 'static>(
        &mut self,
        service: &Arc<P4WriteService<T>>,
        sender: &mpsc::Sender<PaneMessage>,
    ) {
        let NavOverlay::Menu {
            kind,
            entries,
            selected,
            ..
        } = std::mem::replace(&mut self.nav_overlay, NavOverlay::Closed)
        else {
            return;
        };
        let MenuEntry::Action(action, _) = entries
            .get(selected)
            .copied()
            .unwrap_or(MenuEntry::Separator)
        else {
            return;
        };
        match kind {
            MenuKind::Explorer { target } => {
                self.apply_explorer_menu(action, target, sender);
            }
            MenuKind::Review { change_index } => {
                self.selected = change_index;
                self.apply_review_menu(action, service, sender);
            }
        }
    }

    fn apply_explorer_menu(
        &mut self,
        action: MenuAction,
        target: Option<(PathBuf, bool, bool)>,
        sender: &mpsc::Sender<PaneMessage>,
    ) {
        let root = self.explorer.cwd().to_path_buf();
        let create_dir = target
            .as_ref()
            .map(|(path, is_dir, _)| {
                if *is_dir {
                    path.clone()
                } else {
                    path.parent().unwrap_or(&root).to_path_buf()
                }
            })
            .unwrap_or_else(|| root.clone());
        match action {
            MenuAction::NewFile => {
                self.nav_overlay = NavOverlay::Prompt {
                    title: "New file".into(),
                    input: String::new(),
                    kind: PromptKind::NewFile(create_dir),
                };
            }
            MenuAction::NewFolder => {
                self.nav_overlay = NavOverlay::Prompt {
                    title: "New folder".into(),
                    input: String::new(),
                    kind: PromptKind::NewFolder(create_dir),
                };
            }
            MenuAction::Rename => {
                if let Some((path, _, _)) = target {
                    if is_same_path(&path, &root) {
                        self.status = "The workspace root cannot be renamed".to_owned();
                        return;
                    }
                    let name = path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    self.nav_overlay = NavOverlay::Prompt {
                        title: "Rename".into(),
                        input: name,
                        kind: PromptKind::Rename { path },
                    };
                }
            }
            MenuAction::Delete => {
                if let Some((path, is_dir, _)) = target {
                    if is_same_path(&path, &root) {
                        self.status = "The workspace root cannot be deleted".to_owned();
                        return;
                    }
                    self.nav_overlay = NavOverlay::ConfirmDelete { path, is_dir };
                }
            }
            MenuAction::CopyPath => {
                if let Some((path, _, _)) = target {
                    self.copy_status(&path.display().to_string());
                }
            }
            MenuAction::CopyRelativePath => {
                if let Some((path, _, _)) = target {
                    self.copy_status(&relative_path_text(&root, &path));
                }
            }
            MenuAction::OpenExternal => {
                if let Some((path, _, _)) = target {
                    match open_with_default_app(&path) {
                        Ok(()) => self.status = format!("Opened {}", path.display()),
                        Err(error) => self.status = error,
                    }
                }
            }
            MenuAction::OpenDiff => {
                self.open_selected_diff();
            }
            MenuAction::Reveal => {
                let path = target
                    .as_ref()
                    .map(|(path, _, _)| path.as_path())
                    .unwrap_or(&root);
                actions::reveal(path);
                self.status = format!("Revealed {}", path.display());
            }
            MenuAction::OpenChangelist | MenuAction::CopyChangelist | MenuAction::SubmitReview => {}
        }
        let _ = sender;
    }

    fn apply_review_menu<T: P4Transport + 'static>(
        &mut self,
        action: MenuAction,
        service: &Arc<P4WriteService<T>>,
        sender: &mpsc::Sender<PaneMessage>,
    ) {
        match action {
            MenuAction::OpenChangelist => {
                self.open_selected_changelist();
            }
            MenuAction::CopyChangelist => {
                if let Some(change) = self.selected_changelist() {
                    self.copy_status(&change.id.to_string());
                }
            }
            MenuAction::Reveal => {
                actions::reveal(&self.cwd);
                self.status = format!("Revealed {}", self.cwd.display());
            }
            MenuAction::SubmitReview => {
                let Some(change) = self
                    .selected_changelist()
                    .and_then(|change| match change.id {
                        ChangelistId::Numbered(change) => Some(change),
                        ChangelistId::Default => None,
                    })
                else {
                    self.status = "Default changelist Submit is disabled".to_owned();
                    return;
                };
                if let Some(request) = self.overlay.open(change) {
                    self.issue_overlay_request(request, service, sender);
                }
            }
            _ => {}
        }
    }

    fn confirm_prompt(&mut self, sender: &mpsc::Sender<PaneMessage>) {
        let NavOverlay::Prompt { input, kind, .. } =
            std::mem::replace(&mut self.nav_overlay, NavOverlay::Closed)
        else {
            return;
        };
        let Some(name) = validate_name(&input).map(str::to_owned) else {
            self.status = "Enter a file name without path separators".to_owned();
            return;
        };
        let result = match kind {
            PromptKind::NewFile(dir) => actions::create_file(&dir, &name).map(|path| (dir, path)),
            PromptKind::NewFolder(dir) => {
                actions::create_folder(&dir, &name).map(|path| (dir, path))
            }
            PromptKind::Rename { path } => {
                if is_same_path(&path, &self.cwd) {
                    self.status = "The workspace root cannot be renamed".to_owned();
                    return;
                }
                let parent = path.parent().unwrap_or(&self.cwd).to_path_buf();
                actions::rename(&path, &name).map(|new_path| (parent, new_path))
            }
        };
        match result {
            Ok((parent, path)) => {
                self.status = format!("Updated {}", path.display());
                self.explorer.select_path(path);
                self.reload_explorer_directory(parent, sender);
            }
            Err(error) => self.status = error.to_string(),
        }
    }

    fn apply_delete(&mut self, path: &Path, is_dir: bool, sender: &mpsc::Sender<PaneMessage>) {
        if is_same_path(path, &self.cwd) {
            self.status = "The workspace root cannot be deleted".to_owned();
            return;
        }
        match actions::delete(path, is_dir) {
            Ok(()) => {
                let parent = path.parent().unwrap_or(&self.cwd).to_path_buf();
                self.status = format!("Deleted {}", path.display());
                self.explorer.select_path(parent.clone());
                self.reload_explorer_directory(parent, sender);
            }
            Err(error) => self.status = error.to_string(),
        }
    }

    fn reload_explorer_directory(&mut self, path: PathBuf, sender: &mpsc::Sender<PaneMessage>) {
        self.explorer.invalidate_directory(path);
        self.apply_explorer_action(ExplorerAction::LoadDirectory, sender);
    }

    fn copy_status(&mut self, text: &str) {
        match copy_to_clipboard(text) {
            Ok(()) => self.status = "Copied to clipboard".to_owned(),
            Err(error) => self.status = error.to_string(),
        }
    }
}

fn format_changelist_row(selected: bool, change: &Changelist) -> String {
    let marker = if selected { ">" } else { " " };
    let description = change
        .description
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("<no description>")
        .trim();
    format!(
        "{marker} CL {}  {}  {description}",
        change.id,
        change.status.canonical_name()
    )
}

fn menu_popup_rect(x: u16, y: u16, entries: &[MenuEntry], width: u16, height: u16) -> Rect {
    let label_width = entries
        .iter()
        .map(|entry| match entry {
            MenuEntry::Action(_, label) => display_width(label),
            MenuEntry::Separator => 0,
        })
        .max()
        .unwrap_or(0) as u16;
    let popup_width = (label_width.saturating_add(4)).min(width).max(8);
    let popup_height = (entries.len() as u16).saturating_add(2).min(height).max(3);
    let px = x.min(width.saturating_sub(popup_width));
    let py = y.saturating_add(1).min(height.saturating_sub(popup_height));
    Rect::area(px, py, popup_width, popup_height)
}

fn menu_index_at(rect: Rect, x: u16, y: u16, len: usize, offset: usize) -> Option<usize> {
    if !rect.contains(x, y) {
        return None;
    }
    if y <= rect.y || y >= rect.y.saturating_add(rect.height.max(1)).saturating_sub(1) {
        return None;
    }
    let index = offset + usize::from(y.saturating_sub(rect.y.saturating_add(1)));
    (index < len).then_some(index)
}

fn apply_message_effect(
    pane: &mut PaneModel,
    effect: MessageEffect,
    sender: &mpsc::Sender<PaneMessage>,
) {
    match effect {
        MessageEffect::None => {}
        MessageEffect::ReloadOverview => pane.reload_overview(sender),
        MessageEffect::LoadExplorerRoot => request_explorer_root(pane, sender),
        MessageEffect::LoadExplorerExpanded => {
            let generation = pane.explorer.generation();
            let identity = match &pane.overview {
                OverviewState::Ready(overview) => overview.identity.clone(),
                OverviewState::Loading | OverviewState::Failed(_) => return,
            };
            for path in pane.explorer.remaining_expanded_directories() {
                request_explorer_directory(
                    pane.cwd.clone(),
                    identity.clone(),
                    generation,
                    path,
                    sender.clone(),
                );
            }
        }
        MessageEffect::LoadExplorerDirectory => {
            if let Some(path) = pane.explorer.take_pending_directory() {
                let OverviewState::Ready(overview) = &pane.overview else {
                    return;
                };
                request_explorer_directory(
                    pane.cwd.clone(),
                    overview.identity.clone(),
                    pane.explorer.generation(),
                    path,
                    sender.clone(),
                );
            }
        }
    }
}

fn request_explorer_root(pane: &PaneModel, sender: &mpsc::Sender<PaneMessage>) {
    let OverviewState::Ready(overview) = &pane.overview else {
        return;
    };
    request_explorer_root_with(
        pane.cwd.clone(),
        overview.identity.clone(),
        pane.explorer.generation(),
        sender.clone(),
    );
}

fn request_explorer_root_with(
    cwd: PathBuf,
    identity: WorkspaceIdentity,
    generation: u64,
    sender: mpsc::Sender<PaneMessage>,
) {
    thread::spawn(move || {
        let result = load_explorer_root(&cwd, &identity);
        let _ = sender.send(PaneMessage::ExplorerRoot { generation, result });
    });
}

fn load_explorer_root(
    cwd: &Path,
    identity: &WorkspaceIdentity,
) -> Result<LoadedDirectory, ExplorerRootFailure> {
    let client = P4Client::new(StdProcessTransport, "p4", cwd);
    match cwd_is_in_client_view(&client, cwd) {
        Ok(false) => return Err(ExplorerRootFailure::NotInView),
        Err(error) => return Err(ExplorerRootFailure::Failed(error.to_string())),
        Ok(true) => {}
    }
    load_explorer_directory(&client, identity, cwd).map_err(|error| match error {
        ExplorerError::Query(source) if source.kind == crate::p4::P4ErrorKind::NotInClientView => {
            ExplorerRootFailure::NotInView
        }
        other => ExplorerRootFailure::Failed(other.to_string()),
    })
}

fn request_explorer_directory(
    cwd: PathBuf,
    identity: WorkspaceIdentity,
    generation: u64,
    path: PathBuf,
    sender: mpsc::Sender<PaneMessage>,
) {
    thread::spawn(move || {
        let client = P4Client::new(StdProcessTransport, "p4", &cwd);
        let result = load_explorer_directory(&client, &identity, &path);
        let _ = sender.send(PaneMessage::ExplorerDirectory {
            generation,
            path,
            result,
        });
    });
}

fn request_overview(cwd: PathBuf, generation: u64, sender: mpsc::Sender<PaneMessage>) {
    thread::spawn(move || {
        let result = load_overview(&cwd);
        let _ = sender.send(PaneMessage::Overview { generation, result });
    });
}

fn load_overview(cwd: &Path) -> Result<WorkspaceOverview, OverviewFailure> {
    let client = P4Client::new(StdProcessTransport, "p4", cwd);
    let info = client.run(&P4Query::Info).map_err(OverviewFailure::Query)?;
    let identity = match workspace_owning_cwd(cwd, &info.records) {
        Ok(identity) => identity,
        Err(WorkspaceCwdError::Mapping(error)) => return Err(OverviewFailure::Mapping(error)),
        Err(WorkspaceCwdError::Query(error)) => return Err(OverviewFailure::Query(error)),
    };
    let changes = client
        .run(&P4Query::PendingChangesLimited {
            user: identity.user.clone(),
            client: identity.client.clone(),
            max_results: MAX_VISIBLE_CHANGELISTS,
        })
        .map_err(OverviewFailure::Query)?;
    let changes =
        pending_changelists_from_changes(&changes.records, &identity.user, &identity.client)
            .map_err(OverviewFailure::Mapping)?;
    Ok(WorkspaceOverview { identity, changes })
}

fn dispatch_overlay<T: P4Transport + 'static>(
    request: SubmitOverlayRequest,
    service: &Arc<P4WriteService<T>>,
    cwd: &Path,
    sender: &mpsc::Sender<PaneMessage>,
) {
    match request {
        SubmitOverlayRequest::Preflight { change, request_id } => {
            let service = Arc::clone(service);
            let sender = sender.clone();
            thread::spawn(move || {
                let result = service.preview_submit(change);
                let _ = sender.send(PaneMessage::Preflight {
                    change,
                    request_id,
                    result: Box::new(result),
                });
            });
        }
        SubmitOverlayRequest::Execute {
            change,
            authorization,
        } => {
            let service = Arc::clone(service);
            let cwd = cwd.to_path_buf();
            let sender = sender.clone();
            thread::spawn(move || match SubmitProvider::load_from_environment() {
                SubmitProvider::Native => {
                    let result = service.submit_change(authorization);
                    let _ = sender.send(PaneMessage::Submit { change, result });
                }
                SubmitProvider::External(provider) => {
                    if let Err(error) = service.prepare_external_handoff(authorization) {
                        let _ = sender.send(PaneMessage::ExternalHandoff {
                            change,
                            result: ExternalHandoffResult::ValidationFailed(error),
                        });
                        return;
                    }
                    let result = match provider.launch(change, &cwd) {
                        Ok(()) => ExternalHandoffResult::Launched(provider.label().to_owned()),
                        Err(ExternalLaunchError::StartFailed) => {
                            ExternalHandoffResult::LaunchFailed(format!(
                                "{} could not be started for CL {change}. No p4 submit was run.",
                                provider.label()
                            ))
                        }
                        Err(ExternalLaunchError::InvalidConfiguration(detail)) => {
                            ExternalHandoffResult::LaunchFailed(detail)
                        }
                    };
                    let _ = sender.send(PaneMessage::ExternalHandoff { change, result });
                }
                SubmitProvider::Invalid(detail) => {
                    let _ = sender.send(PaneMessage::ExternalHandoff {
                            change,
                            result: ExternalHandoffResult::LaunchFailed(format!(
                                "Submit provider configuration is invalid: {detail}. No external tool or p4 submit was started."
                            )),
                        });
                }
            });
        }
        SubmitOverlayRequest::Reconcile {
            receipt,
            request_id,
        } => {
            let change = receipt.change();
            let service = Arc::clone(service);
            let sender = sender.clone();
            thread::spawn(move || {
                let result = service.reconcile_submit(&receipt);
                let _ = sender.send(PaneMessage::Reconcile {
                    change,
                    request_id,
                    result,
                });
            });
        }
    }
}

fn dispatch_description_load<T: P4Transport + 'static>(
    service: Arc<P4WriteService<T>>,
    sender: mpsc::Sender<PaneMessage>,
    change: u64,
    request_id: u64,
) {
    thread::spawn(move || {
        let result = service.load_description_for_edit(change);
        let _ = sender.send(PaneMessage::DescriptionLoaded {
            change,
            request_id,
            result,
        });
    });
}

fn dispatch_description_apply<T: P4Transport + 'static>(
    service: Arc<P4WriteService<T>>,
    sender: mpsc::Sender<PaneMessage>,
    change: u64,
    description: String,
    request_id: u64,
    intent: DescriptionApplyIntent,
) {
    thread::spawn(move || {
        let result = service
            .preview_description_apply(change, description)
            .and_then(|preview| {
                let authorization = preview
                    .authorize(intent)
                    .expect("Description Apply dispatch requires explicit write intent");
                service.apply_description(authorization)
            });
        let _ = sender.send(PaneMessage::DescriptionApplied {
            change,
            request_id,
            result,
        });
    });
}

#[derive(Debug, Clone, Copy, Default)]
struct Rect {
    x: u16,
    y: u16,
    width: u16,
    height: u16,
}

impl Rect {
    fn row(x: u16, y: u16, width: u16) -> Self {
        Self {
            x,
            y,
            width,
            height: 1,
        }
    }

    fn area(x: u16, y: u16, width: u16, height: u16) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    fn contains(self, x: u16, y: u16) -> bool {
        let height = self.height.max(1);
        y >= self.y
            && y < self.y.saturating_add(height)
            && x >= self.x
            && x < self.x.saturating_add(self.width)
    }
}

#[derive(Debug, Default)]
struct HitTargets {
    cancel: Option<Rect>,
    submit: Option<Rect>,
    refresh: Option<Rect>,
    changelists: Vec<Rect>,
    changelist_offset: usize,
    review_submit: Option<Rect>,
    description_edit: Option<Rect>,
    description_review: Option<Rect>,
    description_cancel: Option<Rect>,
    description_apply: Option<Rect>,
    view_explorer: Option<Rect>,
    view_review: Option<Rect>,
    explorer_rows: Vec<Rect>,
    explorer_offset: usize,
}

#[derive(Debug, Default)]
struct RenderedFrame {
    lines: Vec<String>,
    hits: HitTargets,
    row_styles: Vec<RowStyle>,
    cursor: Option<(u16, u16)>,
}

#[derive(Debug, Clone, Copy, Default)]
struct RowStyle {
    foreground: Option<Color>,
    background: Option<Color>,
    bold: bool,
}

fn render_frame(pane: &PaneModel, width: u16, height: u16) -> RenderedFrame {
    let width = width.max(1) as usize;
    let height = height.max(1) as usize;
    let mut frame = RenderedFrame {
        lines: vec![" ".repeat(width); height],
        hits: HitTargets::default(),
        row_styles: vec![RowStyle::default(); height],
        cursor: None,
    };
    render_view_tabs(&mut frame, pane.view, width);
    render_header(&mut frame, pane, width);

    match pane.view {
        PaneView::Explorer => render_explorer(&mut frame, pane, width, height),
        PaneView::Review => render_review(&mut frame, pane, width, height),
    }
    if height >= 1 {
        put_display(&mut frame.lines, 0, height - 1, &pane.status);
    }

    render_nav_overlay(&mut frame, pane, width, height);

    if !matches!(pane.overlay.state(), SubmitOverlayState::Closed) {
        render_overlay(
            &mut frame,
            pane.overlay.state(),
            &pane.submit_provider,
            width,
            height,
        );
    }
    if pane.nav_overlay.is_open() || !matches!(pane.overlay.state(), SubmitOverlayState::Closed) {
        // Overlays are rendered after the underlying rows. Row-wide Explorer
        // and selection colors must not bleed through a menu or confirmation.
        frame.row_styles.fill(RowStyle::default());
        frame.cursor = None;
    }
    for line in &mut frame.lines {
        *line = pad_display(line, width);
    }
    frame
}

fn render_view_tabs(frame: &mut RenderedFrame, view: PaneView, _width: usize) {
    let explorer = if view == PaneView::Explorer {
        "▸ [📁 Explorer]"
    } else {
        "  📁 Explorer "
    };
    let review = if view == PaneView::Review {
        "▸ [P4 Review]"
    } else {
        "  P4 Review "
    };
    let gap = "  ";
    put_display(&mut frame.lines, 0, 0, &format!("{explorer}{gap}{review}"));
    let explorer_width = display_width(explorer) as u16;
    frame.hits.view_explorer = Some(Rect::row(0, 0, explorer_width));
    frame.hits.view_review = Some(Rect::row(
        explorer_width + display_width(gap) as u16,
        0,
        display_width(review) as u16,
    ));
}

fn render_header(frame: &mut RenderedFrame, pane: &PaneModel, _width: usize) {
    let header = match pane.view {
        PaneView::Explorer => pane
            .cwd
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| pane.cwd.display().to_string())
            .to_uppercase(),
        PaneView::Review => match &pane.overview {
            OverviewState::Ready(overview) => {
                format!("{} · {}", overview.identity.client, overview.identity.user)
            }
            OverviewState::Loading => "Loading workspace…".to_owned(),
            OverviewState::Failed(_) => "Workspace unavailable".to_owned(),
        },
    };
    put_display(&mut frame.lines, 1, 1, &header);
}

fn render_review(frame: &mut RenderedFrame, pane: &PaneModel, width: usize, height: usize) {
    if height >= 2 {
        put_display(&mut frame.lines, 0, height - 2, review_help(pane));
    }
    match &pane.overview {
        OverviewState::Loading => put_display(&mut frame.lines, 0, 2, "Loading changelists..."),
        OverviewState::Failed(message) => {
            put_display(&mut frame.lines, 0, 2, "Workspace refresh failed");
            put_display(&mut frame.lines, 0, 3, message);
        }
        OverviewState::Ready(overview) => {
            render_review_composer(frame, pane, width);
            if height >= REVIEW_LIST_TOP + REVIEW_BOTTOM_ROWS {
                let file_history_row = height - 4;
                let workspace_history_row = height - 3;
                put_display(
                    &mut frame.lines,
                    0,
                    file_history_row,
                    "▸ FILE HISTORY  (coming soon)",
                );
                put_display(
                    &mut frame.lines,
                    0,
                    workspace_history_row,
                    "▸ WORKSPACE HISTORY  (coming soon)",
                );
                for row in [file_history_row, workspace_history_row] {
                    if let Some(style) = frame.row_styles.get_mut(row) {
                        style.foreground = Some(Color::DarkGrey);
                    }
                }
            }

            let body_top = REVIEW_LIST_TOP;
            let body_height = height.saturating_sub(REVIEW_LIST_TOP + REVIEW_BOTTOM_ROWS);
            if body_height == 0 {
                return;
            }
            let offset = pane.review_offset(body_height);
            let visible = body_height.min(overview.changes.len().saturating_sub(offset));
            frame.hits.changelist_offset = offset;
            let text_width = width.saturating_sub(1);
            let max_scroll_x = pane.review_content_width().saturating_sub(text_width);
            let scroll_x = pane.review_scroll_x.min(max_scroll_x);
            for (visible_index, change) in overview
                .changes
                .iter()
                .skip(offset)
                .take(visible)
                .enumerate()
            {
                let index = offset + visible_index;
                let row = body_top + visible_index;
                let line = format_changelist_row(index == pane.selected, change);
                let shown = slice_display(&line, scroll_x, text_width);
                put_display(&mut frame.lines, 0, row, &shown);
                if index == pane.selected
                    && let Some(style) = frame.row_styles.get_mut(row)
                {
                    style.background = Some(Color::DarkGrey);
                    style.bold = true;
                }
                frame
                    .hits
                    .changelists
                    .push(Rect::row(0, row as u16, text_width as u16));
            }
            draw_scrollbar(
                &mut frame.lines,
                width.saturating_sub(1),
                body_top,
                body_height,
                overview.changes.len(),
                body_height,
                offset,
            );
        }
    }
}

fn render_review_composer(frame: &mut RenderedFrame, pane: &PaneModel, width: usize) {
    let Some(change) = pane.selected_changelist() else {
        put_display(&mut frame.lines, 0, REVIEW_SECTION_ROW, "▾ CHANGELISTS (0)");
        return;
    };
    let numbered_change = match change.id {
        ChangelistId::Numbered(change) => Some(change),
        ChangelistId::Default => None,
    };
    let editor_label = match (&pane.description_editor, numbered_change) {
        (DescriptionEditor::Loading { change, .. }, Some(selected)) if *change == selected => {
            " · LOADING FULL TEXT"
        }
        (DescriptionEditor::Editing { change, .. }, Some(selected)) if *change == selected => {
            " · EDITING"
        }
        (DescriptionEditor::Applying { change, .. }, Some(selected)) if *change == selected => {
            " · APPLYING"
        }
        _ => "",
    };
    let title = format!(" CL {} DESCRIPTION{editor_label} ", change.id);
    let inner_width = width.saturating_sub(2);
    let title = slice_display(&title, 0, inner_width);
    let top_fill = "─".repeat(inner_width.saturating_sub(display_width(&title)));
    put_display(&mut frame.lines, 0, 2, &format!("┌{title}{top_fill}┐"));

    let (description, cursor, viewport_cursor) =
        numbered_change.map_or((change.description.as_str(), None, None), |id| {
            pane.description_editor
                .text_for_change(id, &change.description)
        });
    let description = if description.is_empty() && cursor.is_none() {
        "<no description>"
    } else {
        description
    };
    let content_width = width.saturating_sub(4).max(1);
    let DescriptionTextLayout {
        lines,
        cursor_line,
        cursor_column,
    } = layout_description_text(description, viewport_cursor.unwrap_or(0), content_width);
    let first_visual_line = viewport_cursor.map_or(0, |_| {
        cursor_line.saturating_sub(REVIEW_DESCRIPTION_ROWS.saturating_sub(1))
    });
    for visible_line in 0..REVIEW_DESCRIPTION_ROWS {
        let content = lines
            .get(first_visual_line + visible_line)
            .map_or("", String::as_str);
        let content = pad_display(content, content_width);
        put_display(
            &mut frame.lines,
            0,
            REVIEW_DESCRIPTION_FIRST_ROW + visible_line,
            &format!("│ {content} │"),
        );
    }
    put_display(
        &mut frame.lines,
        0,
        REVIEW_DESCRIPTION_FIRST_ROW + REVIEW_DESCRIPTION_ROWS,
        &format!("└{}┘", "─".repeat(inner_width)),
    );
    for row in 2..=REVIEW_DESCRIPTION_FIRST_ROW + REVIEW_DESCRIPTION_ROWS {
        if let Some(style) = frame.row_styles.get_mut(row) {
            style.foreground = Some(if cursor.is_some() {
                Color::Yellow
            } else {
                Color::Cyan
            });
        }
    }
    if cursor.is_some()
        && cursor_line >= first_visual_line
        && cursor_line < first_visual_line + REVIEW_DESCRIPTION_ROWS
    {
        let cursor_x = 2usize
            .saturating_add(cursor_column)
            .min(width.saturating_sub(2));
        let cursor_y = REVIEW_DESCRIPTION_FIRST_ROW + cursor_line - first_visual_line;
        if cursor_y < frame.lines.len() {
            frame.cursor = Some((cursor_x as u16, cursor_y as u16));
        }
    }

    let editable = pane.selected_editable_description().is_ok();
    if editable {
        frame.hits.description_edit = Some(Rect::area(
            0,
            2,
            width as u16,
            (REVIEW_DESCRIPTION_ROWS + 3) as u16,
        ));
    }
    render_description_actions(frame, pane, numbered_change, editable, width);

    let submit_label = match change.id {
        ChangelistId::Numbered(change) => format!("  ✓ Review & Submit CL {change}  "),
        ChangelistId::Default => "  Submit unavailable for default CL  ".to_owned(),
    };
    let submit = slice_display(&submit_label, 0, width.saturating_sub(1));
    put_display(&mut frame.lines, 0, REVIEW_SUBMIT_ROW, &submit);
    if matches!(change.id, ChangelistId::Numbered(_)) {
        frame.hits.review_submit = Some(Rect::row(0, REVIEW_SUBMIT_ROW as u16, width as u16));
        if let Some(style) = frame.row_styles.get_mut(REVIEW_SUBMIT_ROW) {
            style.foreground = Some(Color::White);
            style.background = Some(Color::Blue);
            style.bold = true;
        }
    } else if let Some(style) = frame.row_styles.get_mut(REVIEW_SUBMIT_ROW) {
        style.foreground = Some(Color::DarkGrey);
    }

    let count = pane.review_count();
    put_display(
        &mut frame.lines,
        0,
        REVIEW_SECTION_ROW,
        &format!("▾ CHANGELISTS ({count})"),
    );
    if let Some(style) = frame.row_styles.get_mut(REVIEW_SECTION_ROW) {
        style.bold = true;
    }
}

fn render_description_actions(
    frame: &mut RenderedFrame,
    pane: &PaneModel,
    selected_change: Option<u64>,
    editable: bool,
    width: usize,
) {
    let row = REVIEW_DESCRIPTION_ACTION_ROW;
    match (&pane.description_editor, selected_change) {
        (DescriptionEditor::Idle, _) if editable => {
            put_display(&mut frame.lines, 2, row, "[Edit Description]");
            put_display(&mut frame.lines, 23, row, "click or e");
        }
        (DescriptionEditor::Editing { change, .. }, Some(selected)) if *change == selected => {
            let (apply, next) = put_description_button(frame, row, 2, "[Apply Description]", width);
            frame.hits.description_review = Some(apply);
            let (cancel, _) =
                put_description_button(frame, row, next.saturating_add(2), "[Cancel]", width);
            frame.hits.description_cancel = Some(cancel);
        }
        (DescriptionEditor::Loading { change, .. }, Some(selected)) if *change == selected => {
            put_display(&mut frame.lines, 2, row, "Loading full description...");
            let (cancel, _) = put_description_button(frame, row, 24, "[Cancel]", width);
            frame.hits.description_cancel = Some(cancel);
        }
        (DescriptionEditor::Applying { change, .. }, Some(selected)) if *change == selected => {
            put_display(
                &mut frame.lines,
                2,
                row,
                "Applying and verifying description...",
            );
        }
        _ if !editable => {
            put_display(&mut frame.lines, 2, row, "Description editing unavailable");
        }
        _ => {}
    }
}

fn put_description_button(
    frame: &mut RenderedFrame,
    row: usize,
    x: usize,
    label: &str,
    width: usize,
) -> (Rect, usize) {
    let shown = slice_display(label, 0, width.saturating_sub(x));
    put_display(&mut frame.lines, x, row, &shown);
    let shown_width = display_width(&shown);
    (
        Rect::row(x as u16, row as u16, shown_width as u16),
        x.saturating_add(shown_width),
    )
}

fn render_explorer(frame: &mut RenderedFrame, pane: &PaneModel, width: usize, height: usize) {
    if height >= 3 {
        let text_width = width.saturating_sub(1);
        let legend = explorer_legend();
        let scroll_x = pane
            .explorer
            .scroll_x()
            .min(display_width(legend).saturating_sub(text_width));
        let shown = slice_display(legend, scroll_x, text_width);
        put_display(&mut frame.lines, 0, height - 3, &shown);
    }
    if height >= 2 {
        put_display(&mut frame.lines, 0, height - 2, explorer_help(pane));
    }
    match pane.explorer.load_state() {
        ExplorerLoadState::Idle | ExplorerLoadState::Checking => {
            put_display(&mut frame.lines, 0, 2, "Loading workspace files...");
        }
        ExplorerLoadState::NotInClientView => {
            put_display(
                &mut frame.lines,
                0,
                2,
                "Workspace is not in the current client view",
            );
            put_display(&mut frame.lines, 0, 3, connection_message());
        }
        ExplorerLoadState::Failed(message) => {
            put_display(&mut frame.lines, 0, 2, "Explorer refresh failed");
            put_display(&mut frame.lines, 0, 3, message);
        }
        ExplorerLoadState::Ready => {
            let body_top = 2;
            let body_height = height.saturating_sub(NAV_CHROME_ROWS);
            if body_height == 0 {
                return;
            }
            render_tree_column(frame, pane, 0, body_top, width, body_height);
        }
    }
}

fn explorer_help(_pane: &PaneModel) -> &'static str {
    "Enter open  d diff  m menu  r refresh  1/2 views  q close"
}

fn explorer_legend() -> &'static str {
    "A add  M edit  D delete  R move  U untracked  ↓ behind  ⊘ not-in-view  ? unmapped"
}

fn review_help(pane: &PaneModel) -> &'static str {
    match &pane.description_editor {
        DescriptionEditor::Editing { .. } => {
            "type/paste edit  Enter newline  Ctrl+Enter apply  Esc cancel"
        }
        DescriptionEditor::Loading { .. } => "loading full description  Esc cancel",
        DescriptionEditor::Applying { .. } => "applying description; input is locked",
        DescriptionEditor::Idle => "e/click edit  ↑↓ select  Enter files  s review  r refresh",
    }
}

fn render_tree_column(
    frame: &mut RenderedFrame,
    pane: &PaneModel,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
) {
    if height == 0 || width == 0 {
        return;
    }
    let text_width = width.saturating_sub(1);
    let (offset, visible, rows) = pane.explorer.tree_window(height);
    frame.hits.explorer_offset = offset;
    let selected = pane.explorer.selected_path();
    let content_width = rows
        .iter()
        .map(|row| {
            display_width(&ExplorerModel::format_row(
                row,
                selected.is_some_and(|path| path == row.path),
            ))
        })
        .max()
        .unwrap_or(0);
    let scroll_x = pane
        .explorer
        .scroll_x()
        .min(content_width.saturating_sub(text_width));
    for (visible_index, row) in rows.iter().skip(offset).take(visible).enumerate() {
        let screen_row = y + visible_index;
        let is_selected = selected.is_some_and(|path| path == row.path);
        let line = ExplorerModel::format_row(row, is_selected);
        let shown = slice_display(&line, scroll_x, text_width);
        put_display(&mut frame.lines, x, screen_row, &shown);
        if let Some(style) = frame.row_styles.get_mut(screen_row) {
            *style = explorer_row_style(row.decoration.as_ref(), is_selected);
        }
        frame
            .hits
            .explorer_rows
            .push(Rect::row(x as u16, screen_row as u16, text_width as u16));
    }
    draw_scrollbar(
        &mut frame.lines,
        width.saturating_sub(1),
        y,
        height,
        rows.len(),
        height,
        offset,
    );
}

fn explorer_row_style(decoration: Option<&ExplorerDecoration>, selected: bool) -> RowStyle {
    let foreground = match decoration {
        Some(ExplorerDecoration::Untracked) => Some(Color::Rgb {
            r: 0x73,
            g: 0xc9,
            b: 0x91,
        }),
        Some(ExplorerDecoration::Opened { action, .. }) => match action {
            FileAction::Add | FileAction::Branch => Some(Color::Rgb {
                r: 0x81,
                g: 0xb8,
                b: 0x8b,
            }),
            FileAction::Edit | FileAction::Integrate | FileAction::Import => Some(Color::Rgb {
                r: 0xe2,
                g: 0xc0,
                b: 0x8d,
            }),
            FileAction::Delete | FileAction::Purge | FileAction::Archive => Some(Color::Rgb {
                r: 0xc7,
                g: 0x4e,
                b: 0x39,
            }),
            FileAction::MoveAdd | FileAction::MoveDelete => Some(Color::Rgb {
                r: 0x73,
                g: 0xc9,
                b: 0x91,
            }),
            FileAction::Unknown(_) => Some(Color::Grey),
        },
        Some(ExplorerDecoration::OutOfDate) => Some(Color::Cyan),
        Some(ExplorerDecoration::NotInView | ExplorerDecoration::Unmapped) => Some(Color::DarkGrey),
        Some(ExplorerDecoration::Unopened) | None => None,
    };
    RowStyle {
        foreground,
        background: selected.then_some(Color::DarkGrey),
        bold: selected,
    }
}

fn draw_scrollbar(
    lines: &mut [String],
    x: usize,
    y: usize,
    height: usize,
    total: usize,
    viewport: usize,
    pos: usize,
) {
    if total <= viewport || height == 0 {
        return;
    }
    let thumb = ((viewport * height) / total).max(1).min(height);
    let travel = height.saturating_sub(thumb);
    let max_pos = total.saturating_sub(viewport);
    let thumb_pos = (pos * travel).checked_div(max_pos).unwrap_or(0);
    for row in 0..height {
        let glyph = if row >= thumb_pos && row < thumb_pos + thumb {
            "┃"
        } else {
            "│"
        };
        put_display(lines, x, y + row, glyph);
    }
}

fn render_nav_overlay(frame: &mut RenderedFrame, pane: &PaneModel, width: usize, height: usize) {
    match &pane.nav_overlay {
        NavOverlay::Closed => {}
        NavOverlay::Menu {
            x,
            y,
            entries,
            selected,
            ..
        } => {
            let rect = menu_popup_rect(*x, *y, entries, width as u16, height as u16);
            let inner_width = rect.width.saturating_sub(2) as usize;
            let inner_height = usize::from(rect.height.saturating_sub(2));
            let offset = menu_window(*selected, entries.len(), inner_height);
            let horizontal = "─".repeat(inner_width.max(1));
            put_display(
                &mut frame.lines,
                rect.x as usize,
                rect.y as usize,
                &format!("┌{horizontal}┐"),
            );
            for (visible_index, (index, entry)) in entries
                .iter()
                .enumerate()
                .skip(offset)
                .take(inner_height)
                .enumerate()
            {
                let row = rect.y as usize + 1 + visible_index;
                let content = match entry {
                    MenuEntry::Separator => "─".repeat(inner_width.max(1)),
                    MenuEntry::Action(_, label) => {
                        let marker = if index == *selected { ">" } else { " " };
                        format!("{marker} {label}")
                    }
                };
                put_display(
                    &mut frame.lines,
                    rect.x as usize,
                    row,
                    &format!("│{}│", pad_display(&content, inner_width)),
                );
            }
            let bottom = rect.y.saturating_add(rect.height).saturating_sub(1) as usize;
            if bottom < height {
                put_display(
                    &mut frame.lines,
                    rect.x as usize,
                    bottom,
                    &format!("└{horizontal}┘"),
                );
            }
        }
        NavOverlay::Prompt { title, input, .. } => {
            put_display(
                &mut frame.lines,
                0,
                height.saturating_sub(2),
                &format!(" {title}: {input}█  (Enter ok · Esc cancel)"),
            );
        }
        NavOverlay::ConfirmDelete { path, .. } => {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            put_display(
                &mut frame.lines,
                0,
                height.saturating_sub(2),
                &format!(" Delete '{name}' permanently? (y/N)"),
            );
        }
    }
}

fn render_overlay(
    frame: &mut RenderedFrame,
    state: &SubmitOverlayState,
    provider: &SubmitProvider,
    screen_width: usize,
    screen_height: usize,
) {
    let provider_label = provider.label();
    let native_submit = provider.is_native();
    let overlay_width = screen_width
        .saturating_sub(4)
        .clamp(28, 72)
        .min(screen_width);
    let overlay_height = 16.min(screen_height);
    let left = screen_width.saturating_sub(overlay_width) / 2;
    let top = screen_height.saturating_sub(overlay_height) / 2;
    let inner_width = overlay_width.saturating_sub(4);

    let border = if overlay_width >= 2 {
        format!("+{}+", "-".repeat(overlay_width - 2))
    } else {
        "+".to_owned()
    };
    put(&mut frame.lines, left, top, &border);
    for row in 1..overlay_height.saturating_sub(1) {
        let body = if overlay_width >= 2 {
            format!("|{}|", " ".repeat(overlay_width - 2))
        } else {
            "|".to_owned()
        };
        put(&mut frame.lines, left, top + row, &body);
    }
    if overlay_height >= 2 {
        put(&mut frame.lines, left, top + overlay_height - 1, &border);
    }

    let x = left.saturating_add(2);
    match state {
        SubmitOverlayState::Preflight { change, .. } => {
            put(&mut frame.lines, x, top + 2, &format!("Submit CL {change}"));
            put(
                &mut frame.lines,
                x,
                top + 4,
                "Running read-only preflight...",
            );
            put(
                &mut frame.lines,
                x,
                top + 6,
                "Esc: cancel review (never submits)",
            );
        }
        SubmitOverlayState::Review { preview } => {
            put(
                &mut frame.lines,
                x,
                top + 1,
                &format!("Submit CL {}", preview.change),
            );
            put(
                &mut frame.lines,
                x,
                top + 2,
                &truncate_chars(
                    &format!(
                        "Server: {}   User: {}   Client: {}",
                        preview.server_id(),
                        preview.user(),
                        preview.client()
                    ),
                    inner_width,
                ),
            );
            put(
                &mut frame.lines,
                x,
                top + 3,
                &format!(
                    "{} file(s)   {}",
                    preview.file_count,
                    action_summary(&preview.actions)
                ),
            );
            put(
                &mut frame.lines,
                x,
                top + 4,
                &truncate_chars(
                    preview.description.lines().next().unwrap_or_default(),
                    inner_width,
                ),
            );
            put(
                &mut frame.lines,
                x,
                top + 6,
                &truncate_chars(&format!("Provider: {provider_label}"), inner_width),
            );
            put(
                &mut frame.lines,
                x,
                top + 7,
                "[ok] Description and ownership",
            );
            put(
                &mut frame.lines,
                x,
                top + 8,
                "[ok] Files mapped; no unresolved/out-of-date files",
            );
            put(
                &mut frame.lines,
                x,
                top + 10,
                if native_submit {
                    "Enter/Esc: Cancel   Ctrl+Enter: Submit"
                } else {
                    "Enter/Esc: Cancel   Ctrl+Enter: Open provider"
                },
            );
            let cancel_text = ">[Cancel]<";
            let submit_text = if native_submit {
                "[Submit]"
            } else {
                "[Open provider]"
            };
            let button_row = top + overlay_height.saturating_sub(3);
            let cancel_x = x;
            let submit_x = x + cancel_text.len() + 3;
            put(&mut frame.lines, cancel_x, button_row, cancel_text);
            put(&mut frame.lines, submit_x, button_row, submit_text);
            frame.hits.cancel = Some(Rect::row(
                cancel_x as u16,
                button_row as u16,
                cancel_text.len() as u16,
            ));
            frame.hits.submit = Some(Rect::row(
                submit_x as u16,
                button_row as u16,
                submit_text.len() as u16,
            ));
        }
        SubmitOverlayState::Running { change, .. } => {
            put(
                &mut frame.lines,
                x,
                top + 2,
                &format!(
                    "{} CL {change}...",
                    if native_submit {
                        "Submitting"
                    } else {
                        "Opening external provider for"
                    }
                ),
            );
            put(
                &mut frame.lines,
                x,
                top + 4,
                if native_submit {
                    "Exactly one p4 submit process is running."
                } else {
                    "Herdr will not run p4 submit for this handoff."
                },
            );
            put(
                &mut frame.lines,
                x,
                top + 6,
                "The pane cannot cancel or start another submit now.",
            );
        }
        SubmitOverlayState::Reconciling { change, .. } => {
            put(
                &mut frame.lines,
                x,
                top + 2,
                &format!("Reconciling CL {change}..."),
            );
            put(
                &mut frame.lines,
                x,
                top + 4,
                "Running read-only info and describe queries.",
            );
            put(
                &mut frame.lines,
                x,
                top + 6,
                "No submit retry is being performed.",
            );
        }
        SubmitOverlayState::Failure {
            change, failure, ..
        } => {
            put(&mut frame.lines, x, top + 1, failure.title());
            put(&mut frame.lines, x, top + 2, &format!("CL {change}"));
            let detail = wrap_words(&failure.detail, inner_width);
            for (offset, line) in detail.iter().take(3).enumerate() {
                put(&mut frame.lines, x, top + 4 + offset, line);
            }
            let next = wrap_words(failure.next_step, inner_width);
            for (offset, line) in next.iter().take(3).enumerate() {
                put(&mut frame.lines, x, top + 8 + offset, line);
            }
            let refresh_text = if failure.certainty == SubmitOutcomeCertainty::Unknown {
                "[Read-only reconcile]"
            } else {
                "[Refresh preflight]"
            };
            let button_row = top + overlay_height.saturating_sub(3);
            let refresh_x = if failure.certainty == SubmitOutcomeCertainty::Unknown {
                x
            } else {
                put(&mut frame.lines, x, button_row, "[Close]");
                frame.hits.cancel = Some(Rect::row(x as u16, button_row as u16, 7));
                x + 10
            };
            put(&mut frame.lines, refresh_x, button_row, refresh_text);
            frame.hits.refresh = Some(Rect::row(
                refresh_x as u16,
                button_row as u16,
                refresh_text.len() as u16,
            ));
        }
        SubmitOverlayState::Success { result, reconciled } => {
            put(&mut frame.lines, x, top + 2, "Submit confirmed");
            put(
                &mut frame.lines,
                x,
                top + 4,
                &format!(
                    "CL {} submitted with {} file(s).",
                    result.submitted_change, result.file_count
                ),
            );
            if *reconciled {
                put(
                    &mut frame.lines,
                    x,
                    top + 6,
                    "Confirmed by read-only reconciliation after an uncertain result.",
                );
            }
            put(&mut frame.lines, x, top + 9, "Enter/Esc: close result");
        }
        SubmitOverlayState::Closed => {}
    }
}

#[allow(dead_code)]
fn changelist_window(selected: usize, count: usize, visible_rows: usize) -> (usize, usize) {
    if visible_rows == 0 || count == 0 {
        return (0, 0);
    }
    let visible_rows = visible_rows.min(count);
    let offset = selected
        .saturating_add(1)
        .saturating_sub(visible_rows)
        .min(count.saturating_sub(visible_rows));
    (offset, visible_rows)
}

fn action_summary(actions: &crate::p4::SubmitActionCounts) -> String {
    let values = [
        ("add", actions.adds),
        ("edit", actions.edits),
        ("delete", actions.deletes),
        ("branch", actions.branches),
        ("move/add", actions.move_adds),
        ("move/delete", actions.move_deletes),
        ("integrate", actions.integrates),
        ("import", actions.imports),
    ];
    let summary = values
        .into_iter()
        .filter(|(_, count)| *count > 0)
        .map(|(name, count)| format!("{name}:{count}"))
        .collect::<Vec<_>>()
        .join(" ");
    if summary.is_empty() {
        "no actions".to_owned()
    } else {
        summary
    }
}

fn put_display(lines: &mut [String], column: usize, row: usize, value: &str) {
    let Some(line) = lines.get_mut(row) else {
        return;
    };
    *line = splice_display(line, column, value);
}

fn put(lines: &mut [String], column: usize, row: usize, value: &str) {
    let Some(line) = lines.get_mut(row) else {
        return;
    };
    let mut characters = line.chars().collect::<Vec<_>>();
    for (offset, character) in value.chars().enumerate() {
        let index = column + offset;
        if index >= characters.len() {
            break;
        }
        characters[index] = character;
    }
    *line = characters.into_iter().collect();
}

#[allow(dead_code)]
fn truncate_and_pad(value: &str, width: usize) -> String {
    let mut output = truncate_chars(value, width);
    let count = output.chars().count();
    if count < width {
        output.push_str(&" ".repeat(width - count));
    }
    output
}

fn truncate_chars(value: &str, width: usize) -> String {
    value.chars().take(width).collect()
}

fn wrap_words(value: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in value.split_whitespace() {
        let next_len = line.chars().count() + usize::from(!line.is_empty()) + word.chars().count();
        if next_len > width && !line.is_empty() {
            lines.push(line);
            line = String::new();
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use crate::{
        app::{SubmitFailure, SubmitFault},
        domain::{CaseHandling, ChangelistStatus},
        p4::LoadedDirectory,
    };

    fn pane_with_changes() -> PaneModel {
        let mut pane = PaneModel::new(PathBuf::from("C:/Example Workspace"));
        pane.install_overview(Ok(WorkspaceOverview {
            identity: WorkspaceIdentity {
                server_id: "server".into(),
                user: "ExampleUser".into(),
                client: "ExampleClient".into(),
                root: PathBuf::from("C:/Example Workspace"),
                stream: None,
                case_handling: CaseHandling::Insensitive,
            },
            changes: vec![Changelist {
                id: ChangelistId::Numbered(42),
                status: ChangelistStatus::Pending,
                owner: "ExampleUser".into(),
                client: "ExampleClient".into(),
                description: "Fix submit overlay".into(),
                files: Vec::new(),
                preserved_spec_fields: Default::default(),
                spec_token: None,
                content_token: None,
            }],
        }));
        pane
    }

    #[test]
    fn pane_opens_on_file_explorer() {
        let pane = pane_with_changes();
        assert_eq!(pane.view, PaneView::Explorer);
        let rendered = render_frame(&pane, 80, 24).lines.join("\n");
        assert!(rendered.contains("Explorer"));
        assert!(rendered.contains("▸ [📁 Explorer]"));
        assert!(rendered.contains("Enter open  d diff  m menu  r refresh"));
        assert!(rendered.contains("A add  M edit  D delete"));
        assert!(!rendered.contains("CL 42  pending"));
    }

    #[test]
    fn narrow_explorer_legend_keeps_full_labels_and_pans() {
        let mut pane = pane_with_changes();
        pane.set_nav_size(24, 16);
        let initial = render_frame(&pane, 24, 16);
        assert!(initial.lines[13].contains("A add"));
        assert!(!initial.lines[13].contains("A+"));

        pane.explorer
            .set_scroll_x(usize::MAX, pane.body_width(), pane.explorer_content_width());
        let panned = render_frame(&pane, 24, 16);
        assert!(panned.lines[13].contains("? unmapped"));
    }

    #[test]
    fn manual_explorer_refresh_invalidates_cached_disk_rows() {
        let mut pane = pane_with_changes();
        let root = PathBuf::from("C:/Example Workspace");
        pane.explorer
            .install_ready_listing_for_test(LoadedDirectory {
                path: root.clone(),
                entries: vec![crate::domain::ExplorerEntry {
                    name: "deleted-externally.txt".into(),
                    path: root.join("deleted-externally.txt"),
                    kind: crate::domain::ExplorerEntryKind::File,
                    decoration: Some(crate::domain::ExplorerDecoration::Unopened),
                    file_type: Some(crate::domain::FileType::new("text")),
                    have_rev: Some(1),
                    head_rev: Some(1),
                }],
                truncated: false,
            });
        pane.explorer.select_index(1);
        let previous_generation = pane.explorer.generation();

        assert_eq!(pane.begin_active_refresh(), MessageEffect::LoadExplorerRoot);
        assert_eq!(pane.explorer.load_state(), &ExplorerLoadState::Checking);
        assert_ne!(pane.explorer.generation(), previous_generation);
        assert_eq!(pane.explorer.pending_directory(), Some(root.as_path()));
        assert!(
            pane.explorer
                .visible_rows()
                .iter()
                .all(|row| row.name != "deleted-externally.txt")
        );
        assert!(pane.status.contains("Refreshing Explorer"));
    }

    #[test]
    fn pane_snapshot_keeps_submit_as_review_not_direct_write() {
        let mut pane = pane_with_changes();
        pane.view = PaneView::Review;
        let frame = render_frame(&pane, 80, 24);
        let rendered = frame.lines.join("\n");
        assert!(rendered.contains("Submit"));
        assert!(rendered.contains("CL 42 DESCRIPTION"));
        assert!(rendered.contains("CL 42  pending  Fix submit overlay"));
        assert!(rendered.contains("FILE HISTORY  (coming soon)"));
        assert!(rendered.contains("WORKSPACE HISTORY  (coming soon)"));
        assert!(frame.hits.review_submit.is_some());
        assert!(!rendered.contains("p4 submit -c"));
    }

    #[test]
    fn review_description_is_a_focusable_multiline_editor() {
        let mut pane = pane_with_numbered_changes(2);
        pane.view = PaneView::Review;

        dispatch_mouse(
            &mut pane,
            mouse(
                MouseEventKind::Down(MouseButton::Left),
                10,
                REVIEW_DESCRIPTION_FIRST_ROW as u16,
            ),
        );
        assert!(matches!(
            pane.description_editor,
            DescriptionEditor::Loading { change: 1, .. }
        ));
        let request_id = pane.description_request_id;
        pane.handle_message(PaneMessage::DescriptionLoaded {
            change: 1,
            request_id,
            result: Ok("Change 1".into()),
        });

        dispatch_key(
            &mut pane,
            KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE),
        );
        dispatch_key(&mut pane, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(pane.handle_paste("粘贴\r\nsecond line"));
        dispatch_key(&mut pane, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));

        let DescriptionEditor::Editing {
            change,
            input,
            cursor,
        } = &pane.description_editor
        else {
            panic!("description editor lost focus");
        };
        assert_eq!(*change, 1);
        assert_eq!(input, "Change 1!\n粘贴\nsecond line");
        assert!(input.is_char_boundary(*cursor));
        assert_eq!(pane.selected, 0, "editor input must not move the CL list");

        let frame = render_frame(&pane, 80, 24);
        let rendered = frame.lines.join("\n");
        assert!(rendered.contains("EDITING"));
        assert!(frame.cursor.is_some(), "focused editor needs a real cursor");
        assert!(frame.hits.description_review.is_some());
        assert!(frame.hits.description_cancel.is_some());
    }

    #[test]
    fn empty_description_editor_does_not_render_placeholder_as_input() {
        let mut pane = pane_with_changes();
        pane.view = PaneView::Review;
        let OverviewState::Ready(overview) = &mut pane.overview else {
            panic!("ready overview");
        };
        overview.changes[0].description.clear();
        pane.description_editor.begin(42, "");

        let frame = render_frame(&pane, 80, 24);
        let description_rows = &frame.lines
            [REVIEW_DESCRIPTION_FIRST_ROW..REVIEW_DESCRIPTION_FIRST_ROW + REVIEW_DESCRIPTION_ROWS];
        assert!(!description_rows.join("\n").contains("<no description>"));
        assert_eq!(frame.cursor, Some((2, REVIEW_DESCRIPTION_FIRST_ROW as u16)));
        dispatch_key(
            &mut pane,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
        );
        assert!(matches!(
            &pane.description_editor,
            DescriptionEditor::Editing { input, .. } if input == "x"
        ));
    }

    #[test]
    fn active_description_editor_blocks_context_menu_selection_and_submit() {
        let mut pane = pane_with_numbered_changes(2);
        pane.view = PaneView::Review;
        pane.description_editor.begin(1, "Change 1");

        dispatch_mouse(
            &mut pane,
            mouse(
                MouseEventKind::Down(MouseButton::Right),
                3,
                (REVIEW_LIST_TOP + 1) as u16,
            ),
        );
        assert_eq!(pane.selected, 0);
        assert!(!pane.nav_overlay.is_open());
        assert!(matches!(
            pane.description_editor,
            DescriptionEditor::Editing { change: 1, .. }
        ));

        dispatch_mouse(
            &mut pane,
            mouse(
                MouseEventKind::Down(MouseButton::Left),
                4,
                REVIEW_SUBMIT_ROW as u16,
            ),
        );
        assert!(matches!(pane.overlay.state(), SubmitOverlayState::Closed));
    }

    #[test]
    fn description_apply_state_keeps_the_editing_viewport() {
        let mut pane = pane_with_changes();
        pane.view = PaneView::Review;
        let input = format!(
            "{}{}{}{}",
            "0".repeat(16),
            "1".repeat(16),
            "2".repeat(16),
            "3333"
        );
        let cursor = input.len();

        pane.description_editor = DescriptionEditor::Applying {
            change: 42,
            input: input.clone(),
            cursor,
            request_id: 7,
        };
        let applying_rows = render_frame(&pane, 20, 24).lines
            [REVIEW_DESCRIPTION_FIRST_ROW..REVIEW_DESCRIPTION_FIRST_ROW + REVIEW_DESCRIPTION_ROWS]
            .to_vec();
        assert!(applying_rows.join("\n").contains("3333"));
        assert!(!applying_rows.join("\n").contains("0000"));
    }

    #[test]
    fn newline_and_tab_report_description_input_limit_errors() {
        let mut pane = pane_with_changes();
        pane.view = PaneView::Review;
        pane.description_editor = DescriptionEditor::Editing {
            change: 42,
            input: "x".repeat(crate::p4::MAX_DESCRIPTION_BYTES),
            cursor: crate::p4::MAX_DESCRIPTION_BYTES,
        };

        dispatch_key(&mut pane, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(pane.status, "Description reached its input limit");
        pane.status.clear();
        dispatch_key(&mut pane, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(pane.status, "Description reached its input limit");
    }

    #[test]
    fn ctrl_enter_starts_apply_without_a_second_confirmation() {
        let mut pane = pane_with_changes();
        pane.view = PaneView::Review;
        pane.description_editor = DescriptionEditor::Editing {
            change: 42,
            input: "Updated description".into(),
            cursor: "Updated description".len(),
        };

        let rendered = render_frame(&pane, 80, 24).lines.join("\n");
        assert!(rendered.contains("[Apply Description]"));

        dispatch_key(
            &mut pane,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
        );
        assert!(matches!(
            pane.description_editor,
            DescriptionEditor::Applying { request_id: 1, .. }
        ));
    }

    #[test]
    fn stale_description_load_message_cannot_replace_the_active_request() {
        let mut pane = pane_with_changes();
        pane.description_editor = DescriptionEditor::Loading {
            change: 42,
            request_id: 7,
        };

        assert_eq!(
            pane.handle_message(PaneMessage::DescriptionLoaded {
                change: 42,
                request_id: 6,
                result: Err(DescriptionApplyError::Stale),
            }),
            MessageEffect::None
        );
        assert!(matches!(
            pane.description_editor,
            DescriptionEditor::Loading { request_id: 7, .. }
        ));
    }

    #[test]
    fn successful_description_apply_updates_the_composer_and_reloads() {
        let mut pane = pane_with_changes();
        pane.description_editor = DescriptionEditor::Applying {
            change: 42,
            input: "Updated description".into(),
            cursor: "Updated description".len(),
            request_id: 9,
        };

        assert_eq!(
            pane.handle_message(PaneMessage::DescriptionApplied {
                change: 42,
                request_id: 9,
                result: Ok(DescriptionApplyResult {
                    change: 42,
                    spec_token: crate::domain::SpecToken::from_bytes_for_test([2; 32]),
                }),
            }),
            MessageEffect::ReloadOverview
        );
        assert!(matches!(pane.description_editor, DescriptionEditor::Idle));
        assert_eq!(
            pane.selected_changelist()
                .map(|change| change.description.as_str()),
            Some("Updated description")
        );
        assert!(pane.status.contains("updated and verified"));
    }

    #[test]
    fn uncertain_result_offers_only_read_only_reconciliation() {
        let mut pane = pane_with_changes();
        pane.overlay
            .replace_state_for_test(SubmitOverlayState::Failure {
                change: 42,
                failure: SubmitFailure {
                    fault: SubmitFault::Timeout,
                    certainty: SubmitOutcomeCertainty::Unknown,
                    detail: "The write timed out; the server may have accepted it.".into(),
                    next_step: "Do not submit again.",
                },
                receipt: None,
            });
        let frame = render_frame(&pane, 80, 24);
        let rendered = frame.lines.join("\n");
        assert!(rendered.contains("Submission result unknown"));
        assert!(rendered.contains("[Read-only reconcile]"));
        assert!(!rendered.contains("[Close]"));
        assert!(!rendered.contains("[Submit]"));
    }

    #[test]
    fn small_terminal_render_is_bounded() {
        let pane = pane_with_changes();
        let frame = render_frame(&pane, 20, 5);
        assert_eq!(frame.lines.len(), 5);
        assert!(frame.lines.iter().all(|line| display_width(line) == 20));
    }

    fn sample_changelist(id: u64, description: &str) -> Changelist {
        Changelist {
            id: ChangelistId::Numbered(id),
            status: ChangelistStatus::Pending,
            owner: "ExampleUser".into(),
            client: "ExampleClient".into(),
            description: description.into(),
            files: Vec::new(),
            preserved_spec_fields: Default::default(),
            spec_token: None,
            content_token: None,
        }
    }

    fn pane_with_numbered_changes(count: usize) -> PaneModel {
        let mut pane = PaneModel::new(PathBuf::from("C:/Example Workspace"));
        pane.install_overview(Ok(WorkspaceOverview {
            identity: WorkspaceIdentity {
                server_id: "perforce.example".into(),
                user: "ExampleUser".into(),
                client: "ExampleClient".into(),
                root: PathBuf::from("C:/Example Workspace"),
                stream: None,
                case_handling: CaseHandling::Insensitive,
            },
            changes: (1..=count)
                .map(|id| sample_changelist(id as u64, &format!("Change {id}")))
                .collect(),
        }));
        pane
    }

    fn dispatch_key(pane: &mut PaneModel, key: KeyEvent) {
        let service = Arc::new(P4WriteService::new(
            P4Client::new_with_directory_environment(
                crate::p4::fake::FakeP4Transport::default(),
                "p4",
                &pane.cwd,
                Default::default(),
            ),
        ));
        let (sender, _receiver) = mpsc::channel();
        pane.handle_key(key, &service, &sender);
    }

    #[test]
    fn key_release_does_not_move_the_changelist_selection() {
        let mut pane = pane_with_numbered_changes(3);
        pane.view = PaneView::Review;
        dispatch_key(
            &mut pane,
            KeyEvent::new_with_kind(
                KeyCode::Char('j'),
                KeyModifiers::NONE,
                KeyEventKind::Release,
            ),
        );
        assert_eq!(pane.selected, 0);
        dispatch_key(
            &mut pane,
            KeyEvent::new_with_kind(KeyCode::Char('j'), KeyModifiers::NONE, KeyEventKind::Press),
        );
        assert_eq!(pane.selected, 1);
    }

    #[test]
    fn selected_changelist_stays_in_the_visible_window() {
        let mut pane = pane_with_numbered_changes(40);
        pane.view = PaneView::Review;
        pane.selected = 39;
        let frame = render_frame(&pane, 80, 24);
        let rendered = frame.lines.join("\n");
        assert!(rendered.contains("> CL 40"));
        assert!(!rendered.contains("CL 1  pending"));
        assert_eq!(frame.hits.changelist_offset, 30);
        assert_eq!(frame.hits.changelists.len(), 10);
    }

    #[test]
    fn review_overlay_shows_server_user_and_client() {
        let mut pane = pane_with_changes();
        pane.overlay
            .replace_state_for_test(SubmitOverlayState::Review {
                preview: SubmitPreview::from_workspace_for_test(
                    42,
                    "Fix submit overlay",
                    WorkspaceIdentity {
                        server_id: "perforce.example".into(),
                        user: "ExampleUser".into(),
                        client: "ExampleClient".into(),
                        root: PathBuf::from("C:/Example Workspace"),
                        stream: None,
                        case_handling: CaseHandling::Insensitive,
                    },
                ),
            });
        let rendered = render_frame(&pane, 80, 24).lines.join("\n");
        assert!(rendered.contains("Submit CL 42"));
        assert!(rendered.contains("Server: perforce.example"));
        assert!(rendered.contains("User: ExampleUser"));
        assert!(rendered.contains("Client: ExampleClient"));
        assert!(rendered.contains("Provider: Native p4 submit"));
        assert!(rendered.contains("[Submit]"));
        assert!(!rendered.contains("[Open provider]"));
    }

    fn pane_with_external_provider(label: &str) -> PaneModel {
        let mut pane = pane_with_changes();
        pane.submit_provider = Arc::new(SubmitProvider::external_for_test(
            label,
            PathBuf::from("C:/Tools/submit-tool.exe"),
            vec!["--changelist".into(), "{change}".into()],
        ));
        pane
    }

    fn review_overlay_for(pane: &mut PaneModel) {
        pane.overlay
            .replace_state_for_test(SubmitOverlayState::Review {
                preview: SubmitPreview::from_workspace_for_test(
                    42,
                    "Fix submit overlay",
                    WorkspaceIdentity {
                        server_id: "perforce.example".into(),
                        user: "ExampleUser".into(),
                        client: "ExampleClient".into(),
                        root: PathBuf::from("C:/Example Workspace"),
                        stream: None,
                        case_handling: CaseHandling::Insensitive,
                    },
                ),
            });
    }

    #[test]
    fn external_review_overlay_offers_open_provider_not_submit() {
        let mut pane = pane_with_external_provider("P4Lab");
        review_overlay_for(&mut pane);
        let rendered = render_frame(&pane, 80, 24).lines.join("\n");
        assert!(rendered.contains("Provider: P4Lab"));
        assert!(rendered.contains("[Open provider]"));
        assert!(rendered.contains("Ctrl+Enter: Open provider"));
        assert!(!rendered.contains("[Submit]"));
        assert!(!rendered.contains("Ctrl+Enter: Submit"));
    }

    #[test]
    fn external_provider_label_cannot_impersonate_native_submit() {
        let mut pane = pane_with_external_provider("Native p4 submit");
        review_overlay_for(&mut pane);
        let rendered = render_frame(&pane, 80, 24).lines.join("\n");
        assert!(rendered.contains("Provider: Native p4 submit"));
        assert!(rendered.contains("[Open provider]"));
        assert!(!rendered.contains("[Submit]"));
        pane.overlay
            .replace_state_for_test(SubmitOverlayState::Running {
                change: 42,
                receipt: SubmitPreview::from_workspace_for_test(
                    42,
                    "Fix submit overlay",
                    WorkspaceIdentity {
                        server_id: "perforce.example".into(),
                        user: "ExampleUser".into(),
                        client: "ExampleClient".into(),
                        root: PathBuf::from("C:/Example Workspace"),
                        stream: None,
                        case_handling: CaseHandling::Insensitive,
                    },
                )
                .authorize(SubmitIntent::CtrlEnter)
                .expect("authorization")
                .reconciliation_receipt(),
            });
        let running = render_frame(&pane, 80, 24).lines.join("\n");
        assert!(running.contains("Opening external provider for CL 42"));
        assert!(running.contains("Herdr will not run p4 submit for this handoff."));
        assert!(!running.contains("Submitting CL 42"));
        assert!(!running.contains("Exactly one p4 submit process is running."));
    }

    #[test]
    fn explorer_view_switch_preserves_review_and_tree_selection() {
        let mut pane = pane_with_changes();
        pane.selected = 0;
        pane.explorer
            .install_ready_listing_for_test(LoadedDirectory {
                path: PathBuf::from("C:/Example Workspace"),
                entries: vec![crate::domain::ExplorerEntry {
                    name: "Foo.cpp".into(),
                    path: PathBuf::from("C:/Example Workspace/Foo.cpp"),
                    kind: crate::domain::ExplorerEntryKind::File,
                    decoration: Some(crate::domain::ExplorerDecoration::Opened {
                        action: crate::domain::FileAction::Edit,
                        change: Some(ChangelistId::Numbered(42)),
                    }),
                    file_type: Some(crate::domain::FileType::new("text")),
                    have_rev: Some(1),
                    head_rev: Some(1),
                }],
                truncated: false,
            });
        pane.view = PaneView::Explorer;
        pane.explorer.select_index(1);
        dispatch_key(
            &mut pane,
            KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE),
        );
        assert_eq!(pane.view, PaneView::Review);
        assert_eq!(pane.selected, 0);
        dispatch_key(
            &mut pane,
            KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE),
        );
        assert_eq!(pane.view, PaneView::Explorer);
        assert_eq!(
            pane.explorer.selected_path(),
            Some(Path::new("C:/Example Workspace/Foo.cpp"))
        );
    }

    #[test]
    fn submit_shortcut_is_ignored_in_explorer() {
        let mut pane = pane_with_changes();
        pane.view = PaneView::Explorer;
        dispatch_key(
            &mut pane,
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE),
        );
        assert!(matches!(pane.overlay.state(), SubmitOverlayState::Closed));
        assert!(pane.status.contains("Review"));
        pane.view = PaneView::Review;
        dispatch_key(
            &mut pane,
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE),
        );
        assert!(!matches!(pane.overlay.state(), SubmitOverlayState::Closed));
    }

    #[test]
    fn explorer_file_activation_keeps_the_navigation_pane_tree_only() {
        let mut pane = pane_with_changes();
        pane.explorer
            .install_ready_listing_for_test(LoadedDirectory {
                path: PathBuf::from("C:/Example Workspace"),
                entries: vec![crate::domain::ExplorerEntry {
                    name: "Foo.cpp".into(),
                    path: PathBuf::from("C:/Example Workspace/Foo.cpp"),
                    kind: crate::domain::ExplorerEntryKind::File,
                    decoration: Some(crate::domain::ExplorerDecoration::Opened {
                        action: crate::domain::FileAction::Edit,
                        change: Some(ChangelistId::Numbered(42)),
                    }),
                    file_type: Some(crate::domain::FileType::new("text")),
                    have_rev: Some(1),
                    head_rev: Some(1),
                }],
                truncated: false,
            });
        pane.view = PaneView::Explorer;
        pane.content.disable_host_for_test();
        pane.explorer.select_index(1);
        dispatch_key(&mut pane, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(pane.view, PaneView::Explorer);
        assert!(pane.status.contains("HERDR_PANE_ID"));
        let rendered = render_frame(&pane, 80, 24).lines.join("\n");
        assert!(rendered.contains("Explorer"));
        assert!(rendered.contains("Foo.cpp"));
        assert!(!rendered.contains("File diff:"));
    }

    #[test]
    fn explorer_not_in_view_shows_connection_message_without_a_tree() {
        let mut pane = pane_with_changes();
        pane.view = PaneView::Explorer;
        pane.explorer
            .install_not_in_view(pane.explorer.generation());
        let frame = render_frame(&pane, 80, 24);
        let rendered = frame.lines.join("\n");
        assert!(rendered.contains("Explorer"));
        assert!(rendered.contains("not in the current client view"));
        assert!(frame.hits.explorer_rows.is_empty());
        assert!(!rendered.contains("p4 add"));
        assert!(!rendered.contains("git status"));
    }

    #[test]
    fn explorer_ready_tree_renders_local_names_and_readonly_badges() {
        let mut pane = pane_with_changes();
        pane.explorer
            .install_ready_listing_for_test(LoadedDirectory {
                path: PathBuf::from("C:/Example Workspace"),
                entries: vec![
                    crate::domain::ExplorerEntry {
                        name: "src".into(),
                        path: PathBuf::from("C:/Example Workspace/src"),
                        kind: crate::domain::ExplorerEntryKind::Directory,
                        decoration: None,
                        file_type: None,
                        have_rev: None,
                        head_rev: None,
                    },
                    crate::domain::ExplorerEntry {
                        name: "Foo.cpp".into(),
                        path: PathBuf::from("C:/Example Workspace/Foo.cpp"),
                        kind: crate::domain::ExplorerEntryKind::File,
                        decoration: Some(crate::domain::ExplorerDecoration::Opened {
                            action: crate::domain::FileAction::Edit,
                            change: Some(ChangelistId::Numbered(42)),
                        }),
                        file_type: Some(crate::domain::FileType::new("text")),
                        have_rev: Some(1),
                        head_rev: Some(1),
                    },
                ],
                truncated: false,
            });
        pane.view = PaneView::Explorer;
        let rendered = render_frame(&pane, 100, 24).lines.join("\n");
        assert!(rendered.contains("Explorer"));
        assert!(rendered.contains("Foo.cpp"));
        assert!(rendered.contains("src"));
        assert!(rendered.contains("📂"));
        assert!(rendered.contains("📁"));
        assert!(rendered.contains("🔩"));
        assert!(rendered.contains("  M"));
        assert!(!rendered.contains("p4 add"));
        assert!(!rendered.contains("p4 edit"));
    }

    #[test]
    fn stale_overview_generation_is_ignored() {
        let mut pane = pane_with_changes();
        pane.overview_generation = 2;
        pane.handle_message(PaneMessage::Overview {
            generation: 1,
            result: Ok(WorkspaceOverview {
                identity: WorkspaceIdentity {
                    server_id: "other".into(),
                    user: "OtherUser".into(),
                    client: "OtherClient".into(),
                    root: PathBuf::from("C:/Other"),
                    stream: None,
                    case_handling: CaseHandling::Insensitive,
                },
                changes: vec![sample_changelist(99, "Stale")],
            }),
        });
        let OverviewState::Ready(overview) = &pane.overview else {
            panic!("overview should stay ready");
        };
        assert_eq!(overview.changes[0].id, ChangelistId::Numbered(42));
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn dispatch_mouse(pane: &mut PaneModel, event: MouseEvent) {
        let service = Arc::new(P4WriteService::new(
            P4Client::new_with_directory_environment(
                crate::p4::fake::FakeP4Transport::default(),
                "p4",
                &pane.cwd,
                Default::default(),
            ),
        ));
        let (sender, _receiver) = mpsc::channel();
        pane.set_nav_size(80, 24);
        let frame = render_frame(pane, 80, 24);
        pane.handle_mouse(event, &frame.hits, &service, &sender);
    }

    #[test]
    fn keyboard_m_opens_explorer_context_menu() {
        let mut pane = pane_with_changes();
        pane.explorer
            .install_ready_listing_for_test(LoadedDirectory {
                path: PathBuf::from("C:/Example Workspace"),
                entries: vec![crate::domain::ExplorerEntry {
                    name: "Foo.cpp".into(),
                    path: PathBuf::from("C:/Example Workspace/Foo.cpp"),
                    kind: crate::domain::ExplorerEntryKind::File,
                    decoration: None,
                    file_type: Some(crate::domain::FileType::new("text")),
                    have_rev: Some(1),
                    head_rev: Some(1),
                }],
                truncated: false,
            });
        pane.explorer.select_index(1);
        dispatch_key(
            &mut pane,
            KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE),
        );
        let rendered = render_frame(&pane, 80, 24).lines.join("\n");
        assert!(rendered.contains("Copy Path"));
        assert!(rendered.contains("Rename"));
        assert!(rendered.contains("Reveal in File Explorer"));
        assert!(!rendered.contains("p4 add"));
    }

    #[test]
    fn embedded_review_submit_button_opens_preflight_not_a_direct_write() {
        let mut pane = pane_with_changes();
        pane.view = PaneView::Review;
        let frame = render_frame(&pane, 80, 24);
        assert_eq!(
            frame.hits.review_submit.map(|target| target.width),
            Some(80)
        );
        dispatch_mouse(
            &mut pane,
            mouse(
                MouseEventKind::Down(MouseButton::Left),
                79,
                REVIEW_SUBMIT_ROW as u16,
            ),
        );
        assert!(matches!(
            pane.overlay.state(),
            SubmitOverlayState::Preflight { change: 42, .. }
        ));
    }

    #[test]
    fn workspace_root_menu_omits_destructive_actions() {
        let mut pane = pane_with_changes();
        pane.explorer
            .install_ready_listing_for_test(LoadedDirectory {
                path: PathBuf::from("C:/Example Workspace"),
                entries: vec![crate::domain::ExplorerEntry {
                    name: "Foo.cpp".into(),
                    path: PathBuf::from("C:/Example Workspace/Foo.cpp"),
                    kind: crate::domain::ExplorerEntryKind::File,
                    decoration: None,
                    file_type: Some(crate::domain::FileType::new("text")),
                    have_rev: Some(1),
                    head_rev: Some(1),
                }],
                truncated: false,
            });
        dispatch_key(
            &mut pane,
            KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE),
        );
        let rendered = render_frame(&pane, 80, 24).lines.join("\n");
        assert!(rendered.contains("New File"));
        assert!(rendered.contains("Reveal in File Explorer"));
        assert!(!rendered.contains("Rename"));
        assert!(!rendered.contains("Delete"));
    }

    #[test]
    fn review_wheel_keeps_the_visible_offset() {
        let mut pane = pane_with_numbered_changes(40);
        pane.view = PaneView::Review;
        pane.selected = 25;
        pane.set_nav_size(80, 24);
        let before = render_frame(&pane, 80, 24).hits.changelist_offset;
        assert_eq!(before, 16);
        dispatch_mouse(&mut pane, mouse(MouseEventKind::ScrollDown, 2, 8));
        let after = render_frame(&pane, 80, 24).hits.changelist_offset;
        assert_eq!(after, before + 3);
    }

    #[test]
    fn explorer_wheel_scrolls_without_changing_selection() {
        let mut pane = pane_with_changes();
        let root = PathBuf::from("C:/Example Workspace");
        pane.explorer
            .install_ready_listing_for_test(LoadedDirectory {
                path: root.clone(),
                entries: (0..40)
                    .map(|index| crate::domain::ExplorerEntry {
                        name: format!("file-{index:02}.txt"),
                        path: root.join(format!("file-{index:02}.txt")),
                        kind: crate::domain::ExplorerEntryKind::File,
                        decoration: None,
                        file_type: Some(crate::domain::FileType::new("text")),
                        have_rev: Some(1),
                        head_rev: Some(1),
                    })
                    .collect(),
                truncated: false,
            });
        pane.set_nav_size(80, 24);
        let selected = pane.explorer.selected_path().map(Path::to_path_buf);
        let before = render_frame(&pane, 80, 24).hits.explorer_offset;
        dispatch_mouse(&mut pane, mouse(MouseEventKind::ScrollDown, 2, 8));
        let after = render_frame(&pane, 80, 24);
        assert_eq!(pane.explorer.selected_path(), selected.as_deref());
        assert!(after.hits.explorer_offset > before);
        assert!(after.lines.join("\n").contains("file-10.txt"));
    }

    #[test]
    fn explorer_rows_stay_single_line_and_pan_horizontally() {
        let mut pane = pane_with_changes();
        let long_name = "very-long-source-file-name-that-cannot-fit.cpp";
        pane.explorer
            .install_ready_listing_for_test(LoadedDirectory {
                path: PathBuf::from("C:/Example Workspace"),
                entries: vec![crate::domain::ExplorerEntry {
                    name: long_name.into(),
                    path: PathBuf::from("C:/Example Workspace").join(long_name),
                    kind: crate::domain::ExplorerEntryKind::File,
                    decoration: None,
                    file_type: Some(crate::domain::FileType::new("text")),
                    have_rev: Some(1),
                    head_rev: Some(1),
                }],
                truncated: false,
            });
        pane.set_nav_size(24, 16);
        let clipped = render_frame(&pane, 24, 16);
        assert!(clipped.lines.iter().all(|line| !line.contains('\n')));
        assert!(clipped.lines.iter().any(|line| line.contains("very-long")));
        assert!(
            !clipped
                .lines
                .iter()
                .any(|line| line.contains("cannot-fit.cpp"))
        );
        pane.explorer.set_scroll_x(32, 23, 80);
        let panned = render_frame(&pane, 24, 16).lines.join("\n");
        assert!(
            panned.contains("cannot-fit")
                || panned.contains("fit.cpp")
                || panned.contains("that-cannot")
        );
    }
}
