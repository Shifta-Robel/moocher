//! `moo tui`: the dashboard's core views in a terminal (over SSH, too).
//!
//! Views: instances, questions, tasks & halts, nodes. Keys are shown in the
//! footer: `1`–`4` or Tab switch views, ↑/↓ select, Enter shows details,
//! `a` answers a question, `m` messages an instance, `x` stops it, `r`
//! recovers a halt, `q` quits. Data is refreshed every 2 seconds.

use crate::Api;
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use moocher_protocol::api::Overview;
use moocher_protocol::model::*;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Tabs, Wrap};
use ratatui::Frame;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub enum View {
    #[default]
    Instances,
    Questions,
    Tasks,
    Nodes,
}

const VIEWS: [(View, &str); 4] = [(View::Instances, "1 Instances"), (View::Questions, "2 Questions"), (View::Tasks, "3 Tasks"), (View::Nodes, "4 Nodes")];

#[derive(Default)]
pub enum Mode {
    #[default]
    Normal,
    Detail,
    /// Typing: what the text is for, and the text.
    Input(InputFor, String),
    Confirm(String),
}

pub enum InputFor {
    Answer(String),
    Message(String),
}

#[derive(Default)]
pub struct App {
    pub view: View,
    pub ov: Option<Overview>,
    pub questions: Vec<Question>,
    pub halts: Vec<Halt>,
    pub sel: usize,
    pub mode: Mode,
    pub status: String,
}

fn state_style(s: SessionState) -> (Style, &'static str) {
    // Colour plus a symbol, so states are readable without colour too.
    match s {
        SessionState::Running => (Style::default().fg(Color::Green), "● running"),
        SessionState::Idle => (Style::default().fg(Color::Gray), "○ idle"),
        SessionState::Blocked => (Style::default().fg(Color::Yellow), "? waiting"),
        SessionState::Parked => (Style::default().fg(Color::Cyan), "◷ parked"),
        SessionState::Error => (Style::default().fg(Color::Red), "! error"),
        SessionState::Dead => (Style::default().fg(Color::Red).add_modifier(Modifier::BOLD), "✗ dead"),
        SessionState::Halted => (Style::default().fg(Color::LightRed), "‖ halted"),
        SessionState::Starting => (Style::default().fg(Color::Blue), "… starting"),
        SessionState::Stopped => (Style::default().fg(Color::DarkGray), "■ stopped"),
        SessionState::Unreachable => (Style::default().fg(Color::DarkGray), "⌁ unreachable"),
        SessionState::Lost => (Style::default().fg(Color::DarkGray), "⌀ lost"),
    }
}

