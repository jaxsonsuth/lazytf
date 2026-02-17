use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use color_eyre::eyre::{Result, WrapErr, eyre};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event as CEvent, KeyCode, KeyEvent,
        KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use glob::glob;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
};
use serde::Deserialize;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    sync::{mpsc, watch},
};

const CONFIG_CANDIDATES: [&str; 3] = ["lazyterraform.yaml", "Config.yaml", "config.yaml"];
const OUTPUT_BUFFER_LIMIT: usize = 4_000;

#[derive(Debug, Deserialize)]
struct Config {
    accounts: BTreeMap<String, AccountConfig>,
}

#[derive(Debug, Deserialize)]
struct AccountConfig {
    aws_profile: String,
    composition_path: String,
    region: Option<String>,
    #[serde(default)]
    var_files: Vec<String>,
}

#[derive(Debug, Clone)]
struct AccountState {
    name: String,
    aws_profile: String,
    region: Option<String>,
    composition_path: PathBuf,
    composition_issue: Option<String>,
    var_files: Vec<PathBuf>,
    auth: AuthStatus,
    workspaces: Vec<String>,
}

#[derive(Debug)]
struct LoadedConfig {
    path: PathBuf,
    base_dir: PathBuf,
    config: Config,
}

#[derive(Debug, Default)]
struct CliOptions {
    config_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthStatus {
    Unknown,
    Checking,
    Authenticated,
    Failed,
}

impl AuthStatus {
    fn icon(self) -> &'static str {
        match self {
            Self::Unknown => "?",
            Self::Checking => "~",
            Self::Authenticated => "*",
            Self::Failed => "x",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Checking => "checking",
            Self::Authenticated => "ready",
            Self::Failed => "failed",
        }
    }

