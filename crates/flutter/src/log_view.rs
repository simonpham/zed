use util::ResultExt;
use gpui::{
    div, App, Context, EventEmitter, IntoElement,
    ParentElement, Render, Styled, Task,
    WeakEntity, Window,
};
use ui::prelude::*;
use ui::{Checkbox, IconButton, IconName, IconSize, Tooltip, ToggleState};
use workspace::dock::{Panel, PanelEvent, DockPosition};
use zed_actions::flutter::{OpenFlutterLogs, HotReload, HotRestart};
use workspace::Workspace;
use crate::vm_service::DartVmService;
use editor::{Editor, EditorEvent, Inlay, InlayContent};
use project::InlayId;
use editor::scroll::Autoscroll;
use multi_buffer::{MultiBuffer, MultiBufferOffset};
use editor::ToPoint;
use gpui::{Action, Entity, HighlightStyle};
use language::language_settings::SoftWrap;
use text::{Rope, Bias};
use smol::io::{AsyncBufReadExt, BufReader};
use smol::process::{Child, Command, Stdio};

pub struct FlutterLogPanel {
    workspace: WeakEntity<Workspace>,
    editor: Entity<Editor>,
    metadata: Vec<Option<LogMetadata>>,
    next_inlay_id: usize,
    active_timestamp_inlay: Option<InlayId>,
    connection_task: Option<Task<()>>,
    focus_handle: gpui::FocusHandle,
    vm_service_uri: Option<String>,
    width: Option<gpui::Pixels>,
    height: Option<gpui::Pixels>,
    dock_position: DockPosition,
    manual_search_path: Option<std::path::PathBuf>,
    auto_scroll: bool,
    fine_ranges: Vec<std::ops::Range<editor::Anchor>>,
    info_ranges: Vec<std::ops::Range<editor::Anchor>>,
    warning_ranges: Vec<std::ops::Range<editor::Anchor>>,
    severe_ranges: Vec<std::ops::Range<editor::Anchor>>,
    run_process: Option<std::sync::Arc<std::sync::Mutex<Option<Child>>>>,
    run_task: Option<Task<()>>,
    vm_connected: bool,
    _subscriptions: Vec<gpui::Subscription>,
}

#[derive(Clone)]
struct LogMetadata {
    timestamp: String,
    logger_name: Option<String>,
}

struct FineLog;
struct InfoLog;
struct WarningLog;
struct SevereLog;

