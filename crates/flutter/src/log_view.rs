use util::ResultExt;
use gpui::{
    div, list, AnyElement, App, Context, EventEmitter, InteractiveElement, IntoElement,
    ListAlignment, ListState, ParentElement, Render, Styled, Task,
    WeakEntity, Window, px,
};
use ui::prelude::*;
use ui::{Checkbox, IconButton, IconName, IconSize, Tooltip};
use workspace::dock::{Panel, PanelEvent, DockPosition};
use zed_actions::flutter::OpenFlutterLogs;
use workspace::Workspace;
use crate::vm_service::DartVmService;

pub struct FlutterLogPanel {
    workspace: WeakEntity<Workspace>,
    logs: Vec<LogEntry>,
    list_state: ListState,
    connection_task: Option<Task<()>>,
    focus_handle: gpui::FocusHandle,
    vm_service_uri: Option<String>,
    width: Option<gpui::Pixels>,
    height: Option<gpui::Pixels>,
    dock_position: DockPosition,
    manual_search_path: Option<std::path::PathBuf>,
    auto_scroll: bool,
}

#[derive(Clone)]
struct LogEntry {
    timestamp: chrono::DateTime<chrono::Local>,
    level: String,
    message: String,
}

impl FlutterLogPanel {
    pub fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let weak_workspace = workspace.weak_handle();
        let list_state = ListState::new(0, ListAlignment::Top, px(1000.));

