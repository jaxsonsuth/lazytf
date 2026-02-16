use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use color_eyre::eyre::{Result, WrapErr, eyre};
use crossterm::{
    event::{self, Event as CEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
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
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
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
}

#[derive(Debug, Clone)]
struct AccountState {
    name: String,
    aws_profile: String,
    region: Option<String>,
    composition_path: PathBuf,
    auth: AuthStatus,
    workspaces: Vec<String>,
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

#[derive(Debug)]
struct InflightOperation {
    kind: OperationKind,
    account_idx: usize,
    cancel_tx: watch::Sender<bool>,
}

#[derive(Debug)]
struct AppState {
    accounts: Vec<AccountState>,
    selected_account: usize,
    selected_workspace: usize,
    focused_panel: FocusPanel,
    output_lines: Vec<String>,
    output_scroll_from_bottom: usize,
    status_line: String,
    inflight: Option<InflightOperation>,
    pending_apply_confirmation: bool,
    quit_requested: bool,
}

impl AppState {
    fn from_config(config: Config, cwd: &Path) -> Result<Self> {
        if config.accounts.is_empty() {
            return Err(eyre!(
                "Config has no accounts. Add at least one account under `accounts:`"
            ));
        }

        let mut accounts = Vec::with_capacity(config.accounts.len());
        let mut startup_lines =
            vec!["lazytf ready. Press `a` to authenticate selected account.".to_string()];

        for (name, account_cfg) in config.accounts {
            let composition_path =
                match resolve_composition_path(cwd, &account_cfg.composition_path) {
                    Ok(path) => path,
                    Err(err) => {
                        let fallback =
                            fallback_composition_path(cwd, &account_cfg.composition_path);
                        startup_lines.push(format!(
                            "warning: account `{name}` composition_path unresolved (`{}`): {err}",
                            account_cfg.composition_path
                        ));
                        startup_lines.push(format!(
                            "warning: using fallback path `{}` so UI can start",
                            fallback.display()
                        ));
                        fallback
                    }
                };

            accounts.push(AccountState {
                name,
                aws_profile: account_cfg.aws_profile,
                region: account_cfg.region,
                composition_path,
                auth: AuthStatus::Unknown,
                workspaces: Vec::new(),
            });
        }

        Ok(Self {
            accounts,
            selected_account: 0,
            selected_workspace: 0,
            focused_panel: FocusPanel::Accounts,
            output_lines: startup_lines,
            output_scroll_from_bottom: 0,
            status_line: "idle".to_string(),
            inflight: None,
            pending_apply_confirmation: false,
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

    fn request_cancel(&mut self) {
        if let Some(op) = &self.inflight {
            let _ = op.cancel_tx.send(true);
            self.push_output("Cancellation requested. Sending SIGINT to running command...");
            self.set_status("cancelling...");
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

    let cwd = std::env::current_dir().wrap_err("Unable to read current working directory")?;
    let config = load_config(&cwd)?;
    let mut app = AppState::from_config(config, &cwd)?;

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

    if app.is_busy() {
        return;
    }

    match key.code {
        KeyCode::Tab => {
            app.focused_panel = app.focused_panel.next();
        }
        KeyCode::BackTab => {
            app.focused_panel = app.focused_panel.previous();
        }
        KeyCode::Left | KeyCode::Char('h') => {
            app.focused_panel = app.focused_panel.previous();
        }
        KeyCode::Right | KeyCode::Char('l') => {
            app.focused_panel = app.focused_panel.next();
        }
        KeyCode::Up | KeyCode::Char('k') => {
            move_selection_up(app);
            app.clear_apply_confirmation();
        }
        KeyCode::Down | KeyCode::Char('j') => {
            move_selection_down(app);
            app.clear_apply_confirmation();
        }
        KeyCode::Char('a') => {
            start_auth_login(app, worker_tx.clone());
            app.clear_apply_confirmation();
        }
        KeyCode::Char('s') => {
            start_auth_check_for_selected(app, worker_tx.clone());
            app.clear_apply_confirmation();
        }
        KeyCode::Char('r') => {
            start_workspace_refresh(app, worker_tx.clone());
            app.clear_apply_confirmation();
        }
        KeyCode::Char('i') => {
            start_terraform_operation(app, worker_tx.clone(), OperationKind::TerraformInit);
            app.clear_apply_confirmation();
        }
        KeyCode::Char('p') => {
            start_terraform_operation(app, worker_tx.clone(), OperationKind::TerraformPlan);
            app.clear_apply_confirmation();
        }
        KeyCode::Char('A') => {
            app.pending_apply_confirmation = true;
            app.set_status("apply confirmation pending: press y to confirm");
            app.push_output("Apply requested. Press `y` to confirm apply, any nav key to cancel.");
        }
        KeyCode::Char('y') if app.pending_apply_confirmation => {
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
    let (cancel_tx, cancel_rx) = watch::channel(false);
    app.inflight = Some(InflightOperation {
        kind: OperationKind::AuthLogin,
        account_idx,
        cancel_tx,
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

    let account_idx = app.selected_account;
    let (cancel_tx, cancel_rx) = watch::channel(false);
    app.inflight = Some(InflightOperation {
        kind: OperationKind::RefreshWorkspaces,
        account_idx,
        cancel_tx,
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
    let (cancel_tx, cancel_rx) = watch::channel(false);

    app.inflight = Some(InflightOperation {
        kind,
        account_idx,
        cancel_tx,
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
    cancel_rx: watch::Receiver<bool>,
    event_tx: mpsc::UnboundedSender<WorkerEvent>,
) -> Result<RunOutcome> {
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

    let command = match kind {
        OperationKind::TerraformInit => {
            terraform_command(&account, &["init", "-input=false", "-no-color"])
        }
        OperationKind::TerraformPlan => {
            terraform_command(&account, &["plan", "-input=false", "-no-color"])
        }
        OperationKind::TerraformApply => terraform_command(
            &account,
            &["apply", "-input=false", "-no-color", "-auto-approve"],
        ),
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

fn terraform_command(account: &AccountState, args: &[&str]) -> Command {
    let mut command = Command::new("terraform");
    command.args(args);
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

async fn run_streaming_command(
    mut command: Command,
    mut cancel_rx: watch::Receiver<bool>,
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

    let status = loop {
        tokio::select! {
            child_status = child.wait() => {
                break child_status.wrap_err("Failed while waiting for command")?;
            }
            changed = cancel_rx.changed(), if !sigint_sent => {
                if changed.is_ok() && *cancel_rx.borrow() {
                    cancelled = true;
                    if let Some(pid) = child.id() {
                        send_sigint(pid)?;
                        let _ = event_tx.send(WorkerEvent::OutputLine("Sent SIGINT to running command.".to_string()));
                        sigint_sent = true;
                    }
                }
            }
            _ = tokio::time::sleep(Duration::from_secs(8)), if sigint_sent => {
                let _ = event_tx.send(WorkerEvent::OutputLine("Command still running after SIGINT, forcing termination...".to_string()));
                let _ = child.start_kill();
                sigint_sent = false;
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
            "| {} | focus: {:?}",
            app.current_operation_label(),
            app.focused_panel
        )),
    ]);
    frame.render_widget(title, root[0]);

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(28),
            Constraint::Percentage(28),
            Constraint::Percentage(44),
        ])
        .split(root[1]);

    draw_accounts_panel(frame, app, columns[0]);
    draw_workspaces_panel(frame, app, columns[1]);
    draw_output_panel(frame, app, columns[2]);

    let help = vec![
        Line::from("j/k or arrows: move  tab/h/l: panel  a:aws login  s:auth check  r:workspaces"),
        Line::from("i:init  p:plan  A then y:apply  c:cancel  q:quit"),
    ];
    frame.render_widget(Paragraph::new(help), root[2]);

    if app.pending_apply_confirmation {
        draw_apply_confirmation(frame);
    }
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
    let max_scroll = total_lines.saturating_sub(visible_rows);
    let scroll = app.output_scroll_from_bottom.min(max_scroll);

    let end = total_lines.saturating_sub(scroll);
    let start = end.saturating_sub(visible_rows);

    let text: Vec<Line<'_>> = app.output_lines[start..end]
        .iter()
        .map(|line| Line::from(line.as_str()))
        .collect();

    let widget = Paragraph::new(text)
        .block(
            Block::default()
                .title("Output")
                .borders(Borders::ALL)
                .border_style(border_style),
        )
        .wrap(Wrap { trim: false });

    frame.render_widget(widget, area);
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

fn load_config(cwd: &Path) -> Result<Config> {
    let config_path = find_config_path(cwd)?;
    let contents = fs::read_to_string(&config_path).wrap_err_with(|| {
        format!(
            "Failed to read config file at {}",
            config_path.to_string_lossy()
        )
    })?;

    serde_yaml::from_str(&contents).wrap_err_with(|| {
        format!(
            "Failed to parse YAML config at {}",
            config_path.to_string_lossy()
        )
    })
}

fn find_config_path(cwd: &Path) -> Result<PathBuf> {
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
    execute!(stdout, EnterAlternateScreen).wrap_err("Failed to enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend).wrap_err("Failed to initialize terminal backend")?;
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    disable_raw_mode().wrap_err("Failed to disable terminal raw mode")?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)
        .wrap_err("Failed to leave alternate screen")?;
    terminal.show_cursor().wrap_err("Failed to show cursor")?;
    Ok(())
}