    fn color(self) -> Color {
        match self {
            Self::Unknown => Color::DarkGray,
            Self::Checking => Color::Yellow,
            Self::Authenticated => Color::Green,
            Self::Failed => Color::Red,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FocusPanel {
    Accounts,
    Workspaces,
    Output,
}

impl FocusPanel {
    fn next(self) -> Self {
        match self {
            Self::Accounts => Self::Workspaces,
            Self::Workspaces => Self::Output,
            Self::Output => Self::Accounts,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::Accounts => Self::Output,
            Self::Workspaces => Self::Accounts,
            Self::Output => Self::Workspaces,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayoutMode {
    Split,
    OutputOnly,
}

impl LayoutMode {
    fn label(self) -> &'static str {
        match self {
            Self::Split => "split",
            Self::OutputOnly => "output",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OperationKind {
    AuthLogin,
    RefreshWorkspaces,
    TerraformInit,
    TerraformPlan,
    TerraformApply,
}

impl OperationKind {
    fn label(self) -> &'static str {
        match self {
            Self::AuthLogin => "aws sso login",
            Self::RefreshWorkspaces => "workspace refresh",
            Self::TerraformInit => "terraform init",
            Self::TerraformPlan => "terraform plan",
            Self::TerraformApply => "terraform apply",
        }
    }

    fn requires_workspace(self) -> bool {
        matches!(self, Self::TerraformPlan | Self::TerraformApply)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CancelSignal {
    None,
    Graceful,
    Force,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CancelStage {
    None,
    GracefulRequested,
    ForceRequested,
}

#[derive(Debug)]
struct InflightOperation {
    kind: OperationKind,
    account_idx: usize,
    cancel_tx: watch::Sender<CancelSignal>,
    cancel_stage: CancelStage,
}

#[derive(Debug)]
struct AppState {
    accounts: Vec<AccountState>,
    selected_account: usize,
    selected_workspace: usize,
    focused_panel: FocusPanel,
    previous_focus_panel: FocusPanel,
    layout_mode: LayoutMode,
    output_lines: Vec<String>,
    output_scroll_from_bottom: usize,
    status_line: String,
    inflight: Option<InflightOperation>,
    pending_apply_confirmation: bool,
    show_help: bool,
    quit_requested: bool,
}

impl AppState {
    fn from_config(config: Config, config_base_dir: &Path) -> Result<Self> {
        if config.accounts.is_empty() {
            return Err(eyre!(
                "Config has no accounts. Add at least one account under `accounts:`"
            ));
        }

        let mut accounts = Vec::with_capacity(config.accounts.len());
        let mut startup_lines =
            vec!["lazytf ready. Press `a` to authenticate selected account.".to_string()];

        for (name, account_cfg) in config.accounts {
            let (composition_path, composition_issue) = match resolve_composition_path(
                config_base_dir,
                &account_cfg.composition_path,
            ) {
                Ok(path) => (path, None),
                Err(err) => {
                    let fallback =
                        fallback_composition_path(config_base_dir, &account_cfg.composition_path);
                    let issue = format!(
                        "composition_path `{}` invalid: {err}",
                        account_cfg.composition_path
                    );
                    startup_lines.push(format!("warning: account `{name}` {issue}"));
                    startup_lines.push(format!(
                            "warning: using fallback path `{}` so UI can start; execution remains blocked until fixed",
                            fallback.display()
                        ));
                    (fallback, Some(issue))
                }
            };

            accounts.push(AccountState {
                name,
                aws_profile: account_cfg.aws_profile,
                region: account_cfg.region,
                var_files: resolve_var_file_paths(&account_cfg.var_files, &composition_path),
                composition_path,
                composition_issue,
                auth: AuthStatus::Unknown,
                workspaces: Vec::new(),
            });
        }

        Ok(Self {
            accounts,
            selected_account: 0,
            selected_workspace: 0,
            focused_panel: FocusPanel::Accounts,
            previous_focus_panel: FocusPanel::Accounts,
            layout_mode: LayoutMode::Split,
            output_lines: startup_lines,
            output_scroll_from_bottom: 0,
            status_line: "idle".to_string(),
            inflight: None,
            pending_apply_confirmation: false,
            show_help: false,
            quit_requested: false,
        })
    }

    fn selected_account(&self) -> Option<&AccountState> {
        self.accounts.get(self.selected_account)
    }

    fn selected_account_mut(&mut self) -> Option<&mut AccountState> {
        self.accounts.get_mut(self.selected_account)
    }

    fn selected_workspace_name(&self) -> Option<String> {
        let account = self.selected_account()?;
        account.workspaces.get(self.selected_workspace).cloned()
    }

    fn current_operation_label(&self) -> String {
        match &self.inflight {
            Some(op) => {
                let account_name = self
                    .accounts
                    .get(op.account_idx)
                    .map(|a| a.name.as_str())
                    .unwrap_or("?");
                format!("running {} on {account_name}", op.kind.label())
            }
            None => self.status_line.clone(),
        }
    }

    fn is_busy(&self) -> bool {
        self.inflight.is_some()
    }

    fn push_output(&mut self, line: impl Into<String>) {
        self.output_lines.push(line.into());
        if self.output_lines.len() > OUTPUT_BUFFER_LIMIT {
            let to_drop = self.output_lines.len() - OUTPUT_BUFFER_LIMIT;
            self.output_lines.drain(0..to_drop);
        }
    }

    fn set_status(&mut self, status: impl Into<String>) {
        self.status_line = status.into();
    }

    fn clear_apply_confirmation(&mut self) {
        self.pending_apply_confirmation = false;
    }

    fn toggle_help(&mut self) {
        self.show_help = !self.show_help;
    }

    fn close_help(&mut self) {
        self.show_help = false;
    }

    fn request_cancel(&mut self) {
        if let Some(op) = self.inflight.as_mut() {
            match op.cancel_stage {
                CancelStage::None => {
                    let _ = op.cancel_tx.send(CancelSignal::Graceful);
                    op.cancel_stage = CancelStage::GracefulRequested;
                    self.push_output(
                        "Graceful cancel requested. Sending SIGINT and waiting for Terraform to clean up state lock...",
                    );
                    self.push_output("Press `c` again to force kill if absolutely necessary.");
                    self.set_status("cancelling (graceful)...");
                }
                CancelStage::GracefulRequested => {
                    let _ = op.cancel_tx.send(CancelSignal::Force);
                    op.cancel_stage = CancelStage::ForceRequested;
                    self.push_output(
                        "Force kill requested. This may leave Terraform state locked.",
                    );
                    self.set_status("cancelling (forced)...");
                }
                CancelStage::ForceRequested => {
                    self.push_output(
                        "Force kill already requested. Waiting for process to exit...",
                    );
                }
            }
        }
    }

    fn is_output_only(&self) -> bool {
        self.layout_mode == LayoutMode::OutputOnly
    }

    fn enter_output_only(&mut self) {
        if self.layout_mode == LayoutMode::Split {
            self.previous_focus_panel = self.focused_panel;
        }
        self.layout_mode = LayoutMode::OutputOnly;
        self.focused_panel = FocusPanel::Output;
    }

    fn exit_output_only(&mut self) {
        if self.layout_mode == LayoutMode::Split {
            return;
        }

        self.layout_mode = LayoutMode::Split;
        self.focused_panel = self.previous_focus_panel;
    }

    fn toggle_output_only(&mut self) {
        if self.is_output_only() {
            self.exit_output_only();
        } else {
            self.enter_output_only();
        }
    }
}

#[derive(Debug)]
enum WorkerEvent {
    OutputLine(String),
    AccountAuthUpdate {
        account_idx: usize,
        status: AuthStatus,
        message: String,
    },
    WorkspacesLoaded {
        account_idx: usize,
        workspaces: Vec<String>,
    },
    OperationFinished {
        kind: OperationKind,
        account_idx: usize,
        success: bool,
        cancelled: bool,
        message: String,
    },
}

#[derive(Debug)]
struct RunOutcome {
    success: bool,
    cancelled: bool,
    exit_code: Option<i32>,
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    let cli_options = parse_cli_options()?;
    let cwd = std::env::current_dir().wrap_err("Unable to read current working directory")?;
    let loaded_config = load_config(&cwd, cli_options.config_path.as_deref())?;
    let mut app = AppState::from_config(loaded_config.config, &loaded_config.base_dir)?;
    app.push_output(format!(
        "Loaded config from {}",
        loaded_config.path.display()
    ));

    let (worker_tx, mut worker_rx) = mpsc::unbounded_channel::<WorkerEvent>();
    let (ctrlc_tx, mut ctrlc_rx) = mpsc::unbounded_channel::<()>();

    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = ctrlc_tx.send(());
        }
    });

    let mut terminal = setup_terminal()?;

    for idx in 0..app.accounts.len() {
        spawn_auth_check(idx, app.accounts[idx].clone(), worker_tx.clone());
    }

    let run_result = run_event_loop(
        &mut terminal,
        &mut app,
        &worker_tx,
        &mut worker_rx,
        &mut ctrlc_rx,
    );

    restore_terminal(&mut terminal)?;
    run_result
}

fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut AppState,
    worker_tx: &mpsc::UnboundedSender<WorkerEvent>,
    worker_rx: &mut mpsc::UnboundedReceiver<WorkerEvent>,
    ctrlc_rx: &mut mpsc::UnboundedReceiver<()>,
) -> Result<()> {
    loop {
        while let Ok(()) = ctrlc_rx.try_recv() {
            if app.is_busy() {
                app.request_cancel();
                app.quit_requested = true;
            } else {
                app.quit_requested = true;
            }
        }

        while let Ok(event) = worker_rx.try_recv() {
            handle_worker_event(app, event);
        }

        if app.quit_requested && !app.is_busy() {
            break;
        }

        terminal.draw(|frame| draw_ui(frame, app))?;

        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                CEvent::Key(key) if key.kind == KeyEventKind::Press => {
                    handle_key_event(app, key, worker_tx);
                }
                CEvent::Mouse(mouse) => {
                    handle_mouse_event(app, mouse);
                }
                CEvent::Resize(_, _) => {}
                _ => {}
            }
        }
    }

    Ok(())
}

fn handle_worker_event(app: &mut AppState, event: WorkerEvent) {
    match event {
        WorkerEvent::OutputLine(line) => {
            app.push_output(line);
        }
        WorkerEvent::AccountAuthUpdate {
            account_idx,
            status,
            message,
        } => {
            if let Some(account) = app.accounts.get_mut(account_idx) {
                account.auth = status;
            }
            app.push_output(message);
        }
        WorkerEvent::WorkspacesLoaded {
            account_idx,
            mut workspaces,
        } => {
            workspaces.sort();
            let mut summary_message: Option<String> = None;

            if let Some(account) = app.accounts.get_mut(account_idx) {
                account.workspaces = workspaces;
                if account.workspaces.is_empty() {
                    summary_message = Some(format!("No workspaces found for `{}`", account.name));
                } else {
                    summary_message = Some(format!(
                        "Loaded {} workspaces for `{}`",
                        account.workspaces.len(),
                        account.name
                    ));
                }
            }

            if let Some(message) = summary_message {
                app.push_output(message);
            }

            if account_idx == app.selected_account {
                app.selected_workspace = 0;
            }
        }
        WorkerEvent::OperationFinished {
            kind,
            account_idx,
            success,
            cancelled,
            message,
        } => {
            app.push_output(message);
            app.clear_apply_confirmation();

            if let Some(inflight) = &app.inflight {
                if inflight.kind == kind && inflight.account_idx == account_idx {
                    app.inflight = None;
                }
            }

            if cancelled {
                app.set_status("cancelled");
            } else if success {
                app.set_status("idle");
            } else {
                app.set_status("failed");
            }
        }
    }
}

fn handle_key_event(
    app: &mut AppState,
    key: KeyEvent,
    worker_tx: &mpsc::UnboundedSender<WorkerEvent>,
) {
    if key.code == KeyCode::Char('?') {
        app.toggle_help();
        app.clear_apply_confirmation();
        return;
    }

    if app.show_help {
        if key.code == KeyCode::Esc {
            app.close_help();
            return;
        }

        if key.code != KeyCode::Char('q') && key.code != KeyCode::Char('c') {
            return;
        }
    }

    if key.code == KeyCode::Esc {
        app.exit_output_only();
        app.clear_apply_confirmation();
        return;
    }

    if key.code == KeyCode::Char('q') {
        if app.is_busy() {
            app.request_cancel();
            app.quit_requested = true;
        } else {
            app.quit_requested = true;
        }
        return;
    }

    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if app.is_busy() {
            app.request_cancel();
            app.quit_requested = true;
        } else {
            app.quit_requested = true;
        }
        return;
    }

    if key.code == KeyCode::Char('c') {
        app.request_cancel();
        return;
    }

    match key.code {
        KeyCode::Char('z') => {
            app.toggle_output_only();
            app.clear_apply_confirmation();
        }
        KeyCode::Tab => {
            if !app.is_output_only() {
                app.focused_panel = app.focused_panel.next();
            }
        }
        KeyCode::BackTab => {
            if !app.is_output_only() {
                app.focused_panel = app.focused_panel.previous();
            }
        }
        KeyCode::Left | KeyCode::Char('h') => {
            if !app.is_output_only() {
                app.focused_panel = app.focused_panel.previous();
            }
        }
        KeyCode::Right | KeyCode::Char('l') => {
            if !app.is_output_only() {
                app.focused_panel = app.focused_panel.next();
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            move_selection_up(app);
            app.clear_apply_confirmation();
        }
        KeyCode::Down | KeyCode::Char('j') => {
            move_selection_down(app);
            app.clear_apply_confirmation();
        }
        KeyCode::PageUp => {
            if app.focused_panel == FocusPanel::Output {
                app.output_scroll_from_bottom = app.output_scroll_from_bottom.saturating_add(10);
            }
            app.clear_apply_confirmation();
        }
        KeyCode::PageDown => {
            if app.focused_panel == FocusPanel::Output {
                app.output_scroll_from_bottom = app.output_scroll_from_bottom.saturating_sub(10);
            }
            app.clear_apply_confirmation();
        }
        KeyCode::Home | KeyCode::Char('g') => {
            if app.focused_panel == FocusPanel::Output {
                app.output_scroll_from_bottom = usize::MAX;
            }
            app.clear_apply_confirmation();
        }
        KeyCode::End | KeyCode::Char('G') => {
            if app.focused_panel == FocusPanel::Output {
                app.output_scroll_from_bottom = 0;
            }
            app.clear_apply_confirmation();
        }
        KeyCode::Char('a') => {
            if app.is_busy() {
                app.push_output("Another operation is already running. Press `c` to cancel.");
                return;
            }
            start_auth_login(app, worker_tx.clone());
            app.clear_apply_confirmation();
        }
        KeyCode::Char('s') => {
            if app.is_busy() {
                app.push_output("Another operation is already running. Press `c` to cancel.");
                return;
            }
            start_auth_check_for_selected(app, worker_tx.clone());
            app.clear_apply_confirmation();
        }
        KeyCode::Char('r') => {
            if app.is_busy() {
                app.push_output("Another operation is already running. Press `c` to cancel.");
                return;
            }
            start_workspace_refresh(app, worker_tx.clone());
            app.clear_apply_confirmation();
        }
        KeyCode::Char('i') => {
            if app.is_busy() {
                app.push_output("Another operation is already running. Press `c` to cancel.");
                return;
            }
            start_terraform_operation(app, worker_tx.clone(), OperationKind::TerraformInit);
            app.clear_apply_confirmation();
        }
        KeyCode::Char('p') => {
            if app.is_busy() {
                app.push_output("Another operation is already running. Press `c` to cancel.");
                return;
            }
            start_terraform_operation(app, worker_tx.clone(), OperationKind::TerraformPlan);
            app.clear_apply_confirmation();
        }
        KeyCode::Char('A') => {
            if app.is_busy() {
                app.push_output("Another operation is already running. Press `c` to cancel.");
                return;
            }
            app.pending_apply_confirmation = true;
            app.set_status("apply confirmation pending: press y to confirm");
            app.push_output("Apply requested. Press `y` to confirm apply, any nav key to cancel.");
        }
        KeyCode::Char('y') if app.pending_apply_confirmation => {
            if app.is_busy() {
                app.push_output("Another operation is already running. Press `c` to cancel.");
                return;
            }
            start_terraform_operation(app, worker_tx.clone(), OperationKind::TerraformApply);
        }
        _ => {
            app.clear_apply_confirmation();
        }
    }
}

fn move_selection_up(app: &mut AppState) {
    match app.focused_panel {
        FocusPanel::Accounts => {
            if app.selected_account > 0 {
                app.selected_account -= 1;
                app.selected_workspace = 0;
            }
        }
        FocusPanel::Workspaces => {
            if app.selected_workspace > 0 {
                app.selected_workspace -= 1;
            }
        }
        FocusPanel::Output => {
            app.output_scroll_from_bottom = app.output_scroll_from_bottom.saturating_add(1);
        }
    }
}

fn handle_mouse_event(app: &mut AppState, mouse: MouseEvent) {
    if app.focused_panel != FocusPanel::Output {
        return;
    }

    match mouse.kind {
        MouseEventKind::ScrollUp => {
            app.output_scroll_from_bottom = app.output_scroll_from_bottom.saturating_add(3);
        }
        MouseEventKind::ScrollDown => {
            app.output_scroll_from_bottom = app.output_scroll_from_bottom.saturating_sub(3);
        }
        _ => {}
    }
}

fn move_selection_down(app: &mut AppState) {
    match app.focused_panel {
        FocusPanel::Accounts => {
            let max_idx = app.accounts.len().saturating_sub(1);
            if app.selected_account < max_idx {
                app.selected_account += 1;
                app.selected_workspace = 0;
            }
        }
        FocusPanel::Workspaces => {
            if let Some(account) = app.selected_account() {
                let max_idx = account.workspaces.len().saturating_sub(1);
                if app.selected_workspace < max_idx {
                    app.selected_workspace += 1;
                }
            }
        }
        FocusPanel::Output => {
            app.output_scroll_from_bottom = app.output_scroll_from_bottom.saturating_sub(1);
        }
    }
}

fn start_auth_check_for_selected(app: &mut AppState, event_tx: mpsc::UnboundedSender<WorkerEvent>) {
    if let Some(account) = app.selected_account().cloned() {
        let idx = app.selected_account;
        if let Some(account_mut) = app.selected_account_mut() {
            account_mut.auth = AuthStatus::Checking;
        }
        spawn_auth_check(idx, account, event_tx);
    }
}

fn start_auth_login(app: &mut AppState, event_tx: mpsc::UnboundedSender<WorkerEvent>) {
    if app.is_busy() {
        app.push_output("Another operation is already running.");
        return;
    }

    let Some(account) = app.selected_account().cloned() else {
        app.push_output("No account selected.");
        return;
    };

    let account_idx = app.selected_account;
    let (cancel_tx, cancel_rx) = watch::channel(CancelSignal::None);
    app.inflight = Some(InflightOperation {
        kind: OperationKind::AuthLogin,
        account_idx,
        cancel_tx,
        cancel_stage: CancelStage::None,
    });
    app.set_status(format!("running aws sso login for {}", account.name));

    tokio::spawn(async move {
        let _ = event_tx.send(WorkerEvent::OutputLine(format!(
            "Starting AWS SSO login for `{}` (profile `{}`)",
            account.name, account.aws_profile
        )));

        let mut login_cmd = Command::new("aws");
        login_cmd.args(["sso", "login", "--profile", &account.aws_profile]);

        let login_result = run_streaming_command(login_cmd, cancel_rx, event_tx.clone()).await;
        match login_result {
            Ok(outcome) if outcome.success => {
                let _ = event_tx.send(WorkerEvent::OutputLine(format!(
                    "SSO login complete for `{}`. Checking credentials...",
                    account.name
                )));

                match check_auth(&account).await {
                    Ok(true) => {
                        let _ = event_tx.send(WorkerEvent::AccountAuthUpdate {
                            account_idx,
                            status: AuthStatus::Authenticated,
                            message: format!("Authenticated to `{}`", account.name),
                        });

                        let _ = event_tx.send(WorkerEvent::OutputLine(format!(
                            "Loading workspaces for `{}`...",
                            account.name
                        )));

                        match fetch_workspaces(&account).await {
                            Ok(workspaces) => {
                                let _ = event_tx.send(WorkerEvent::WorkspacesLoaded {
                                    account_idx,
                                    workspaces,
                                });
                                let _ = event_tx.send(WorkerEvent::OperationFinished {
                                    kind: OperationKind::AuthLogin,
                                    account_idx,
                                    success: true,
                                    cancelled: false,
                                    message: format!("Auth/login complete for `{}`", account.name),
                                });
                            }
                            Err(err) => {
                                let _ = event_tx.send(WorkerEvent::OperationFinished {
                                    kind: OperationKind::AuthLogin,
                                    account_idx,
                                    success: false,
                                    cancelled: false,
                                    message: format!(
                                        "Authenticated, but failed to load workspaces for `{}`: {err}",
                                        account.name
                                    ),
                                });
                            }
                        }
                    }
                    Ok(false) => {
                        let _ = event_tx.send(WorkerEvent::AccountAuthUpdate {
                            account_idx,
                            status: AuthStatus::Failed,
                            message: format!(
                                "Credentials for `{}` are not usable yet",
                                account.name
                            ),
                        });
                        let _ = event_tx.send(WorkerEvent::OperationFinished {
                            kind: OperationKind::AuthLogin,
                            account_idx,
                            success: false,
                            cancelled: false,
                            message: format!("Auth check failed for `{}`", account.name),
                        });
                    }
                    Err(err) => {
                        let _ = event_tx.send(WorkerEvent::AccountAuthUpdate {
                            account_idx,
                            status: AuthStatus::Failed,
                            message: format!("Auth check errored for `{}`: {err}", account.name),
                        });
                        let _ = event_tx.send(WorkerEvent::OperationFinished {
                            kind: OperationKind::AuthLogin,
                            account_idx,
                            success: false,
                            cancelled: false,
                            message: format!("Auth check errored for `{}`", account.name),
                        });
                    }
                }
            }
            Ok(outcome) => {
                let _ = event_tx.send(WorkerEvent::AccountAuthUpdate {
                    account_idx,
                    status: AuthStatus::Failed,
                    message: format!("AWS login failed for `{}`", account.name),
                });
                let _ = event_tx.send(WorkerEvent::OperationFinished {
                    kind: OperationKind::AuthLogin,
                    account_idx,
                    success: false,
                    cancelled: outcome.cancelled,
                    message: format!(
                        "AWS login failed for `{}` with exit code {}",
                        account.name,
                        outcome.exit_code.unwrap_or(-1)
                    ),
                });
            }
            Err(err) => {
                let _ = event_tx.send(WorkerEvent::AccountAuthUpdate {
                    account_idx,
                    status: AuthStatus::Failed,
                    message: format!("Failed to run AWS login for `{}`: {err}", account.name),
                });
                let _ = event_tx.send(WorkerEvent::OperationFinished {
                    kind: OperationKind::AuthLogin,
                    account_idx,
                    success: false,
                    cancelled: false,
                    message: format!("AWS login execution failed for `{}`: {err}", account.name),
                });
            }
        }
    });
}

fn start_workspace_refresh(app: &mut AppState, event_tx: mpsc::UnboundedSender<WorkerEvent>) {
    if app.is_busy() {
        app.push_output("Another operation is already running.");
        return;
    }

    let Some(account) = app.selected_account().cloned() else {
        app.push_output("No account selected.");
        return;
    };

    if account.auth != AuthStatus::Authenticated {
        app.push_output("Selected account is not authenticated. Press `a` to run AWS SSO login.");
        return;
    }

    if let Err(err) = validate_composition_for_execution(&account) {
        app.push_output(format!(
            "Cannot refresh workspaces for `{}`: {err}",
            account.name
        ));
        app.set_status("failed");
        return;
    }

    let account_idx = app.selected_account;
    let (cancel_tx, cancel_rx) = watch::channel(CancelSignal::None);
    app.inflight = Some(InflightOperation {
        kind: OperationKind::RefreshWorkspaces,
        account_idx,
        cancel_tx,
        cancel_stage: CancelStage::None,
    });
    app.set_status(format!("loading workspaces for {}", account.name));

    tokio::spawn(async move {
        let command = terraform_command(&account, &["workspace", "list"]);
        let result = run_streaming_command(command, cancel_rx, event_tx.clone()).await;

        match result {
            Ok(outcome) if outcome.success => match fetch_workspaces(&account).await {
                Ok(workspaces) => {
                    let _ = event_tx.send(WorkerEvent::WorkspacesLoaded {
                        account_idx,
                        workspaces,
                    });
                    let _ = event_tx.send(WorkerEvent::OperationFinished {
                        kind: OperationKind::RefreshWorkspaces,
                        account_idx,
                        success: true,
                        cancelled: false,
                        message: format!("Workspace refresh completed for `{}`", account.name),
                    });
                }
                Err(err) => {
                    let _ = event_tx.send(WorkerEvent::OperationFinished {
                        kind: OperationKind::RefreshWorkspaces,
                        account_idx,
                        success: false,
                        cancelled: false,
                        message: format!("Workspace refresh failed for `{}`: {err}", account.name),
                    });
                }
            },
            Ok(outcome) => {
                let _ = event_tx.send(WorkerEvent::OperationFinished {
                    kind: OperationKind::RefreshWorkspaces,
                    account_idx,
                    success: false,
                    cancelled: outcome.cancelled,
                    message: format!(
                        "Workspace refresh command failed for `{}` with exit code {}",
                        account.name,
                        outcome.exit_code.unwrap_or(-1)
                    ),
                });
            }
            Err(err) => {
                let _ = event_tx.send(WorkerEvent::OperationFinished {
                    kind: OperationKind::RefreshWorkspaces,
                    account_idx,
                    success: false,
                    cancelled: false,
                    message: format!(
                        "Workspace refresh command failed for `{}`: {err}",
                        account.name
                    ),
                });
            }
        }
    });
}

fn start_terraform_operation(
    app: &mut AppState,
    event_tx: mpsc::UnboundedSender<WorkerEvent>,
    kind: OperationKind,
) {
    if app.is_busy() {
        app.push_output("Another operation is already running.");
        return;
    }

    let Some(account) = app.selected_account().cloned() else {
        app.push_output("No account selected.");
        return;
    };

    if account.auth != AuthStatus::Authenticated {
        app.push_output("Selected account is not authenticated. Press `a` first.");
        return;
    }

    if let Err(err) = validate_operation_preflight(&account, kind) {
        app.push_output(format!("Cannot run {}: {err}", kind.label()));
        app.set_status("failed");
        return;
    }

    let workspace = if kind.requires_workspace() {
        match app.selected_workspace_name() {
            Some(workspace) => workspace,
            None => {
                app.push_output("No workspace selected. Press `r` to load workspaces first.");
                return;
            }
        }
    } else {
        String::new()
    };

    let account_idx = app.selected_account;
    let (cancel_tx, cancel_rx) = watch::channel(CancelSignal::None);

    app.inflight = Some(InflightOperation {
        kind,
        account_idx,
        cancel_tx,
        cancel_stage: CancelStage::None,
    });
    app.set_status(format!("running {} for {}", kind.label(), account.name));

    tokio::spawn(async move {
        let run_result = run_terraform_operation(
            kind,
            account.clone(),
            workspace.clone(),
            cancel_rx,
            event_tx.clone(),
        )
        .await;

        match run_result {
            Ok(outcome) => {
                let message = if outcome.success {
                    format!("{} succeeded for `{}`", kind.label(), account.name)
                } else if outcome.cancelled {
                    format!("{} cancelled for `{}`", kind.label(), account.name)
                } else {
                    format!(
                        "{} failed for `{}` with exit code {}",
                        kind.label(),
                        account.name,
                        outcome.exit_code.unwrap_or(-1)
                    )
                };

                let _ = event_tx.send(WorkerEvent::OperationFinished {
                    kind,
                    account_idx,
                    success: outcome.success,
                    cancelled: outcome.cancelled,
                    message,
                });
            }
            Err(err) => {
                let _ = event_tx.send(WorkerEvent::OperationFinished {
                    kind,
                    account_idx,
                    success: false,
                    cancelled: false,
                    message: format!("{} failed for `{}`: {err}", kind.label(), account.name),
                });
            }
        }
    });
}

fn spawn_auth_check(
    account_idx: usize,
    account: AccountState,
    event_tx: mpsc::UnboundedSender<WorkerEvent>,
) {
    tokio::spawn(async move {
        let _ = event_tx.send(WorkerEvent::AccountAuthUpdate {
            account_idx,
            status: AuthStatus::Checking,
            message: format!(
                "Checking auth for `{}` (profile `{}`)",
                account.name, account.aws_profile
            ),
        });

        match check_auth(&account).await {
            Ok(true) => {
                let _ = event_tx.send(WorkerEvent::AccountAuthUpdate {
                    account_idx,
                    status: AuthStatus::Authenticated,
                    message: format!("Credentials valid for `{}`", account.name),
                });

                match fetch_workspaces(&account).await {
                    Ok(workspaces) => {
                        let _ = event_tx.send(WorkerEvent::WorkspacesLoaded {
                            account_idx,
                            workspaces,
                        });
                    }
                    Err(err) => {
                        let _ = event_tx.send(WorkerEvent::OutputLine(format!(
                            "Could not load workspaces for `{}` yet: {err}",
                            account.name
                        )));
                    }
                }
            }
            Ok(false) => {
                let _ = event_tx.send(WorkerEvent::AccountAuthUpdate {
                    account_idx,
                    status: AuthStatus::Failed,
                    message: format!("No valid AWS session for `{}`", account.name),
                });
            }
            Err(err) => {
                let _ = event_tx.send(WorkerEvent::AccountAuthUpdate {
                    account_idx,
                    status: AuthStatus::Failed,
                    message: format!("Auth check errored for `{}`: {err}", account.name),
                });
            }
        }
    });
}

async fn run_terraform_operation(
    kind: OperationKind,
    account: AccountState,
    workspace: String,
    cancel_rx: watch::Receiver<CancelSignal>,
    event_tx: mpsc::UnboundedSender<WorkerEvent>,
) -> Result<RunOutcome> {
    validate_operation_preflight(&account, kind)?;

    if kind.requires_workspace() {
        let _ = event_tx.send(WorkerEvent::OutputLine(format!(
            "Selecting workspace `{}` in `{}`",
            workspace, account.name
        )));

        let mut select_cmd = terraform_command(&account, &["workspace", "select", &workspace]);
        let select_out = select_cmd
            .output()
            .await
            .wrap_err("Failed to run terraform workspace select")?;
        emit_process_output(&event_tx, &select_out.stdout);
        emit_process_output(&event_tx, &select_out.stderr);
        if !select_out.status.success() {
            return Ok(RunOutcome {
                success: false,
                cancelled: false,
                exit_code: select_out.status.code(),
            });
        }
    }

    if matches!(
        kind,
        OperationKind::TerraformPlan | OperationKind::TerraformApply
    ) && !account.var_files.is_empty()
    {
        let _ = event_tx.send(WorkerEvent::OutputLine(format!(
            "Using var files: {}",
            account
                .var_files
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }

    let command = match kind {
        OperationKind::TerraformInit => {
            terraform_command(&account, &["init", "-input=false", "-no-color"])
        }
        OperationKind::TerraformPlan => {
            let mut args = vec![
                "plan".to_string(),
                "-input=false".to_string(),
                "-no-color".to_string(),
            ];
            append_var_file_args(&mut args, &account.var_files);
            terraform_command_owned(&account, &args)
        }
        OperationKind::TerraformApply => {
            let mut args = vec![
                "apply".to_string(),
                "-input=false".to_string(),
                "-no-color".to_string(),
                "-auto-approve".to_string(),
            ];
            append_var_file_args(&mut args, &account.var_files);
            terraform_command_owned(&account, &args)
        }
        _ => {
            return Err(eyre!(
                "Unsupported terraform operation for runner: {}",
                kind.label()
            ));
        }
    };

    let _ = event_tx.send(WorkerEvent::OutputLine(format!(
        "Running `{}` in {}",
        kind.label(),
        account.composition_path.display()
    )));

    run_streaming_command(command, cancel_rx, event_tx).await
}

fn emit_process_output(event_tx: &mpsc::UnboundedSender<WorkerEvent>, bytes: &[u8]) {
    for line in String::from_utf8_lossy(bytes).lines() {
        let _ = event_tx.send(WorkerEvent::OutputLine(line.to_string()));
    }
}

async fn check_auth(account: &AccountState) -> Result<bool> {
    let mut command = Command::new("aws");
    command.args([
        "sts",
        "get-caller-identity",
        "--profile",
        &account.aws_profile,
        "--output",
        "json",
    ]);

    if let Some(region) = &account.region {
        command.env("AWS_REGION", region);
        command.env("AWS_DEFAULT_REGION", region);
    }

    let output = command
        .output()
        .await
        .wrap_err("Failed to run aws sts get-caller-identity")?;

    Ok(output.status.success())
}

async fn fetch_workspaces(account: &AccountState) -> Result<Vec<String>> {
    validate_composition_for_execution(account)?;

    let mut command = terraform_command(account, &["workspace", "list"]);
    let output = command
        .output()
        .await
        .wrap_err("Failed to run terraform workspace list")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(eyre!(
            "terraform workspace list failed for {}: {}",
            account.name,
            stderr.trim()
        ));
    }

    Ok(parse_workspace_output(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

fn parse_workspace_output(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            let cleaned = line.trim().trim_start_matches('*').trim();
            if cleaned.is_empty() {
                None
            } else {
                Some(cleaned.to_string())
            }
        })
        .collect()
}

fn validate_composition_for_execution(account: &AccountState) -> Result<()> {
    if let Some(issue) = &account.composition_issue {
        return Err(eyre!(
            "Account `{}` configuration is invalid: {}",
            account.name,
            issue
        ));
    }

    if !account.composition_path.exists() {
        return Err(eyre!(
            "composition_path does not exist for `{}`: {}",
            account.name,
            account.composition_path.display()
        ));
    }

    if !account.composition_path.is_dir() {
        return Err(eyre!(
            "composition_path is not a directory for `{}`: {}",
            account.name,
            account.composition_path.display()
        ));
    }

    Ok(())
}

fn validate_var_files_for_execution(account: &AccountState) -> Result<()> {
    let missing_files: Vec<String> = account
        .var_files
        .iter()
        .filter(|path| !path.exists())
        .map(|path| path.display().to_string())
        .collect();

    if missing_files.is_empty() {
        Ok(())
    } else {
        Err(eyre!(
            "Configured var_files are missing for `{}`: {}",
            account.name,
            missing_files.join(", ")
        ))
    }
}

fn validate_operation_preflight(account: &AccountState, kind: OperationKind) -> Result<()> {
    validate_composition_for_execution(account)?;

    if matches!(
        kind,
        OperationKind::TerraformPlan | OperationKind::TerraformApply
    ) && !account.var_files.is_empty()
    {
        validate_var_files_for_execution(account)?;
    }

    Ok(())
}

fn append_var_file_args(args: &mut Vec<String>, var_files: &[PathBuf]) {
    for var_file in var_files {
        args.push(format!("-var-file={}", var_file.display()));
    }
}

fn terraform_base_command(account: &AccountState) -> Command {
    let mut command = Command::new("terraform");
    command.current_dir(&account.composition_path);
    command.env("AWS_PROFILE", &account.aws_profile);
    command.env("AWS_SDK_LOAD_CONFIG", "1");
    command.env("TF_IN_AUTOMATION", "1");

    if let Some(region) = &account.region {
        command.env("AWS_REGION", region);
        command.env("AWS_DEFAULT_REGION", region);
    }

    command
}

fn terraform_command(account: &AccountState, args: &[&str]) -> Command {
    let mut command = terraform_base_command(account);
    command.args(args);
    command
}

fn terraform_command_owned(account: &AccountState, args: &[String]) -> Command {
    let mut command = terraform_base_command(account);
    command.args(args);
    command
}

async fn run_streaming_command(
    mut command: Command,
    mut cancel_rx: watch::Receiver<CancelSignal>,
    event_tx: mpsc::UnboundedSender<WorkerEvent>,
) -> Result<RunOutcome> {
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    let mut child = command.spawn().wrap_err("Failed to spawn command")?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| eyre!("Command stdout was not piped"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| eyre!("Command stderr was not piped"))?;

    let tx_stdout = event_tx.clone();
    let tx_stderr = event_tx.clone();

    let stdout_task = tokio::spawn(async move { stream_reader(stdout, tx_stdout).await });
    let stderr_task = tokio::spawn(async move { stream_reader(stderr, tx_stderr).await });

    let mut cancelled = false;
    let mut sigint_sent = false;
    let mut force_kill_sent = false;

    let status = loop {
        tokio::select! {
            child_status = child.wait() => {
                break child_status.wrap_err("Failed while waiting for command")?;
            }
            changed = cancel_rx.changed() => {
                if changed.is_ok() {
                    match *cancel_rx.borrow() {
                        CancelSignal::None => {}
                        CancelSignal::Graceful => {
                            cancelled = true;
                            if !sigint_sent {
                                if let Some(pid) = child.id() {
                                    send_sigint(pid)?;
                                    let _ = event_tx.send(WorkerEvent::OutputLine("Sent SIGINT to running command.".to_string()));
                                }
                                sigint_sent = true;
                            }
                        }
                        CancelSignal::Force => {
                            cancelled = true;
                            if !force_kill_sent {
                                let _ = event_tx.send(WorkerEvent::OutputLine("Force kill signal sent to running command.".to_string()));
                                let _ = child.start_kill();
                                force_kill_sent = true;
                            }
                        }
                    }
                }
            }
        }
    };

    let _ = stdout_task.await;
    let _ = stderr_task.await;

    Ok(RunOutcome {
        success: status.success(),
        cancelled,
        exit_code: status.code(),
    })
}

async fn stream_reader<R>(reader: R, event_tx: mpsc::UnboundedSender<WorkerEvent>) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        let _ = event_tx.send(WorkerEvent::OutputLine(line));
    }
    Ok(())
}

#[cfg(unix)]
fn send_sigint(pid: u32) -> Result<()> {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;

    let pid_i32 = i32::try_from(pid).wrap_err("child PID overflowed i32")?;
    kill(Pid::from_raw(pid_i32), Signal::SIGINT).wrap_err("failed to send SIGINT")?;
    Ok(())
}

#[cfg(not(unix))]
fn send_sigint(_pid: u32) -> Result<()> {
    Ok(())
}

fn draw_ui(frame: &mut ratatui::Frame<'_>, app: &AppState) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(10),
            Constraint::Length(2),
        ])
        .split(frame.area());

    let title = Line::from(vec![
        Span::styled(
            " lazytf ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            "| {} | mode: {} | focus: {:?}",
            app.current_operation_label(),
            app.layout_mode.label(),
            app.focused_panel
        )),
    ]);
    frame.render_widget(title, root[0]);

    if app.is_output_only() {
        draw_output_only_layout(frame, app, root[1]);
    } else {
        draw_split_layout(frame, app, root[1]);
    }

    let help = if app.is_output_only() {
        vec![
            Line::from(
                "z/esc:exit fullscreen  ?:help  pgup/pgdn g/G mouse:scroll  c:cancel (again=force)  q:quit",
            ),
            Line::from("output-only mode for plan review"),
        ]
    } else {
        vec![
            Line::from(
                "j/k or arrows: move  tab/h/l: panel  z:fullscreen output  ?:help  a:aws login  s:auth check  r:workspaces",
            ),
            Line::from(
                "i:init  p:plan  A then y:apply  c:cancel (again=force)  q:quit  pgup/pgdn g/G/mouse:output scroll",
            ),
        ]
    };
    frame.render_widget(Paragraph::new(help), root[2]);

    if app.pending_apply_confirmation {
        draw_apply_confirmation(frame);
    }

    if app.show_help {
        draw_help_modal(frame);
    }
}

fn draw_split_layout(frame: &mut ratatui::Frame<'_>, app: &AppState, area: Rect) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(28),
            Constraint::Percentage(28),
            Constraint::Percentage(44),
        ])
        .split(area);