        let this = Self {
            workspace: weak_workspace,
            logs: Vec::new(),
            list_state,
            connection_task: None,
            focus_handle: cx.focus_handle(),
            vm_service_uri: None,
            width: None,
            height: Some(gpui::px(300.)),
            dock_position: DockPosition::Bottom,
            manual_search_path: None,
            auto_scroll: true,
        };
        this
    }

    pub fn connect(&mut self, cx: &mut Context<Self>) {
        self.try_connect(cx);
    }

    pub fn set_search_path(&mut self, path: std::path::PathBuf, cx: &mut Context<Self>) {
        self.manual_search_path = Some(path);
        if self.vm_service_uri.is_none() {
            self.try_connect(cx);
        }
    }

    fn try_connect(&mut self, cx: &mut Context<Self>) {
        let weak_view = cx.weak_entity();
        // Cancel existing task to restart with potentially new search path or just ensures single task
        self.connection_task = None;
        
        self.connection_task = Some(cx.spawn(|this: gpui::WeakEntity<FlutterLogPanel>, cx: &mut gpui::AsyncApp| {
            let mut cx = cx.clone();
            async move {
                let mut attempts = 0;
                loop {
                    // Read worktree paths inside the spawn to avoid borrow conflicts
                    let worktree_paths: Option<Vec<std::path::PathBuf>> = this.update(&mut cx, |view, cx| {
                        let mut paths = Vec::new();
                        if let Some(manual) = &view.manual_search_path {
                            paths.push(manual.clone());
                        }
                        
                        // Also include workspace roots as fallback
                        if let Some(workspace) = view.workspace.upgrade() {
                             paths.extend(workspace.read(cx).worktrees(cx)
                                .map(|wt| wt.read(cx).abs_path().to_path_buf()));
                        }
                        paths
                    }).ok();

                    if let Some(worktree_paths) = worktree_paths {
                        for worktree_path in worktree_paths {
                            let flutter_url_path = worktree_path.join(".dart_tool/flutter_url");
                            match std::fs::read_to_string(&flutter_url_path) {
                                Ok(contents) => {
                                    let uri = contents.trim().to_string();
                                    if !uri.is_empty() {
                                        // Convert HTTP/HTTPS to WS/WSS
                                        let mut ws_uri = uri.replace("http://", "ws://").replace("https://", "wss://");
                                        if !ws_uri.ends_with("/ws") && !ws_uri.ends_with("/ws/") {
                                            if ws_uri.ends_with('/') {
                                                ws_uri.push_str("ws");
                                            } else {
                                                ws_uri.push_str("/ws");
                                            }
                                        }
                                        
                                        // Attempt connection
                                        let ws_uri_clone = ws_uri.clone();
                                        let result = gpui_tokio::Tokio::spawn(&cx, async move {
                                            DartVmService::connect(&ws_uri_clone).await
                                        });

                                        match result {
                                            Ok(task) => match task.await {
                                                Ok(Ok(service)) => {
                                                     if let Some(view) = weak_view.upgrade() {
                                                         view.update(&mut cx, |view, cx| {
                                                             view.handle_connection(service, ws_uri.clone(), cx);
                                                         }).log_err();
                                                     }
                                                     return;
                                                }
                                                Ok(Err(e)) => {
                                                    if attempts % 5 == 0 { 
                                                         if let Some(view) = weak_view.upgrade() {
                                                             view.update(&mut cx, |view, cx| {
                                                                 view.add_log(format!("Connection failed to {}: {}", ws_uri, e), "ERROR", cx);
                                                             }).log_err();
                                                         }
                                                    }
                                                }
                                                Err(e) => {
                                                     if let Some(view) = weak_view.upgrade() {
                                                         view.update(&mut cx, |view, cx| {
                                                             view.add_log(format!("Connection task failed: {}", e), "ERROR", cx);
                                                         }).log_err();
                                                     }
                                                }
                                            },
                                            Err(e) => {
                                                if attempts == 0 {
                                                     if let Some(view) = weak_view.upgrade() {
                                                         view.update(&mut cx, |view, cx| {
                                                             view.add_log(format!("Failed to spawn connection task: {}", e), "ERROR", cx);
                                                         }).log_err();
                                                     }
                                                }
                                            }
                                        }
                                    }
                                }
                                Err(_) => {
                                    // File not found or unreadable, ignore
                                }
                            }
                        }
                    } else {
                         this.update(&mut cx, |view: &mut FlutterLogPanel, cx: &mut Context<FlutterLogPanel>| {
                            view.add_log("No workspace available".to_string(), "WARNING", cx);
                        }).ok();
                    }

                    attempts += 1;
                    if attempts > 60 { // 30 seconds
                         this.update(&mut cx, |view: &mut FlutterLogPanel, cx: &mut Context<FlutterLogPanel>| {
                            view.add_log("Could not link to running Flutter app (connection timed out).".to_string(), "ERROR", cx);
                             view.add_log("Please ensure 'flutter run' is active and '.dart_tool/flutter_url' exists.".to_string(), "ERROR", cx);
                        }).ok();
                        break;
                    }

                    if attempts == 1 {
                        this.update(&mut cx, |view: &mut FlutterLogPanel, cx: &mut Context<FlutterLogPanel>| {
                            view.add_log("Waiting for Flutter app to start...".to_string(), "INFO", cx);
                        }).ok();
                    }

                    cx.background_executor().timer(std::time::Duration::from_millis(500)).await;
                }
            }
        }));
    }

    fn handle_connection(&mut self, mut service: DartVmService, uri: String, cx: &mut Context<Self>) {
        self.vm_service_uri = Some(uri.clone());
        let weak_view = cx.weak_entity();
        
        let (tx, mut rx) = futures::channel::mpsc::unbounded();

        let tokio_task = cx.spawn(|_, cx: &mut gpui::AsyncApp| {
             let cx_root = cx.clone();
             async move {
                 let result = gpui_tokio::Tokio::spawn(&cx_root, async move {
                        let _ = tx.unbounded_send(LogEvent::Connected);

                        if let Err(e) = service.subscribe_logging().await {
                             let _ = tx.unbounded_send(LogEvent::Error(format!("Failed to subscribe to logs: {}", e)));
                             return;
                        }

                        let _ = tx.unbounded_send(LogEvent::Subscribed);

                        while let Some(log_record) = service.next_log().await {
                             let _ = tx.unbounded_send(LogEvent::Log(log_record));
                        }

                        let _ = tx.unbounded_send(LogEvent::Disconnected);
                });
                
                if let Ok(task) = result {
                    task.await.log_err();
                }
             }
        });

        self.connection_task = Some(cx.spawn(|_, cx: &mut gpui::AsyncApp| {
            let mut cx_root = cx.clone();
            async move {
                let consumer = async move {
                    use futures::StreamExt;
                    while let Some(event) = rx.next().await {
                        if let Some(view) = weak_view.upgrade() {
                            let _ = view.update::<(), gpui::AsyncApp>(&mut cx_root, |view: &mut FlutterLogPanel, cx: &mut Context<FlutterLogPanel>| {
                                match event {
                                    LogEvent::Connected => view.add_log("Connected to Dart VM Service".to_string(), "INFO", cx),
                                    LogEvent::Subscribed => view.add_log("Subscribed to Logging stream".to_string(), "INFO", cx),
                                    LogEvent::Disconnected => view.add_log("Disconnected from VM Service".to_string(), "INFO", cx),
                                    LogEvent::Error(msg) => view.add_log(msg, "ERROR", cx),
                                    LogEvent::Log(record) => {
                                        let message = record.message
                                            .and_then(|m| m.value_as_string)
                                            .unwrap_or_else(|| "<no message>".to_string());
                                        let level = record.level.unwrap_or(0);
                                        let level_str = match level {
                                            0..=500 => "FINE",
                                            501..=800 => "INFO",
                                            801..=900 => "WARNING",
                                            901..=1000 => "SEVERE",
                                            _ => "SHOUT",
                                        };
                                        view.add_log(message, level_str, cx);
                                    }
                                }
                            });
                        } else {
                            break;
                        }
                    }
                };
                
                futures::join!(tokio_task, consumer);
            }
        }));
    }

    fn add_log(&mut self, message: String, level: &str, cx: &mut Context<Self>) {
        let entry = LogEntry {
            timestamp: chrono::Local::now(),
            level: level.to_string(),
            message,
        };
        let old_len = self.logs.len();
        self.logs.push(entry);
        self.list_state.splice(old_len..old_len, 1);
        if self.auto_scroll {
            self.list_state.scroll_to_reveal_item(self.logs.len().saturating_sub(1));
        }
        cx.notify();
    }

    fn clear_logs(&mut self, cx: &mut Context<Self>) {
        self.logs.clear();
        self.list_state.reset(0);
        cx.notify();
    }
    
    fn toggle_auto_scroll(&mut self, checked: bool, cx: &mut Context<Self>) {
        self.auto_scroll = checked;
        if checked {
            self.list_state.scroll_to_reveal_item(self.logs.len().saturating_sub(1));
        }
        cx.notify();
    }

    fn render_log_entry(&self, ix: usize, _cx: &App) -> AnyElement {
        if let Some(entry) = self.logs.get(ix) {
             let color = match entry.level.as_str() {
                "SEVERE" | "SHOUT" => gpui::red(),
                "WARNING" => gpui::yellow(),
                "INFO" => gpui::blue(),
                _ => gpui::rgb(0xcccccc).into(), // Default gray
             };
             
             div()
                .flex()
                .flex_row()
                .items_start()
                .px_2()
                .py_0p5()
                .text_sm()
                .child(
                    div()
                        .w_24()
                        .flex_none()
                        .text_color(gpui::rgb(0x666666))
                        .child(entry.timestamp.format("%H:%M:%S%.3f").to_string())
                )
                 .child(
                    div()
                        .w_16()
                        .flex_none()
                        .text_color(color)
                        .child(entry.level.clone())
                )
                .child(
                    div()
                        .flex_1()
                        .text_color(if entry.level == "ERROR" { gpui::red() } else { gpui::rgb(0xcccccc).into() })
                        .child(entry.message.clone())
                        .cursor_text()
                )
                .into_any()
        } else {
            div().into_any()
        }
    }

    #[allow(dead_code)]
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.clear_logs(cx);
        self.connection_task = None;
        self.connect(cx);
        cx.notify();
    }
}

