//! Interactive terminal dashboard (ratatui).
//!
//! Tabs: Status, Backends, Gateway, GPUs, Logs. Data refreshes on a
//! 5 s tick and on `r`. `q` / Esc quits. Field extraction from API
//! responses is defensive: the dashboard renders what is present
//! and falls back to raw JSON, so a new server field (or a missing
//! one) never panics the UI.

use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEventKind};
use manager_cli::{ApiClient, ApiError};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, Tabs};
use ratatui::Frame;
use serde_json::Value;
use tokio::sync::mpsc;

const TABS: [&str; 5] = ["Status", "Backends", "Gateway", "GPUs", "Logs"];
const REFRESH_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Default)]
struct Snapshots {
    server: Option<Value>,
    status: Option<Value>,
    backends: Option<Value>,
    gateway: Option<Value>,
    gpus: Option<Value>,
    logs: Option<Value>,
}

struct App {
    tab: usize,
    snaps: Snapshots,
    last_error: Option<String>,
    refreshes: u64,
}

impl App {
    fn new() -> Self {
        Self {
            tab: 0,
            snaps: Snapshots::default(),
            last_error: None,
            refreshes: 0,
        }
    }

    /// Sequential fetch of every tab's data. A failed fetch records
    /// the error and keeps the previous snapshot for that tab.
    async fn refresh(&mut self, client: &mut ApiClient) {
        self.refreshes += 1;
        let mut fetch = async |path: &str| client.get(path).await;
        match fetch("/api/v1/server").await {
            Ok(v) => self.snaps.server = Some(v),
            Err(e) => self.last_error = Some(format!("server: {e}")),
        }
        match fetch("/api/v1/status").await {
            Ok(v) => self.snaps.status = Some(v),
            Err(e) => self.last_error = Some(format!("status: {e}")),
        }
        match fetch("/api/v1/backends").await {
            Ok(v) => self.snaps.backends = Some(v),
            Err(e) => self.last_error = Some(format!("backends: {e}")),
        }
        match fetch("/api/v1/gateway/status").await {
            Ok(v) => self.snaps.gateway = Some(v),
            Err(e) => self.last_error = Some(format!("gateway: {e}")),
        }
        match fetch("/api/v1/telemetry/gpus").await {
            Ok(v) => self.snaps.gpus = Some(v),
            Err(e) => self.last_error = Some(format!("gpus: {e}")),
        }
        match fetch("/api/v1/logs?scope=all&limit=50").await {
            Ok(v) => self.snaps.logs = Some(v),
            Err(e) => self.last_error = Some(format!("logs: {e}")),
        }
        if self.snaps.server.is_some()
            && self.snaps.status.is_some()
            && self.snaps.backends.is_some()
            && self.snaps.gateway.is_some()
            && self.snaps.gpus.is_some()
            && self.snaps.logs.is_some()
        {
            self.last_error = None;
        }
    }
}

fn as_array(value: &Value) -> Option<&Vec<Value>> {
    match value {
        Value::Array(items) => Some(items),
        Value::Object(map) => map
            .get("backends")
            .or_else(|| map.get("gpus"))
            .or_else(|| map.get("items"))
            .and_then(Value::as_array),
        _ => None,
    }
}

fn field<'a>(item: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|k| item.get(*k))
}

fn field_str<'a>(item: &'a Value, keys: &[&str]) -> &'a str {
    field(item, keys).and_then(Value::as_str).unwrap_or("—")
}

fn field_num(item: &Value, keys: &[&str]) -> String {
    match field(item, keys) {
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s.clone(),
        _ => "—".to_string(),
    }
}

fn render_json_lines(value: &Option<Value>, max_lines: usize) -> Vec<String> {
    match value {
        None => vec!["(no data yet)".to_string()],
        Some(v) => {
            let pretty = serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string());
            let mut lines: Vec<String> = pretty.lines().map(str::to_string).collect();
            if lines.len() > max_lines {
                lines.truncate(max_lines);
                lines.push(format!("… ({} more lines)", pretty.lines().count() - max_lines));
            }
            lines
        }
    }
}