impl App {
    fn sessions(&self) -> Vec<&SessionInfo> {
        self.ov.as_ref().map(|o| o.sessions.iter().filter(|s| s.report.state.is_live() || matches!(s.report.state, SessionState::Dead | SessionState::Halted | SessionState::Error)).collect()).unwrap_or_default()
    }
    fn rows(&self) -> usize {
        match self.view {
            View::Instances => self.sessions().len(),
            View::Questions => self.questions.len(),
            View::Tasks => self.halts.len() + self.ov.as_ref().map(|o| o.tasks.len()).unwrap_or(0),
            View::Nodes => self.ov.as_ref().map(|o| o.nodes.len()).unwrap_or(0),
        }
    }
}

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(3), Constraint::Min(3), Constraint::Length(2)]).split(area);
    let titles: Vec<Line> = VIEWS
        .iter()
        .map(|(v, t)| {
            let badge = match v {
                View::Questions if !app.questions.is_empty() => format!(" ({})", app.questions.len()),
                View::Tasks if !app.halts.is_empty() => format!(" ({} halted)", app.halts.len()),
                _ => String::new(),
            };
            Line::from(format!("{t}{badge}"))
        })
        .collect();
    let sel = VIEWS.iter().position(|(v, _)| *v == app.view).unwrap_or(0);
    f.render_widget(Tabs::new(titles).select(sel).block(Block::default().borders(Borders::ALL).title(" moocher ")).highlight_style(Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)), chunks[0]);
    let mut ts = TableState::default();
    ts.select(Some(app.sel.min(app.rows().saturating_sub(1))));
    let hl = Style::default().add_modifier(Modifier::REVERSED);
    match app.view {
        View::Instances => {
            let rows: Vec<Row> = app
                .sessions()
                .iter()
                .map(|s| {
                    let (st, label) = state_style(s.report.state);
                    let done = s.report.todos.iter().filter(|t| t.status == "completed").count();
                    Row::new(vec![
                        Cell::from(s.report.id.clone()),
                        Cell::from(s.report.agent.clone()),
                        Cell::from(s.report.node.clone()),
                        Cell::from(label).style(st),
                        Cell::from(format!("{done}/{}", s.report.todos.len())),
                        Cell::from(format!("${:.4}", s.report.billed().cost_usd)),
                        Cell::from(s.report.error.clone().or(s.report.current_task.clone()).unwrap_or_default()),
                    ])
                })
                .collect();
            let t = Table::new(rows, [Constraint::Length(14), Constraint::Length(12), Constraint::Length(10), Constraint::Length(14), Constraint::Length(6), Constraint::Length(9), Constraint::Min(10)])
                .header(Row::new(["SESSION", "AGENT", "NODE", "STATE", "TODOS", "COST", "NOW"]).style(Style::default().add_modifier(Modifier::BOLD)))
                .block(Block::default().borders(Borders::ALL).title(" instances "))
                .row_highlight_style(hl);
            f.render_stateful_widget(t, chunks[1], &mut ts);
        }
        View::Questions => {
            let rows: Vec<Row> = app
                .questions
                .iter()
                .map(|q| {
                    let imp = match q.importance {
                        Importance::Vital => Cell::from("!! vital").style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
                        Importance::Normal => Cell::from("?  normal").style(Style::default().fg(Color::Yellow)),
                        Importance::Low => Cell::from(".  low"),
                    };
                    Row::new(vec![imp, Cell::from(q.blocked.to_string()), Cell::from(format!("{} {}", q.asker_agent, q.asker)), Cell::from(q.holder.clone()), Cell::from(q.text.replace('\n', " "))])
                })
                .collect();
            let t = Table::new(rows, [Constraint::Length(9), Constraint::Length(4), Constraint::Length(24), Constraint::Length(14), Constraint::Min(10)])
                .header(Row::new(["", "WAIT", "FROM", "HELD BY", "QUESTION"]).style(Style::default().add_modifier(Modifier::BOLD)))
                .block(Block::default().borders(Borders::ALL).title(" questions (a: answer) "))
                .row_highlight_style(hl);
            f.render_stateful_widget(t, chunks[1], &mut ts);
        }
        View::Tasks => {
            let mut rows: Vec<Row> = app
                .halts
                .iter()
                .map(|h| {
                    let opts: Vec<String> = h.options.iter().enumerate().filter(|(_, o)| o.available).map(|(i, o)| format!("{}:{} ~${:.4}", i + 1, o.kind, o.est_cost_usd)).collect();
                    Row::new(vec![Cell::from("‖ HALTED").style(Style::default().fg(Color::LightRed)), Cell::from(h.scope.clone()), Cell::from(format!("{} died ({}) · {}", h.dead_session, h.reason, opts.join("  ")))])
                })
                .collect();
            if let Some(o) = &app.ov {
                rows.extend(o.tasks.iter().map(|t| Row::new(vec![Cell::from(t.status.as_str()), Cell::from(format!("#{} {}", t.id, t.label.as_str())), Cell::from(t.title.clone())])));
            }
            let t = Table::new(rows, [Constraint::Length(10), Constraint::Length(18), Constraint::Min(10)])
                .block(Block::default().borders(Borders::ALL).title(" halts and tasks (r: recover a halt) "))
                .row_highlight_style(hl);
            f.render_stateful_widget(t, chunks[1], &mut ts);
        }
        View::Nodes => {
            let rows: Vec<Row> = app
                .ov
                .as_ref()
                .map(|o| {
                    o.nodes
                        .iter()
                        .map(|n| {
                            let link = if n.connected { Cell::from("● connected").style(Style::default().fg(Color::Green)) } else { Cell::from("○ offline").style(Style::default().fg(Color::Red)) };
                            Row::new(vec![
                                Cell::from(n.name.clone()),
                                Cell::from(n.role.clone()),
                                link,
                                Cell::from(n.link_rtt_ms.map(|r| format!("{r:.0} ms")).unwrap_or_default()),
                                Cell::from(format!("{}/{}", n.running, n.max_sessions)),
                                Cell::from(n.agents.iter().map(|a| a.name.clone()).collect::<Vec<_>>().join(", ")),
                            ])
                        })
                        .collect()
                })
                .unwrap_or_default();
            let t = Table::new(rows, [Constraint::Length(12), Constraint::Length(7), Constraint::Length(12), Constraint::Length(7), Constraint::Length(6), Constraint::Min(10)])
                .header(Row::new(["NODE", "ROLE", "LINK", "RTT", "SLOTS", "AGENTS"]).style(Style::default().add_modifier(Modifier::BOLD)))
                .block(Block::default().borders(Borders::ALL).title(" nodes "))
                .row_highlight_style(hl);
            f.render_stateful_widget(t, chunks[1], &mut ts);
        }
    }
    let help = match &app.mode {
        Mode::Input(_, _) => "type, Enter to send, Esc to cancel".to_string(),
        Mode::Confirm(q) => format!("{q}  (y / n)"),
        _ => "1-4/Tab views · ↑↓ select · Enter details · a answer · m message · x stop · r recover · q quit".to_string(),
    };
    f.render_widget(Paragraph::new(vec![Line::from(Span::styled(app.status.clone(), Style::default().fg(Color::Yellow))), Line::from(help)]), chunks[2]);
    match &app.mode {
        Mode::Detail => {
            let r = popup(area, 80, 70);
            f.render_widget(Clear, r);
            f.render_widget(Paragraph::new(detail(app)).wrap(Wrap { trim: false }).block(Block::default().borders(Borders::ALL).title(" details (Esc) ")), r);
        }
        Mode::Input(what, text) => {
            let r = popup(area, 70, 20);
            f.render_widget(Clear, r);
            let title = match what {
                InputFor::Answer(id) => format!(" answer {id} "),
                InputFor::Message(id) => format!(" message {id} "),
            };
            f.render_widget(Paragraph::new(format!("{text}▏")).wrap(Wrap { trim: false }).block(Block::default().borders(Borders::ALL).title(title)), r);
        }
        _ => {}
    }
}