    draw_accounts_panel(frame, app, columns[0]);
    draw_workspaces_panel(frame, app, columns[1]);
    draw_output_panel(frame, app, columns[2]);
}

fn draw_output_only_layout(frame: &mut ratatui::Frame<'_>, app: &AppState, area: Rect) {
    draw_output_panel(frame, app, area);
}

fn draw_accounts_panel(frame: &mut ratatui::Frame<'_>, app: &AppState, area: Rect) {
    let border_style = if app.focused_panel == FocusPanel::Accounts {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };

    let items: Vec<ListItem<'_>> = app
        .accounts
        .iter()
        .enumerate()
        .map(|(idx, account)| {
            let selected = if idx == app.selected_account {
                ">"
            } else {
                " "
            };
            let line = Line::from(vec![
                Span::raw(format!("{selected} ")),
                Span::styled(
                    account.auth.icon(),
                    Style::default().fg(account.auth.color()),
                ),
                Span::raw(format!(" {} [{}]", account.name, account.auth.label())),
            ]);
            ListItem::new(line)
        })
        .collect();

    let widget = List::new(items).block(
        Block::default()
            .title("Accounts")
            .borders(Borders::ALL)
            .border_style(border_style),
    );

    frame.render_widget(widget, area);
}

fn draw_workspaces_panel(frame: &mut ratatui::Frame<'_>, app: &AppState, area: Rect) {
    let border_style = if app.focused_panel == FocusPanel::Workspaces {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };

    let items: Vec<ListItem<'_>> = if let Some(account) = app.selected_account() {
        if account.workspaces.is_empty() {
            vec![ListItem::new("  (no workspaces loaded)")]
        } else {
            account
                .workspaces
                .iter()
                .enumerate()
                .map(|(idx, workspace)| {
                    let selected = if idx == app.selected_workspace {
                        ">"
                    } else {
                        " "
                    };
                    ListItem::new(format!("{selected} {workspace}"))
                })
                .collect()
        }
    } else {
        vec![ListItem::new("  (no account selected)")]
    };

    let widget = List::new(items).block(
        Block::default()
            .title("Workspaces")
            .borders(Borders::ALL)
            .border_style(border_style),
    );

    frame.render_widget(widget, area);
}