impl FlutterLogPanel {
    pub fn new(workspace: &Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let weak_workspace = workspace.weak_handle();
        
        let editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_read_only(true);
            editor.set_show_gutter(false, cx);
            editor.set_soft_wrap_mode(SoftWrap::None, cx);
            editor.set_placeholder_text("Flutter logs will appear here...", window, cx);
            editor
        });


        let mut _subscriptions = Vec::new();
        _subscriptions.push(cx.subscribe(&editor, |this, editor, event, cx| {
            match event {
                EditorEvent::SelectionsChanged { .. } => {
                    this.update_timestamp_inlay(&editor, cx);
                }
                _ => {}
            }
        }));

        Self {
            workspace: weak_workspace,
            editor,
            metadata: Vec::new(),
            next_inlay_id: 0,
            active_timestamp_inlay: None,
            connection_task: None,
            focus_handle: cx.focus_handle(),
            vm_service_uri: None,
            width: None,
            height: Some(gpui::px(300.)),
            dock_position: DockPosition::Bottom,
            manual_search_path: None,
            auto_scroll: true,
            fine_ranges: Vec::new(),
            info_ranges: Vec::new(),
            warning_ranges: Vec::new(),
            severe_ranges: Vec::new(),
            run_process: None,
            run_task: None,
            vm_connected: false,
            _subscriptions,
        }
    }

    fn update_timestamp_inlay(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let result = editor.update(cx, |editor, cx| {
            let snapshot = editor.display_map.update(cx, |map, cx| map.snapshot(cx));
            let selections = editor.selections.all::<MultiBufferOffset>(&snapshot);
            let selection = selections.first()?;
            let buffer = editor.buffer().read(cx).snapshot(cx);
            let point = selection.head().to_point(&buffer);
            let row = point.row;
            
            let meta = self.metadata.get(row as usize)?.as_ref()?;
            Some((row, meta.timestamp.clone()))
        });

        let (row, timestamp) = match result {
            Some((r, t)) => (r, t),
            None => {
                if let Some(id) = self.active_timestamp_inlay.take() {
                    editor.update(cx, |editor, cx| {
                        editor.splice_inlays(&[id], Vec::new(), cx);
                    });
                }
                return;
            }
        };

        editor.update(cx, |editor, cx| {
            let mut to_remove = Vec::new();
            if let Some(id) = self.active_timestamp_inlay {
                to_remove.push(id);
            }

            let id = InlayId::LogTimestamp(self.next_inlay_id);
            self.next_inlay_id += 1;
            self.active_timestamp_inlay = Some(id);

            let buffer = editor.buffer().read(cx).snapshot(cx);
            let line_len = buffer.line_len(multi_buffer::MultiBufferRow(row));
            let position = buffer.anchor_at(multi_buffer::MultiBufferPoint::new(row, line_len), Bias::Right);

            editor.splice_inlays(&to_remove, vec![Inlay {
                id,
                position,
                content: InlayContent::Text(Rope::from(timestamp.clone())),
            }], cx);
        });
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
                                                             view.add_log(format!("VM service connected at {}", ws_uri), "INFO", None, cx);
                                                             view.handle_connection(service, ws_uri.clone(), cx);
                                                         }).log_err();
                                                     }
                                                     return;
                                                }
                                                Ok(Err(_e)) => {
                                                    // Silently retry - VM service not ready yet
                                                }
                                                Err(e) => {
                                                     if let Some(view) = weak_view.upgrade() {
                                                         view.update(&mut cx, |view, cx| {
                                                             view.add_log(format!("Connection task failed: {}", e), "ERROR", None, cx);
                                                         }).log_err();
                                                     }
                                                }
                                            },
                                            Err(e) => {
                                                if attempts == 0 {
                                                     if let Some(view) = weak_view.upgrade() {
                                                         view.update(&mut cx, |view, cx| {
                                                             view.add_log(format!("Failed to spawn connection task: {}", e), "ERROR", None, cx);
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
                            view.add_log("No workspace available".to_string(), "WARNING", None, cx);
                        }).ok();
                    }

                    attempts += 1;
                    if attempts > 60 { // 30 seconds
                         this.update(&mut cx, |view: &mut FlutterLogPanel, cx: &mut Context<FlutterLogPanel>| {
                            view.add_log("Could not link to running Flutter app (connection timed out).".to_string(), "ERROR", None, cx);
                             view.add_log("Please ensure 'flutter run' is active and '.dart_tool/flutter_url' exists.".to_string(), "ERROR", None, cx);
                        }).ok();
                        break;
                    }

                    if attempts == 1 {
                        this.update(&mut cx, |view: &mut FlutterLogPanel, cx: &mut Context<FlutterLogPanel>| {
                            view.add_log("Waiting for Flutter app to start...".to_string(), "INFO", None, cx);
                        }).ok();
                    }

                    cx.background_executor().timer(std::time::Duration::from_millis(500)).await;
                }
            }
        }));
    }

    fn handle_connection(&mut self, mut service: DartVmService, uri: String, cx: &mut Context<Self>) {
        self.vm_service_uri = Some(uri);
        self.vm_connected = true;
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
                                    LogEvent::Connected => view.add_log("Connected to Dart VM Service".to_string(), "INFO", None, cx),
                                    LogEvent::Subscribed => view.add_log("Subscribed to Logging stream".to_string(), "INFO", None, cx),
                                    LogEvent::Disconnected => view.add_log("Disconnected from VM Service".to_string(), "INFO", None, cx),
                                    LogEvent::Error(msg) => view.add_log(msg, "ERROR", None, cx),
                                    LogEvent::Log(record) => {
                                        let message = record.message
                                            .and_then(|m| m.value_as_string)
                                            .unwrap_or_else(|| "<no message>".to_string());
                                        let level = record.level.unwrap_or(0);
                                        let logger_name = record.logger_name.and_then(|m| m.value_as_string);
                                        let level_str = match level {
                                            0..=500 => "FINE",
                                            501..=800 => "INFO",
                                            801..=900 => "WARNING",
                                            901..=1000 => "SEVERE",
                                            _ => "SHOUT",
                                        };
                                        view.add_log(message, level_str, logger_name, cx);
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

    pub fn add_log(&mut self, message: String, level: &str, logger_name: Option<String>, cx: &mut Context<Self>) {
        let timestamp_str = chrono::Local::now().format("%H:%M:%S").to_string();
        let lines: Vec<&str> = message.trim_end_matches('\n').split('\n').collect();

        for (i, line) in lines.iter().enumerate() {
            let meta = if i == 0 {
                Some(LogMetadata {
                    timestamp: timestamp_str.clone(),
                    logger_name: logger_name.clone(),
                })
            } else {
                None
            };
            self.metadata.push(meta.clone());

            let log_line = format!("{}\n", line);
            let editor = self.editor.clone();
            editor.update(cx, |editor: &mut Editor, cx| {
                let (start, end) = editor.buffer().update(cx, |buffer: &mut MultiBuffer, cx| {
                    let start = buffer.len(cx);
                    buffer.edit([(start..start, log_line.clone())], None, cx);
                    let end = buffer.len(cx);
                    (start, end)
                });

                let buffer = editor.buffer().read(cx);
                let snapshot = buffer.snapshot(cx);
                let range = snapshot.anchor_before(start)..snapshot.anchor_after(end);

                // Accumulate range for highlighting based on level
                match level {
                    "FINE" => {
                        self.fine_ranges.push(range);
                    }
                    "INFO" => {
                        self.info_ranges.push(range);
                    }
                    "WARNING" => {
                        self.warning_ranges.push(range);
                    }
                    "SEVERE" | "SHOUT" | "ERROR" => {
                        self.severe_ranges.push(range);
                    }
                    _ => {}
                }

                // Apply all accumulated highlights
                editor.highlight_text::<FineLog>(
                    self.fine_ranges.clone(),
                    HighlightStyle {
                        color: Some(gpui::hsla(0.5, 0.0, 0.8, 1.0)),
                        ..Default::default()
                    },
                    cx,
                );
                editor.highlight_text::<InfoLog>(
                    self.info_ranges.clone(),
                    HighlightStyle {
                        color: Some(gpui::hsla(0.58, 1.0, 0.6, 1.0)),
                        ..Default::default()
                    },
                    cx,
                );
                editor.highlight_text::<WarningLog>(
                    self.warning_ranges.clone(),
                    HighlightStyle {
                        color: Some(gpui::hsla(0.13, 1.0, 0.55, 1.0)),
                        ..Default::default()
                    },
                    cx,
                );
                editor.highlight_text::<SevereLog>(
                    self.severe_ranges.clone(),
                    HighlightStyle {
                        color: Some(gpui::hsla(0.0, 0.9, 0.55, 1.0)),
                        ..Default::default()
                    },
                    cx,
                );

                // Add Name Inlay
                if let Some(meta) = meta {
                    if let Some(name) = &meta.logger_name {
                        let id = InlayId::LogName(self.next_inlay_id);
                        self.next_inlay_id += 1;
                        let position = snapshot.anchor_at(start, Bias::Left);
                        editor.splice_inlays(&[], vec![Inlay {
                            id,
                            position,
                            content: InlayContent::Text(Rope::from(format!("{} ", name))),
                        }], cx);
                    }
                }
            });
        }

        if self.auto_scroll {
            self.editor.update(cx, |editor, cx| {
                let end = editor.buffer().read(cx).read(cx).len();
                let snapshot = editor.display_snapshot(cx);
                editor.selections.change_with(&snapshot, |s| {
                    s.select_ranges(vec![end..end]);
                });
                editor.request_autoscroll(Autoscroll::newest(), cx);
            });
        }
        cx.notify();
    }

    
    fn toggle_auto_scroll(&mut self, checked: bool, cx: &mut Context<Self>) {
        self.auto_scroll = checked;
        if checked {
            self.editor.update(cx, |editor: &mut Editor, cx| {
                editor.request_autoscroll(Autoscroll::newest(), cx);
            });
        }
        cx.notify();
    }


    #[allow(dead_code)]
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.clear_logs(cx);
        self.connection_task = None;
        self.connect(cx);
        cx.notify();
    }

    pub fn clear_logs(&mut self, cx: &mut Context<Self>) {
        self.metadata.clear();
        self.next_inlay_id = 0;
        self.active_timestamp_inlay = None;
        self.editor.update(cx, |editor, cx| {
            editor.buffer().update(cx, |buffer, cx| {
                let len = buffer.read(cx).len();
                buffer.edit([(MultiBufferOffset(0)..len, "")], None, cx);
            });
            editor.splice_inlays(&[], Vec::new(), cx);
        });
        self.fine_ranges.clear();
        self.info_ranges.clear();
        self.warning_ranges.clear();
        self.severe_ranges.clear();
        cx.notify();
    }

    pub fn is_running(&self) -> bool {
        self.run_process.is_some()
    }

    pub fn start_run(
        &mut self,
        device_id: String,
        target: String,
        cwd: Option<String>,
        workspace_roots: Vec<std::path::PathBuf>,
        cx: &mut Context<Self>,
    ) {
        self.stop_run(cx);

        let process_holder = std::sync::Arc::new(std::sync::Mutex::new(None::<Child>));
        self.run_process = Some(process_holder.clone());

        let cwd_path = cwd.map(std::path::PathBuf::from);
        let search_path = cwd_path.clone();

        if let Some(path) = search_path {
            self.set_search_path(path, cx);
        }

        // Detect FVM usage early to show in log
        // Check for .fvm/fvm_config.json (older FVM) or .fvm/version (newer FVM)
        // Check both target folder and workspace root
        let check_fvm_in_path = |p: &std::path::Path| -> bool {
            p.join(".fvm/fvm_config.json").exists() || p.join(".fvm/version").exists()
        };
        
        let use_fvm = cwd_path.as_ref().map(|p| check_fvm_in_path(p)).unwrap_or(false)
            || workspace_roots.iter().any(|p| check_fvm_in_path(p));
        
        let cmd_prefix = if use_fvm { "fvm flutter" } else { "flutter" };
        self.add_log(
            format!("Starting {} run -d {} -t {}", cmd_prefix, device_id, target),
            "INFO",
            Some("flutter".to_string()),
            cx,
        );

        let weak = cx.weak_entity();
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<ProcessEvent>();
        
        self.run_task = Some(cx.spawn(move |_this: gpui::WeakEntity<FlutterLogPanel>, cx: &mut gpui::AsyncApp| {
            let mut cx = cx.clone();
            let tx_spawn = tx.clone();
            async move {
                // Detect FVM usage by checking for .fvm/fvm_config.json (older) or .fvm/version (newer)
                // Check both target folder and workspace roots
                let check_fvm = |p: &std::path::Path| -> bool {
                    p.join(".fvm/fvm_config.json").exists() || p.join(".fvm/version").exists()
                };
                let use_fvm = cwd_path.as_ref().map(|p| check_fvm(p)).unwrap_or(false)
                    || workspace_roots.iter().any(|p| check_fvm(p));

                let mut cmd = if use_fvm {
                    let mut c = Command::new("fvm");
                    c.args([
                        "flutter",
                        "run",
                        "-d",
                        &device_id,
                        "-t",
                        &target,
                        "--vmservice-out-file=.dart_tool/flutter_url",
                    ]);
                    c
                } else {
                    let mut c = Command::new("flutter");
                    c.args([
                        "run",
                        "-d",
                        &device_id,
                        "-t",
                        &target,
                        "--vmservice-out-file=.dart_tool/flutter_url",
                    ]);
                    c
                };

                if let Some(ref cwd) = cwd_path {
                    cmd.current_dir(cwd);
                }

                cmd.stdout(Stdio::piped());
                cmd.stderr(Stdio::piped());
                cmd.stdin(Stdio::piped());

                let mut child = match cmd.spawn() {
                    Ok(child) => child,
                    Err(e) => {
                        let _ = tx_spawn.unbounded_send(ProcessEvent::Error(format!("Failed to start flutter run: {}", e)));
                        return;
                    }
                };

                let stdout = child.stdout.take();
                let stderr = child.stderr.take();

                {
                    let mut guard = process_holder.lock().unwrap();
                    *guard = Some(child);
                }

                let tx_stdout = tx_spawn.clone();
                let stdout_task = cx.background_executor().spawn(async move {
                    if let Some(stdout) = stdout {
                        let mut reader = BufReader::new(stdout);
                        let mut line = String::new();
                        while let Ok(n) = reader.read_line(&mut line).await {
                            if n == 0 {
                                break;
                            }
                            let content = std::mem::take(&mut line);
                            let _ = tx_stdout.unbounded_send(ProcessEvent::Stdout(content.trim_end().to_string()));
                        }
                    }
                });

                let tx_stderr = tx_spawn.clone();
                let stderr_task = cx.background_executor().spawn(async move {
                    if let Some(stderr) = stderr {
                        let mut reader = BufReader::new(stderr);
                        let mut line = String::new();
                        while let Ok(n) = reader.read_line(&mut line).await {
                            if n == 0 {
                                break;
                            }
                            let content = std::mem::take(&mut line);
                            let _ = tx_stderr.unbounded_send(ProcessEvent::Stderr(content.trim_end().to_string()));
                        }
                    }
                });

                let tx_exit = tx_spawn.clone();
                let exit_task = async move {
                    futures::future::join(stdout_task, stderr_task).await;
                    let _ = tx_exit.unbounded_send(ProcessEvent::Exited);
                };

                let consumer_weak = weak.clone();
                let consumer = async {
                    use futures::StreamExt;
                    while let Some(event) = rx.next().await {
                        if let Some(view) = consumer_weak.upgrade() {
                            let should_break = matches!(event, ProcessEvent::Exited);
                            view.update(&mut cx, |view: &mut FlutterLogPanel, cx: &mut Context<FlutterLogPanel>| {
                                match event {
                                    ProcessEvent::Stdout(msg) => {
                                        // After VM is connected, filter to only show important messages
                                        if view.vm_connected {
                                            let should_show = msg.contains("Performing hot reload")
                                                || msg.contains("Performing hot restart")
                                                || msg.contains("Reloaded")
                                                || msg.contains("Restarted application")
                                                || msg.contains("Try again after fixing")
                                                || msg.contains("Error:")
                                                || msg.contains("Exception:");
                                            if should_show {
                                                view.add_log(msg, "INFO", Some("Flutter".to_string()), cx);
                                            }
                                        } else {
                                            view.add_log(msg, "FINE", Some("Flutter".to_string()), cx);
                                        }
                                    }
                                    ProcessEvent::Stderr(msg) => view.add_log(msg, "WARNING", Some("stderr".to_string()), cx),
                                    ProcessEvent::Error(msg) => {
                                        view.add_log(msg, "ERROR", Some("flutter".to_string()), cx);
                                        view.run_process = None;
                                    }
                                    ProcessEvent::Exited => {
                                        view.add_log("Flutter process exited".to_string(), "INFO", Some("flutter".to_string()), cx);
                                        view.run_process = None;
                                    }
                                }
                            }).log_err();
                            if should_break {
                                break;
                            }
                        } else {
                            break;
                        }
                    }
                };

                futures::join!(exit_task, consumer);
            }
        }));

        self.connect(cx);
        cx.notify();
    }

    pub fn stop_run(&mut self, cx: &mut Context<Self>) {
        if let Some(process_holder) = self.run_process.take() {
            let mut guard = process_holder.lock().unwrap();
            if let Some(mut child) = guard.take() {
                let _ = child.kill();
            }
        }
        self.run_task = None;
        self.connection_task = None;
        self.vm_service_uri = None;
        self.vm_connected = false;
        cx.notify();
    }

    pub fn send_input(&self, text: &str) {
        use smol::io::AsyncWriteExt;
        if let Some(ref process_holder) = self.run_process {
            let mut guard = process_holder.lock().unwrap();
            if let Some(ref mut child) = *guard {
                if let Some(ref mut stdin) = child.stdin {
                    let bytes = text.as_bytes().to_vec();
                    let stdin_ref = stdin as *mut smol::process::ChildStdin;
                    smol::block_on(async {
                        let stdin = unsafe { &mut *stdin_ref };
                        let _ = stdin.write_all(&bytes).await;
                        let _ = stdin.flush().await;
                    });
                }
            }
        }
    }
}