fn popup(area: Rect, w: u16, h: u16) -> Rect {
    let width = area.width * w / 100;
    let height = (area.height * h / 100).max(5);
    Rect { x: area.x + (area.width - width) / 2, y: area.y + (area.height - height) / 2, width, height }
}

fn detail(app: &App) -> String {
    match app.view {
        View::Instances => app.sessions().get(app.sel).map(|s| {
            let todos: Vec<String> = s.report.todos.iter().map(|t| format!("  [{}] {}", match t.status.as_str() { "completed" => "x", "in_progress" => ">", "cancelled" => "-", _ => " " }, t.content)).collect();
            format!(
                "{} · {} on {} · {}\nmodel {}  workspace {}\ntokens {}  cost ${:.4}\n{}\ntodos:\n{}\n\nlast output:\n{}",
                s.report.id,
                s.report.agent,
                s.report.node,
                s.report.state.as_str(),
                s.report.model.clone().unwrap_or_default(),
                s.report.workspace.clone().unwrap_or_default(),
                s.report.tokens.total(),
                s.report.tokens.cost_usd,
                s.report.error.clone().map(|e| format!("ERROR {e}")).unwrap_or_default(),
                todos.join("\n"),
                s.report.last_text.clone().unwrap_or_default()
            )
        }),
        View::Questions => app.questions.get(app.sel).map(|q| {
            let opts: Vec<String> = q.options.iter().enumerate().map(|(i, o)| format!("  {}) {} {}", i + 1, o.label, o.description)).collect();
            let notes: Vec<String> = q.chain.iter().filter_map(|h| h.note.as_ref().map(|n| format!("  {}: {n}", h.holder))).collect();
            format!("{} · {} · {:?}\nfrom {} {} on {}\n\n{}\n\n{}\n{}", q.id, q.importance.as_str(), q.kind, q.asker_agent, q.asker, q.node, q.text, opts.join("\n"), notes.join("\n"))
        }),
        View::Tasks => app.halts.get(app.sel).map(|h| {
            let o: Vec<String> = h.options.iter().enumerate().map(|(i, o)| format!("  {}) {:<17} ~${:.4} {} {}", i + 1, o.kind, o.est_cost_usd, if o.available { "" } else { "(unavailable)" }, o.note)).collect();
            format!("{} · {}\n{} died ({})\nwas doing: {}\nspent ${:.4}\n\n{}", h.id, h.scope, h.dead_session, h.reason, h.current_todo.clone().unwrap_or_default(), h.spent_usd, o.join("\n"))
        }),
        View::Nodes => app.ov.as_ref().and_then(|o| o.nodes.get(app.sel)).map(|n| format!("{} · {} · {}\nIP {}\nagents: {}", n.name, n.role, n.enrollment, n.observed_ip.clone().unwrap_or_default(), n.agents.iter().map(|a| format!("{} ({}/{})", a.name, a.provider, a.model)).collect::<Vec<_>>().join(", "))),
    }
    .unwrap_or_else(|| "nothing selected".into())
}