fn draw_output_panel(frame: &mut ratatui::Frame<'_>, app: &AppState, area: Rect) {
    let border_style = if app.focused_panel == FocusPanel::Output {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };

    let visible_rows = area.height.saturating_sub(2) as usize;
    let total_lines = app.output_lines.len();
    let max_scroll_from_bottom = total_lines.saturating_sub(visible_rows);
    let from_bottom = app.output_scroll_from_bottom.min(max_scroll_from_bottom);
    let scroll_from_top = max_scroll_from_bottom.saturating_sub(from_bottom);

    let text: Vec<Line<'_>> = app
        .output_lines
        .iter()
        .map(|line| styled_output_line(line))
        .collect();

    let output_title = if from_bottom == 0 {
        "Output".to_string()
    } else {
        format!("Output (scroll +{from_bottom})")
    };

    let widget = Paragraph::new(text)
        .scroll((scroll_from_top as u16, 0))
        .block(
            Block::default()
                .title(output_title)
                .borders(Borders::ALL)
                .border_style(border_style),
        );

    frame.render_widget(widget, area);
}

fn styled_output_line(line: &str) -> Line<'static> {
    let trimmed = line.trim_start();

    let style = if trimmed.contains("Error:") {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else if trimmed.contains("Warning:") {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else if trimmed.starts_with('+') {
        Style::default().fg(Color::Green)
    } else if trimmed.starts_with('~') {
        Style::default().fg(Color::Yellow)
    } else if trimmed.starts_with('-') {
        Style::default().fg(Color::Red)
    } else if trimmed.starts_with("Plan:") {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else if trimmed.starts_with("Apply complete!") || trimmed.starts_with("No changes.") {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    } else if trimmed.starts_with("Running `") || trimmed.starts_with("Using var files:") {
        Style::default().fg(Color::Blue)
    } else {
        Style::default()
    };

    Line::from(Span::styled(line.to_string(), style))
}

fn draw_apply_confirmation(frame: &mut ratatui::Frame<'_>) {
    let area = centered_rect(65, 20, frame.area());
    frame.render_widget(Clear, area);
    let popup = Paragraph::new(vec![
        Line::from("Apply confirmation"),
        Line::from(""),
        Line::from("Press `y` to run terraform apply"),
        Line::from("Use any navigation key to cancel"),
    ])
    .block(
        Block::default()
            .title("Confirm")
            .borders(Borders::ALL)
            .border_style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
    );
    frame.render_widget(popup, area);
}

fn draw_help_modal(frame: &mut ratatui::Frame<'_>) {
    let area = centered_rect(82, 70, frame.area());
    frame.render_widget(Clear, area);

    let help_lines = vec![
        Line::from(Span::styled(
            "lazytf keybindings",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("Global:"),
        Line::from("  ?: toggle help   q: quit   Ctrl+C: graceful quit"),
        Line::from("  c: cancel running command (press again to force kill)"),
        Line::from(""),
        Line::from("Layout & Focus:"),
        Line::from("  z: toggle output fullscreen   Esc: exit fullscreen/help"),
        Line::from("  Tab/Shift+Tab or h/l: move focus between panels"),
        Line::from(""),
        Line::from("Navigation:"),
        Line::from("  j/k or arrows: move selection   g/G or Home/End: output top/bottom"),
        Line::from("  PgUp/PgDn or mouse wheel: scroll output"),
        Line::from(""),
        Line::from("Actions:"),
        Line::from("  a: aws sso login   s: auth check   r: refresh workspaces"),
        Line::from("  i: terraform init   p: terraform plan   A then y: terraform apply"),
    ];

    let popup = Paragraph::new(help_lines).block(
        Block::default()
            .title("Help")
            .borders(Borders::ALL)
            .border_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
    );
    frame.render_widget(popup, area);
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

fn parse_cli_options() -> Result<CliOptions> {
    let mut options = CliOptions::default();
    let mut args = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-c" | "--config" => {
                let value = args.next().ok_or_else(|| {
                    eyre!("Missing value for {arg}. Usage: lazytf --config <path>")
                })?;
                options.config_path = Some(PathBuf::from(value));
            }
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            _ => {
                return Err(eyre!(
                    "Unknown argument `{arg}`. Usage: lazytf [--config <path>]"
                ));
            }
        }
    }

    Ok(options)
}

fn print_usage() {
    println!("lazytf - terminal UI for Terraform workflows");
    println!();
    println!("Usage:");
    println!("  lazytf [--config <path>]");
    println!();
    println!("Options:");
    println!("  -c, --config <path>   Path to lazytf config YAML");
    println!("  -h, --help            Show this help");
}

fn load_config(cwd: &Path, explicit_config: Option<&Path>) -> Result<LoadedConfig> {
    let config_path = find_config_path(cwd, explicit_config)?;
    let config_path = config_path
        .canonicalize()
        .unwrap_or_else(|_| config_path.clone());
    let contents = fs::read_to_string(&config_path).wrap_err_with(|| {
        format!(
            "Failed to read config file at {}",
            config_path.to_string_lossy()
        )
    })?;

    let config: Config = serde_yaml::from_str(&contents).wrap_err_with(|| {
        format!(
            "Failed to parse YAML config at {}",
            config_path.to_string_lossy()
        )
    })?;

    let base_dir = config_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| cwd.to_path_buf());

    Ok(LoadedConfig {
        path: config_path,
        base_dir,
        config,
    })
}

fn find_config_path(cwd: &Path, explicit_config: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit_config {
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };

        if resolved.exists() {
            return Ok(resolved);
        }

        return Err(eyre!("Config file does not exist: {}", resolved.display()));
    }

    for candidate in CONFIG_CANDIDATES {
        let path = cwd.join(candidate);
        if path.exists() {
            return Ok(path);
        }
    }

    Err(eyre!(
        "No config file found. Expected one of: {}",
        CONFIG_CANDIDATES.join(", ")
    ))
}