fn draw(frame: &mut Frame, app: &App, base_url: &str) {
    let area = frame.area();
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(area);

    let title = format!(
        "llmdil-cli — {}  (refresh #{})",
        base_url, app.refreshes
    );
    frame.render_widget(Paragraph::new(title), chunks[0]);

    let tabs = Tabs::new(TABS)
        .select(app.tab)
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_widget(tabs, chunks[1]);

    match app.tab {
        0 => {
            let mut lines = vec!["Server identity:".to_string()];
            lines.extend(render_json_lines(&app.snaps.server, 8));
            lines.push(String::new());
            lines.push("Status:".to_string());
            lines.extend(render_json_lines(&app.snaps.status, 12));
            frame.render_widget(
                Paragraph::new(lines.join("\n"))
                    .block(Block::default().borders(Borders::ALL).title("Status")),
                chunks[2],
            );
        }
        1 => {
            let rows: Vec<Row> = match app.snaps.backends.as_ref().and_then(as_array) {
                Some(items) => items
                    .iter()
                    .map(|item| {
                        Row::new(vec![
                            Cell::from(field_str(item, &["id"]).to_string()),
                            Cell::from(field_str(item, &["state", "status"]).to_string()),
                            Cell::from(field_str(item, &["model_id", "model"]).to_string()),
                            Cell::from(field_num(item, &["port"])),
                        ])
                    })
                    .collect(),
                None => Vec::new(),
            };
            let table = Table::new(
                rows,
                [
                    Constraint::Length(20),
                    Constraint::Length(14),
                    Constraint::Min(20),
                    Constraint::Length(8),
                ],
            )
            .header(Row::new(vec!["Id", "State", "Model", "Port"]))
            .block(Block::default().borders(Borders::ALL).title("Backends"));
            frame.render_widget(table, chunks[2]);
        }
        2 => {
            let lines = render_json_lines(&app.snaps.gateway, 30);
            frame.render_widget(
                Paragraph::new(lines.join("\n"))
                    .block(Block::default().borders(Borders::ALL).title("Gateway")),
                chunks[2],
            );
        }
        3 => {
            let rows: Vec<Row> = match app.snaps.gpus.as_ref().and_then(as_array) {
                Some(items) => items
                    .iter()
                    .map(|item| {
                        Row::new(vec![
                            Cell::from(field_str(item, &["name", "id"]).to_string()),
                            Cell::from(field_num(item, &["index"])),
                            Cell::from(field_num(item, &["utilization", "util"])),
                            Cell::from(field_num(item, &["memory_used", "used"])),
                            Cell::from(field_num(item, &["memory_total", "total"])),
                        ])
                    })
                    .collect(),
                None => Vec::new(),
            };
            let table = Table::new(
                rows,
                [
                    Constraint::Min(16),
                    Constraint::Length(8),
                    Constraint::Length(10),
                    Constraint::Length(14),
                    Constraint::Length(14),
                ],
            )
            .header(Row::new(vec!["GPU", "Index", "Util", "Used", "Total"]))
            .block(Block::default().borders(Borders::ALL).title("GPUs"));
            frame.render_widget(table, chunks[2]);
        }
        _ => {
            let lines = match &app.snaps.logs {
                None => vec!["(no data yet)".to_string()],
                Some(v) => {
                    let mut out = Vec::new();
                    if let Some(items) = v
                        .get("lines")
                        .and_then(Value::as_array)
                        .or_else(|| v.as_array())
                    {
                        for item in items {
                            let text = item
                                .get("line")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                                .unwrap_or_else(|| {
                                    serde_json::to_string(item)
                                        .unwrap_or_default()
                                });
                            out.push(text);
                        }
                    } else {
                        out.extend(render_json_lines(&app.snaps.logs, 30));
                    }
                    out
                }
            };
            frame.render_widget(
                Paragraph::new(lines.join("\n"))
                    .block(Block::default().borders(Borders::ALL).title("Logs")),
                chunks[2],
            );
        }
    }

    let footer = match &app.last_error {
        Some(e) => format!("Tab/1-5 switch · r refresh · q quit — last error: {e}"),
        None => "Tab/1-5 switch · r refresh · q quit".to_string(),
    };
    frame.render_widget(Paragraph::new(footer), chunks[3]);
}

pub async fn run(mut client: ApiClient) -> Result<(), ApiError> {
    let base_url = client.base_url().to_string();
    let mut terminal = ratatui::init();
    let result = run_loop(&mut terminal, &mut client, &base_url).await;
    ratatui::restore();
    result
}

async fn run_loop(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    client: &mut ApiClient,
    base_url: &str,
) -> Result<(), ApiError> {
    // Key events arrive on a channel from a blocking reader thread:
    // crossterm's EventStream would need futures-util's StreamExt,
    // which this workspace does not carry.
    let (tx, mut rx) = mpsc::unbounded_channel::<Event>();
    std::thread::spawn(move || {
        while let Ok(event) = crossterm::event::read() {
            if tx.send(event).is_err() {
                break;
            }
        }
    });

    let mut app = App::new();
    app.refresh(client).await;
    terminal
        .draw(|frame| draw(frame, &app, base_url))
        .map_err(|e| ApiError::Input(format!("terminal draw failed: {e}")))?;

    let mut tick = tokio::time::interval(REFRESH_INTERVAL);
    tick.tick().await; // consume the immediate first tick
    loop {
        tokio::select! {
            _ = tick.tick() => {
                app.refresh(client).await;
                terminal
                    .draw(|frame| draw(frame, &app, base_url))
                    .map_err(|e| ApiError::Input(format!("terminal draw failed: {e}")))?;
            }
            event = rx.recv() => {
                let Some(Event::Key(key)) = event else { continue };
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('r') => {
                        app.refresh(client).await;
                    }
                    KeyCode::Tab => {
                        app.tab = (app.tab + 1) % TABS.len();
                    }
                    KeyCode::Char(c @ '1'..='5') => {
                        app.tab = (c as usize) - ('1' as usize);
                    }
                    _ => {}
                }
                terminal
                    .draw(|frame| draw(frame, &app, base_url))
                    .map_err(|e| ApiError::Input(format!("terminal draw failed: {e}")))?;
            }
        }
    }
    Ok(())
}