async fn refresh(api: &Api, app: &mut App) {
    match api.get("/overview").await.and_then(|v| Ok(serde_json::from_value::<Overview>(v)?)) {
        Ok(o) => app.ov = Some(o),
        Err(e) => app.status = format!("refresh failed: {e:#}"),
    }
    if let Ok(v) = api.get("/questions").await {
        app.questions = serde_json::from_value(v).unwrap_or_default();
    }
    if let Ok(v) = api.get("/halts").await {
        app.halts = serde_json::from_value(v).unwrap_or_default();
    }
}

async fn result(app: &mut App, what: &str, r: Result<Value>) {
    app.status = match r {
        Ok(_) => format!("{what}: done"),
        Err(e) => format!("{what}: {e:#}"),
    };
}

pub async fn run(api: &Api) -> Result<()> {
    let mut term = ratatui::init();
    let mut app = App::default();
    refresh(api, &mut app).await;
    let mut last = Instant::now();
    let res: Result<()> = async {
        loop {
            term.draw(|f| draw(f, &app))?;
            if last.elapsed() > Duration::from_secs(2) {
                refresh(api, &mut app).await;
                last = Instant::now();
            }
            if !event::poll(Duration::from_millis(250))? {
                continue;
            }
            let Event::Key(k) = event::read()? else { continue };
            if k.kind != KeyEventKind::Press {
                continue;
            }
            match std::mem::take(&mut app.mode) {
                Mode::Input(what, mut text) => match k.code {
                    KeyCode::Esc => {}
                    KeyCode::Enter => {
                        let r = match &what {
                            InputFor::Answer(id) => api.post(&format!("/questions/{id}/answer"), json!({"answer": text})).await,
                            InputFor::Message(id) => api.post(&format!("/sessions/{id}/message"), json!({"text": text, "delivery": "queue"})).await,
                        };
                        result(&mut app, "send", r).await;
                        refresh(api, &mut app).await;
                    }
                    KeyCode::Backspace => {
                        text.pop();
                        app.mode = Mode::Input(what, text);
                    }
                    KeyCode::Char(c) => {
                        text.push(c);
                        app.mode = Mode::Input(what, text);
                    }
                    _ => app.mode = Mode::Input(what, text),
                },
                Mode::Confirm(q) => {
                    if k.code == KeyCode::Char('y') {
                        if let Some(s) = app.sessions().get(app.sel).map(|s| s.report.id.clone()) {
                            let r = api.req(reqwest::Method::DELETE, &format!("/sessions/{s}"), None).await;
                            result(&mut app, "stop", r).await;
                        }
                    } else if k.code != KeyCode::Char('n') && k.code != KeyCode::Esc {
                        app.mode = Mode::Confirm(q);
                    }
                }
                Mode::Detail => {
                    if !matches!(k.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q')) {
                        app.mode = Mode::Detail;
                    }
                }
                Mode::Normal => match k.code {
                    KeyCode::Char('q') => return Ok(()),
                    KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
                    KeyCode::Char(c @ '1'..='4') => {
                        app.view = VIEWS[(c as u8 - b'1') as usize].0;
                        app.sel = 0;
                    }
                    KeyCode::Tab => {
                        let i = VIEWS.iter().position(|(v, _)| *v == app.view).unwrap_or(0);
                        app.view = VIEWS[(i + 1) % VIEWS.len()].0;
                        app.sel = 0;
                    }
                    KeyCode::Up => app.sel = app.sel.saturating_sub(1),
                    KeyCode::Down => app.sel = (app.sel + 1).min(app.rows().saturating_sub(1)),
                    KeyCode::Enter => app.mode = Mode::Detail,
                    KeyCode::Char('a') if app.view == View::Questions => {
                        if let Some(q) = app.questions.get(app.sel) {
                            app.mode = Mode::Input(InputFor::Answer(q.id.clone()), String::new());
                        }
                    }
                    KeyCode::Char('m') if app.view == View::Instances => {
                        if let Some(s) = app.sessions().get(app.sel) {
                            app.mode = Mode::Input(InputFor::Message(s.report.id.clone()), String::new());
                        }
                    }
                    KeyCode::Char('x') if app.view == View::Instances => {
                        if let Some(s) = app.sessions().get(app.sel) {
                            app.mode = Mode::Confirm(format!("stop {}?", s.report.id));
                        }
                    }
                    KeyCode::Char('r') if app.view == View::Tasks => {
                        // The cheapest available option, like the dashboard's first button.
                        if let Some(h) = app.halts.get(app.sel) {
                            let best = h.options.iter().filter(|o| o.available && o.kind != "abandon").min_by(|a, b| a.est_cost_usd.partial_cmp(&b.est_cost_usd).unwrap());
                            if let Some(o) = best {
                                let (id, kind) = (h.id.clone(), o.kind.clone());
                                let r = api.post(&format!("/halts/{id}/recover"), json!({"option": kind})).await;
                                result(&mut app, &format!("recover {kind}"), r).await;
                                refresh(api, &mut app).await;
                            }
                        }
                    }
                    _ => {}
                },
            }
        }
    }
    .await;
    ratatui::restore();
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn session(id: &str, state: SessionState) -> SessionInfo {
        serde_json::from_value(json!({
            "id": id, "node": "hub", "agent": "builder", "state": state, "started_at": 0, "updated_at": 0,
            "todos": [{"content": "write tests", "status": "completed"}, {"content": "run tests", "status": "in_progress"}],
            "current_task": "run tests"
        }))
        .unwrap()
    }

    fn screen(app: &App) -> String {
        let mut t = Terminal::new(TestBackend::new(120, 20)).unwrap();
        t.draw(|f| draw(f, app)).unwrap();
        let buf = t.backend().buffer().clone();
        (0..buf.area.height).map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>()).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn renders_views() {
        let mut app = App { ov: Some(Overview { nodes: vec![], sessions: vec![session("s-1", SessionState::Running), session("s-2", SessionState::Dead)], tasks: vec![], totals: TokenUsage::default() }), ..Default::default() };
        let s = screen(&app);
        assert!(s.contains("s-1") && s.contains("● running") && s.contains("1/2"), "{s}");
        assert!(s.contains("✗ dead"), "dead shown with a symbol, not only colour");
        app.mode = Mode::Detail;
        assert!(screen(&app).contains("[>] run tests"));
        app.mode = Mode::Normal;
        app.view = View::Questions;
        app.questions = vec![serde_json::from_value(json!({
            "id": "q-1", "kind": "ask", "asker": "s-1", "asker_agent": "builder", "node": "hub", "text": "Which port?",
            "importance": "vital", "asked_importance": "low", "mode": "important_and_vital", "holder": "operator", "holder_since": 0,
            "chain": [], "status": "open", "created_at": 0, "updated_at": 0, "blocked": 2
        }))
        .unwrap()];
        let s = screen(&app);
        assert!(s.contains("2 Questions (1)") && s.contains("!! vital") && s.contains("Which port?"), "{s}");
    }
}