fn resolve_composition_path(cwd: &Path, raw_path: &str) -> Result<PathBuf> {
    let has_glob = raw_path.contains('*') || raw_path.contains('?') || raw_path.contains('[');
    if has_glob {
        let absolute_pattern = if Path::new(raw_path).is_absolute() {
            raw_path.to_string()
        } else {
            cwd.join(raw_path).to_string_lossy().to_string()
        };

        let mut matches: Vec<PathBuf> = glob(&absolute_pattern)
            .wrap_err_with(|| format!("Invalid glob pattern: {absolute_pattern}"))?
            .filter_map(|entry| entry.ok())
            .filter(|path| path.is_dir())
            .collect();

        matches.sort();
        return matches.into_iter().next().ok_or_else(|| {
            eyre!(
                "Path pattern `{raw_path}` did not match any directories from {}",
                cwd.display()
            )
        });
    }

    let path = if Path::new(raw_path).is_absolute() {
        PathBuf::from(raw_path)
    } else {
        cwd.join(raw_path)
    };

    if !path.exists() {
        return Err(eyre!(
            "Configured composition_path does not exist: {}",
            path.display()
        ));
    }
    if !path.is_dir() {
        return Err(eyre!(
            "Configured composition_path is not a directory: {}",
            path.display()
        ));
    }

    Ok(path)
}

fn resolve_var_file_paths(raw_var_files: &[String], composition_path: &Path) -> Vec<PathBuf> {
    raw_var_files
        .iter()
        .map(|raw| {
            let raw_path = Path::new(raw);
            if raw_path.is_absolute() {
                raw_path.to_path_buf()
            } else {
                composition_path.join(raw_path)
            }
        })
        .collect()
}

fn fallback_composition_path(cwd: &Path, raw_path: &str) -> PathBuf {
    if raw_path.contains('*') || raw_path.contains('?') || raw_path.contains('[') {
        return cwd.to_path_buf();
    }

    if Path::new(raw_path).is_absolute() {
        PathBuf::from(raw_path)
    } else {
        cwd.join(raw_path)
    }
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode().wrap_err("Failed to enable terminal raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
        .wrap_err("Failed to enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend).wrap_err("Failed to initialize terminal backend")?;
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    disable_raw_mode().wrap_err("Failed to disable terminal raw mode")?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )
    .wrap_err("Failed to leave alternate screen")?;
    terminal.show_cursor().wrap_err("Failed to show cursor")?;
    Ok(())
}