enum LogEvent {
    Connected,
    Subscribed,
    Disconnected,
    Error(String),
    Log(crate::vm_service::LogRecord),
}

enum ProcessEvent {
    Stdout(String),
    Stderr(String),
    Error(String),
    Exited,
}


impl gpui::Focusable for FlutterLogPanel {
    fn focus_handle(&self, _cx: &App) -> gpui::FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for FlutterLogPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.colors().editor_background)
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .px_2()
                    .py_1()
                    .child(
                         div().flex().items_center().gap_2()
                            .child(div().text_sm().font_weight(gpui::FontWeight::MEDIUM).child("Flutter Logs"))
                            .child(
                                IconButton::new("hot-reload", IconName::BoltFilled)
                                    .icon_size(IconSize::Small)
                                    .disabled(!self.is_running())
                                    .tooltip(|window, cx| Tooltip::text("Hot Reload")(window, cx))
                                .on_click(cx.listener(|_this, _, window, cx| {
                                    window.dispatch_action(HotReload.boxed_clone(), cx);
                                }))
                            )
                            .child(
                                IconButton::new("hot-restart", IconName::RotateCw)
                                    .icon_size(IconSize::Small)
                                    .disabled(!self.is_running())
                                    .tooltip(|window, cx| Tooltip::text("Hot Restart")(window, cx))
                                    .on_click(cx.listener(|_this, _, window, cx| {
                                        window.dispatch_action(HotRestart.boxed_clone(), cx);
                                    }))
                            )
                            .child(div().w_px().h_4().bg(theme.colors().border).mx_1())
                            .child(
                                IconButton::new("clear_logs", IconName::Trash)
                                    .icon_size(IconSize::Small)
                                    .tooltip(|window, cx| Tooltip::text("Clear Logs")(window, cx))
                                    .on_click(cx.listener(|this, _, _window, cx| this.clear_logs(cx)))
                            )
                     )
                    .child(
                        div().flex().items_center().gap_4()
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child(
                                        Checkbox::new(
                                            "auto_scroll",
                                            if self.auto_scroll { ToggleState::Selected } else { ToggleState::Unselected }
                                        )
                                        .on_click(cx.listener(|this, selection, _window, cx| {
                                            this.toggle_auto_scroll(*selection == ToggleState::Selected, cx);
                                        }))
                                    )
                                    .child(div().text_xs().text_color(theme.colors().text_muted).child("Auto-scroll"))
                            )
                            .child(
                                div()
                                    .mr_2()
                                    .text_xs()
                                    .text_color(theme.colors().text_muted)
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
                    .flex_grow()
                    .h_full()
                    .child(self.editor.clone())
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
