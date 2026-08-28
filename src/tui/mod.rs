//! Terminal pane: workspace File Explorer and Submit review views.

mod content;
mod diff;
mod explorer;
mod syntax;
mod wrap;

use std::{
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    thread,
    time::Duration,
};

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute, queue,
    style::Print,
    terminal::{
        self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode,
    },
};

use crate::{
    app::{SubmitOutcomeCertainty, SubmitOverlay, SubmitOverlayRequest, SubmitOverlayState},
    domain::{Changelist, ChangelistId, WorkspaceIdentity},
    p4::{
        DomainMappingError, ExplorerError, LoadedDirectory, P4Client, P4Error, P4Query,
        P4Transport, P4WriteService, StdProcessTransport, SubmitError, SubmitIntent, SubmitPreview,
        SubmitReconciliationResult, SubmitResult, WorkspaceCwdError, cwd_is_in_client_view,
        load_explorer_directory, load_opened_records, pending_changelists_from_changes,
        workspace_owning_cwd,
    },
    submit_provider::{ExternalLaunchError, SubmitProvider},
};

use self::content::ContentPaneClient;
use self::explorer::{
    ExplorerAction, ExplorerLoadState, ExplorerModel, connection_message, open_with_default_app,
};

pub use self::content::{navigation_resize_args_for_layout, run_content_pane};

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
    let mut rendered = render_frame(&pane, width, height);
    terminal
        .draw(&rendered.lines)
        .map_err(|error| error.to_string())?;
    let mut dirty = false;
    loop {
        while let Ok(message) = receiver.try_recv() {
            let effect = pane.handle_message(message);
            dirty = true;
            apply_message_effect(&mut pane, effect, &sender);
        }

        if dirty {
            let (width, height) = terminal::size().map_err(|error| error.to_string())?;
            rendered = render_frame(&pane, width, height);
            terminal
                .draw(&rendered.lines)
                .map_err(|error| error.to_string())?;
            dirty = false;
        }

        if !event::poll(EVENT_POLL).map_err(|error| error.to_string())? {
            continue;
        }
        let event = event::read().map_err(|error| error.to_string())?;
        let (should_close, event_changed) = match event {
            Event::Key(key) if key.kind != KeyEventKind::Press => (false, false),
            Event::Key(key) => (pane.handle_key(key, &service, &sender), true),
            Event::Mouse(mouse) => {
                let changed = matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left));
                (
                    pane.handle_mouse(mouse, &rendered.hits, &service, &sender),
                    changed,
                )
            }
            Event::Resize(_, _) => (false, true),
            Event::FocusGained | Event::FocusLost | Event::Paste(_) => (false, false),
        };
        if should_close {
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
            Hide,
            Clear(ClearType::All)
        ) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        Ok(Self { stdout })
    }

    fn draw(&mut self, lines: &[String]) -> io::Result<()> {
        queue!(self.stdout, MoveTo(0, 0), Clear(ClearType::All))?;
        for (row, line) in lines.iter().enumerate() {
            queue!(self.stdout, MoveTo(0, row as u16), Print(line))?;
        }
        self.stdout.flush()
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(self.stdout, Show, DisableMouseCapture, LeaveAlternateScreen);
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
}

impl PaneModel {
    #[cfg(test)]
    fn new(cwd: PathBuf) -> Self {
        let mut pane = Self::new_with_provider(cwd, Arc::new(SubmitProvider::Native));
        pane.reload_provider_from_environment = false;
        pane
    }

