use anyhow::{Context as _, Result};
use async_tungstenite::tungstenite::Message;
use async_tungstenite::tokio::connect_async;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::atomic::{AtomicU64, Ordering};

static REQUEST_ID: AtomicU64 = AtomicU64::new(1);

fn next_request_id() -> String {
    REQUEST_ID.fetch_add(1, Ordering::SeqCst).to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogRecord {
    pub message: Option<InstanceRef>,
    pub time: Option<i64>,
    pub level: Option<i32>,
    pub logger_name: Option<InstanceRef>,
    pub error: Option<InstanceRef>,
    pub stack_trace: Option<InstanceRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceRef {
    pub value_as_string: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VmEvent {
    pub kind: String,
    #[serde(default)]
    pub log_record: Option<LogRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamNotifyParams {
    pub stream_id: String,
    pub event: VmEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub result: Option<serde_json::Value>,
    #[serde(default)]
    pub params: Option<serde_json::Value>,
    #[serde(default)]
    pub error: Option<serde_json::Value>,
}

pub struct DartVmService {
    sender: async_tungstenite::WebSocketSender<async_tungstenite::tokio::ConnectStream>,
    receiver: async_tungstenite::WebSocketReceiver<async_tungstenite::tokio::ConnectStream>,
}

impl DartVmService {
    pub async fn connect(uri: &str) -> Result<Self> {
        let (ws_stream, _response) = connect_async(uri)
            .await
            .context("Failed to connect to Dart VM Service")?;

        let (sender, receiver) = ws_stream.split();
        Ok(Self { sender, receiver })
    }

    pub async fn subscribe_logging(&mut self) -> Result<()> {
        let request = json!({
            "jsonrpc": "2.0",
            "method": "streamListen",
            "params": {
                "streamId": "Logging"
            },
            "id": next_request_id()
        });

        self.sender
            .send(Message::Text(request.to_string().into()))
            .await
            .context("Failed to send streamListen request")?;

        // Wait for acknowledgment
        if let Some(msg) = self.receiver.next().await {
            let msg = msg.context("WebSocket read error")?;
            if let Message::Text(text) = msg {
                let response: JsonRpcResponse = serde_json::from_str(&text)
                    .context("Failed to parse streamListen response")?;
                if response.error.is_some() {
                    anyhow::bail!("streamListen failed: {:?}", response.error);
                }
            }
        }

        Ok(())
    }

    pub async fn next_log(&mut self) -> Option<LogRecord> {
        loop {
            let msg = self.receiver.next().await?;
            let msg = msg.ok()?;

            if let Message::Text(text) = msg {
                if let Ok(response) = serde_json::from_str::<JsonRpcResponse>(&text) {
                    if response.method.as_deref() == Some("streamNotify") {
                        if let Some(params) = response.params {
                            if let Ok(notify) = serde_json::from_value::<StreamNotifyParams>(params) {
                                if notify.stream_id == "Logging" && notify.event.kind == "Logging" {
                                    return notify.event.log_record;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Extract VM Service WebSocket URI from terminal output
#[allow(dead_code)]
pub fn extract_vm_service_uri(terminal_output: &str) -> Option<String> {
    let patterns = [
        r"ws://127\.0\.0\.1:\d+/[A-Za-z0-9_=-]+/ws",
        r"ws://localhost:\d+/[A-Za-z0-9_=-]+/ws",
    ];

    for pattern in patterns {
        if let Ok(re) = regex::Regex::new(pattern) {
            if let Some(m) = re.find(terminal_output) {
                return Some(m.as_str().to_string());
            }
        }
    }

    None
}