enum LogEvent {
    Connected,
    Subscribed,
    Disconnected,
    Error(String),
    Log(crate::vm_service::LogRecord),
}


impl gpui::Focusable for FlutterLogPanel {
    fn focus_handle(&self, _cx: &App) -> gpui::FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for FlutterLogPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme_bg = cx.theme().colors().surface_background;
        let header_bg = cx.theme().colors().title_bar_background;
        let weak_view = cx.weak_entity();

        div()
            .flex()
            .flex_col()
            .bg(theme_bg)
            .size_full()
            .track_focus(&self.focus_handle)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_2()
                    .py_1()
                    .bg(header_bg)
                    .child(
                         div().flex().items_center().gap_2()
                            .child(div().text_sm().font_weight(gpui::FontWeight::MEDIUM).child("Flutter Logs"))
                            .child(
                                IconButton::new("clear_logs", IconName::Trash)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Clear Logs"))
                                    .on_click(cx.listener(|this, _, _window, cx| this.clear_logs(cx)))
                            )
                     )
                    .child(
                        div().flex().items_center().gap_4()
                            .child(
                                Checkbox::new(
                                    "auto_scroll",
                                    if self.auto_scroll { ToggleState::Selected } else { ToggleState::Unselected }
                                )
                                .label("Auto-scroll")
                                .on_click(cx.listener(|this, selection, _window, cx| {
                                    this.toggle_auto_scroll(*selection == ToggleState::Selected, cx);
                                }))
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(gpui::rgb(0x888888))
                                    .child(if self.vm_service_uri.is_some() {
                                        "Connected"
                                    } else {
                                        "Disconnected"
                                    })
                            )
                    )
            )
            .child(
                div()
                    .id("flutter_logs")
                    .flex()
                    .flex_col()
                    .flex_grow()
                    // virtual list handles scrolling
                    .child(
                        list(self.list_state.clone(), move |ix, _, cx| {
                            if let Some(view) = weak_view.upgrade() {
                                view.read(cx).render_log_entry(ix, cx)
                            } else {
                                div().into_any()
                            }
                        })
                        .size_full()
                    )
            )
    }
}

impl EventEmitter<PanelEvent> for FlutterLogPanel {}

impl Panel for FlutterLogPanel {
    fn persistent_name() -> &'static str {
        "FlutterLogPanel"
    }

    fn panel_key() -> &'static str {
        "FlutterLogPanel"
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        self.dock_position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right | DockPosition::Bottom)
    }

    fn set_position(&mut self, position: DockPosition, _window: &mut Window, cx: &mut Context<Self>) {
        self.dock_position = position;
        cx.notify();
    }

    fn size(&self, _window: &Window, _cx: &App) -> gpui::Pixels {
        (self.dock_position.axis() == gpui::Axis::Vertical)
            .then(|| self.height)
            .flatten()
            .or(self.width)
            .unwrap_or(gpui::px(300.))
    }

    fn set_size(&mut self, size: Option<gpui::Pixels>, _window: &mut Window, cx: &mut Context<Self>) {
        if self.dock_position.axis() == gpui::Axis::Vertical {
            self.height = size;
        } else {
            self.width = size;
        }
        cx.notify();
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::Library) // Placeholder, ideally specific icon
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Flutter Logs")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(OpenFlutterLogs)
    }
    
    fn activation_priority(&self) -> u32 {
        10
    }
}