    fn new_with_provider(cwd: PathBuf, submit_provider: Arc<SubmitProvider>) -> Self {
        Self {
            cwd: cwd.clone(),
            view: PaneView::Review,
            overview: OverviewState::Loading,
            overview_generation: 1,
            selected: 0,
            explorer: ExplorerModel::new(cwd.clone()),
            content: ContentPaneClient::new(cwd.clone()),
            overlay: SubmitOverlay::default(),
            submit_provider,
            reload_provider_from_environment: true,
            status: "Loading current Perforce workspace...".to_owned(),
        }
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

    fn reload_overview(&mut self, sender: &mpsc::Sender<PaneMessage>) {
        self.overview_generation = self.overview_generation.wrapping_add(1);
        request_overview(self.cwd.clone(), self.overview_generation, sender.clone());
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

        match key.code {
            KeyCode::Char('q') => true,
            KeyCode::Char('1') => {
                self.view = PaneView::Explorer;
                false
            }
            KeyCode::Char('2') => {
                self.view = PaneView::Review;
                false
            }
            KeyCode::Char('r') | KeyCode::Char('R') => {
                self.overview = OverviewState::Loading;
                self.status = "Refreshing current Perforce workspace...".to_owned();
                self.reload_overview(sender);
                false
            }
            KeyCode::Char('s') | KeyCode::Char('S') => {
                if self.view != PaneView::Review {
                    self.status = "Switch to Review (2) to submit".to_owned();
                    return false;
                }
                let Some(change) = self
                    .selected_changelist()
                    .and_then(|change| match change.id {
                        ChangelistId::Numbered(change) => Some(change),
                        ChangelistId::Default => None,
                    })
                else {
                    self.status = "Default changelist Submit is disabled".to_owned();
                    return false;
                };
                if let Some(request) = self.overlay.open(change) {
                    self.issue_overlay_request(request, service, sender);
                }
                false
            }
            _ if self.view == PaneView::Explorer => self.handle_explorer_key(key, sender),
            KeyCode::Enter => {
                self.open_selected_changelist();
                false
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                false
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let count = match &self.overview {
                    OverviewState::Ready(overview) => overview.changes.len(),
                    OverviewState::Loading | OverviewState::Failed(_) => 0,
                };
                self.selected = self.selected.saturating_add(1).min(count.saturating_sub(1));
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
                false
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.explorer.move_selection(1);
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
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return false;
        }
        if !matches!(self.overlay.state(), SubmitOverlayState::Closed) {
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
            return false;
        }
        if hits
            .view_explorer
            .is_some_and(|target| target.contains(mouse.column, mouse.row))
        {
            self.view = PaneView::Explorer;
            return false;
        }
        if hits
            .view_review
            .is_some_and(|target| target.contains(mouse.column, mouse.row))
        {
            self.view = PaneView::Review;
            return false;
        }
        if self.view == PaneView::Explorer {
            if let Some(index) = hits
                .explorer_rows
                .iter()
                .position(|target| target.contains(mouse.column, mouse.row))
            {
                let action = self
                    .explorer
                    .activate_index(hits.explorer_offset.saturating_add(index));
                self.apply_explorer_action(action, sender);
            }
            return false;
        }
        if let Some(index) = hits
            .changelists
            .iter()
            .position(|target| target.contains(mouse.column, mouse.row))
        {
            self.selected = hits.changelist_offset.saturating_add(index);
            self.open_selected_changelist();
        }
        false
    }
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
    let opened = load_opened_records(&client).unwrap_or_default();
    load_explorer_directory(&client, identity, cwd, &opened).map_err(|error| match error {
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
        let opened = load_opened_records(&client).unwrap_or_default();
        let result = load_explorer_directory(&client, &identity, &path, &opened);
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

#[derive(Debug, Clone, Copy, Default)]
struct Rect {
    x: u16,
    y: u16,
    width: u16,
}

impl Rect {
    fn contains(self, x: u16, y: u16) -> bool {
        y == self.y && x >= self.x && x < self.x.saturating_add(self.width)
    }
}

#[derive(Debug, Default)]
struct HitTargets {
    cancel: Option<Rect>,
    submit: Option<Rect>,
    refresh: Option<Rect>,
    changelists: Vec<Rect>,
    changelist_offset: usize,
    view_explorer: Option<Rect>,
    view_review: Option<Rect>,
    explorer_rows: Vec<Rect>,
    explorer_offset: usize,
}

#[derive(Debug, Default)]
struct RenderedFrame {
    lines: Vec<String>,
    hits: HitTargets,
}

fn render_frame(pane: &PaneModel, width: u16, height: u16) -> RenderedFrame {
    let width = width.max(1) as usize;
    let height = height.max(1) as usize;
    let mut frame = RenderedFrame {
        lines: vec![" ".repeat(width); height],
        hits: HitTargets::default(),
    };
    render_view_tabs(&mut frame, pane.view, width);
    put(
        &mut frame.lines,
        0,
        1,
        &format!("Workspace path: {}", pane.cwd.display()),
    );

    match pane.view {
        PaneView::Explorer => render_explorer(&mut frame, pane, width, height),
        PaneView::Review => render_review(&mut frame, pane, width, height),
    }
    if height >= 1 {
        put(&mut frame.lines, 0, height - 1, &pane.status);
    }

    if !matches!(pane.overlay.state(), SubmitOverlayState::Closed) {
        render_overlay(
            &mut frame,
            pane.overlay.state(),
            &pane.submit_provider,
            width,
            height,
        );
    }
    for line in &mut frame.lines {
        *line = truncate_and_pad(line, width);
    }
    frame
}

fn render_view_tabs(frame: &mut RenderedFrame, view: PaneView, _width: usize) {
    let prefix = "Herdr Perforce  ";
    let explorer = if view == PaneView::Explorer {
        "[1 Explorer]"
    } else {
        " 1 Explorer "
    };
    let review = if view == PaneView::Review {
        "[2 Review]"
    } else {
        " 2 Review "
    };
    put(
        &mut frame.lines,
        0,
        0,
        &format!("{prefix}{explorer}  {review}"),
    );
    let explorer_x = prefix.chars().count() as u16;
    let explorer_width = explorer.chars().count() as u16;
    frame.hits.view_explorer = Some(Rect {
        x: explorer_x,
        y: 0,
        width: explorer_width,
    });
    frame.hits.view_review = Some(Rect {
        x: explorer_x + explorer_width + 2,
        y: 0,
        width: review.chars().count() as u16,
    });
}

fn render_review(frame: &mut RenderedFrame, pane: &PaneModel, width: usize, height: usize) {
    match &pane.overview {
        OverviewState::Loading => put(&mut frame.lines, 0, 3, "Loading changelists..."),
        OverviewState::Failed(message) => {
            put(&mut frame.lines, 0, 3, "Workspace refresh failed");
            put(&mut frame.lines, 0, 4, message);
            put(
                &mut frame.lines,
                0,
                6,
                "r: retry read-only refresh   q: close pane",
            );
        }
        OverviewState::Ready(overview) => {
            put(
                &mut frame.lines,
                0,
                3,
                &format!(
                    "Client: {}   User: {}",
                    overview.identity.client, overview.identity.user
                ),
            );
            put(&mut frame.lines, 0, 5, "Pending changelists");
            let available_rows = height.saturating_sub(9);
            let (offset, visible) =
                changelist_window(pane.selected, overview.changes.len(), available_rows);
            frame.hits.changelist_offset = offset;
            for (visible_index, change) in overview
                .changes
                .iter()
                .skip(offset)
                .take(visible)
                .enumerate()
            {
                let index = offset + visible_index;
                let row = 6 + visible_index;
                let marker = if index == pane.selected { ">" } else { " " };
                let description = change
                    .description
                    .lines()
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or("<no description>")
                    .trim();
                put(
                    &mut frame.lines,
                    0,
                    row,
                    &format!(
                        "{marker} CL {}  {}  {description}",
                        change.id,
                        change.status.canonical_name()
                    ),
                );
                frame.hits.changelists.push(Rect {
                    x: 0,
                    y: row as u16,
                    width: width as u16,
                });
            }
            if height >= 2 {
                put(
                    &mut frame.lines,
                    0,
                    height - 2,
                    "1/2: views   Enter: files   j/k: select   s: Submit review   r: refresh   q: close",
                );
            }
        }
    }
}

fn render_explorer(frame: &mut RenderedFrame, pane: &PaneModel, width: usize, height: usize) {
    if height >= 2 {
        put(&mut frame.lines, 0, height - 2, explorer_help(pane));
    }
    match pane.explorer.load_state() {
        ExplorerLoadState::Idle | ExplorerLoadState::Checking => {
            put(&mut frame.lines, 0, 3, "Loading workspace files...");
        }
        ExplorerLoadState::NotInClientView => {
            put(
                &mut frame.lines,
                0,
                3,
                "Workspace is not in the current client view",
            );
            put(&mut frame.lines, 0, 5, connection_message());
        }
        ExplorerLoadState::Failed(message) => {
            put(&mut frame.lines, 0, 3, "Explorer refresh failed");
            put(&mut frame.lines, 0, 4, message);
        }
        ExplorerLoadState::Ready => {
            if let OverviewState::Ready(overview) = &pane.overview {
                put(
                    &mut frame.lines,
                    0,
                    3,
                    &format!(
                        "Client: {}   User: {}",
                        overview.identity.client, overview.identity.user
                    ),
                );
            }
            let body_top = 5;
            let body_bottom = height.saturating_sub(2);
            if body_bottom <= body_top {
                return;
            }
            let body_height = body_bottom - body_top;
            render_tree_column(frame, pane, 0, body_top, width, body_height);
        }
    }
}

fn explorer_help(pane: &PaneModel) -> &'static str {
    if pane.explorer.jump_target().is_some() {
        "1/2: views   Enter: file   d: diff   j/k: select   o: external   r: refresh   q: close"
    } else {
        "1/2: views   Enter: file   j/k: select   o: external   r: refresh   q: close"
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
    put_clipped(&mut frame.lines, x, y, "> Files", width);
    let (offset, visible, rows) = pane.explorer.tree_window(height.saturating_sub(1));
    frame.hits.explorer_offset = offset;
    let selected = pane.explorer.selected_path();
    for (visible_index, row) in rows.iter().skip(offset).take(visible).enumerate() {
        let screen_row = y + 1 + visible_index;
        let caret = if selected.is_some_and(|path| path == row.path) {
            ">"
        } else {
            " "
        };
        let glyph = match row.kind {
            crate::domain::ExplorerEntryKind::Directory if row.expanded => "📂",
            crate::domain::ExplorerEntryKind::Directory => "📁",
            crate::domain::ExplorerEntryKind::File => "📄",
        };
        let indent = "  ".repeat(row.depth);
        let badge = row
            .decoration
            .as_ref()
            .map(|decoration| decoration.badge())
            .unwrap_or("");
        let line = if badge.is_empty() {
            format!("{caret}{indent}{glyph} {}", row.name)
        } else {
            format!("{caret}{indent}{glyph} {}  {badge}", row.name)
        };
        put_clipped(&mut frame.lines, x, screen_row, &line, width);
        frame.hits.explorer_rows.push(Rect {
            x: x as u16,
            y: screen_row as u16,
            width: width as u16,
        });
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
            frame.hits.cancel = Some(Rect {
                x: cancel_x as u16,
                y: button_row as u16,
                width: cancel_text.len() as u16,
            });
            frame.hits.submit = Some(Rect {
                x: submit_x as u16,
                y: button_row as u16,
                width: submit_text.len() as u16,
            });
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
                frame.hits.cancel = Some(Rect {
                    x: x as u16,
                    y: button_row as u16,
                    width: 7,
                });
                x + 10
            };
            put(&mut frame.lines, refresh_x, button_row, refresh_text);
            frame.hits.refresh = Some(Rect {
                x: refresh_x as u16,
                y: button_row as u16,
                width: refresh_text.len() as u16,
            });
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

fn put_clipped(lines: &mut [String], column: usize, row: usize, value: &str, width: usize) {
    let clipped: String = value.chars().take(width).collect();
    put(lines, column, row, &clipped);
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
    fn pane_snapshot_keeps_submit_as_review_not_direct_write() {
        let pane = pane_with_changes();
        let frame = render_frame(&pane, 80, 24);
        let rendered = frame.lines.join("\n");
        assert!(rendered.contains("s: Submit review"));
        assert!(rendered.contains("CL 42  pending  Fix submit overlay"));
        assert!(!rendered.contains("p4 submit -c"));
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
        assert!(frame.lines.iter().all(|line| line.chars().count() == 20));
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
        let mut pane = pane_with_numbered_changes(20);
        pane.selected = 19;
        let frame = render_frame(&pane, 80, 24);
        let rendered = frame.lines.join("\n");
        assert!(rendered.contains("> CL 20"));
        assert!(!rendered.contains("CL 1  pending"));
        assert_eq!(frame.hits.changelist_offset, 5);
        assert_eq!(frame.hits.changelists.len(), 15);
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
        assert!(rendered.contains("[1 Explorer]"));
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
        assert!(rendered.contains("[1 Explorer]"));
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
        assert!(rendered.contains("[1 Explorer]"));
        assert!(rendered.contains("Foo.cpp"));
        assert!(rendered.contains("src"));
        assert!(rendered.contains("📂"));
        assert!(rendered.contains("📁"));
        assert!(rendered.contains("📄"));
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
}
