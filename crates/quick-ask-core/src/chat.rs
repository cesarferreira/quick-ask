use anyhow::{Result, anyhow, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use reqwest::{StatusCode, blocking::Client, redirect::Policy};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    io::Read,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

const MAX_EVENT_BYTES: usize = 1_048_576;
const MAX_ERROR_BYTES: u64 = 65_536;
const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_REQUEST_IMAGE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_REQUEST_IMAGES: usize = 32;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Low,
    #[default]
    Medium,
    High,
}

impl ReasoningEffort {
    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
        }
    }
}

pub struct ReplyOptions<'a> {
    pub model: &'a str,
    pub reasoning_effort: Option<ReasoningEffort>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ImageAttachment {
    pub name: String,
    pub path: std::path::PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageAttachment>,
}

#[derive(Clone, Debug)]
pub struct Model {
    pub id: String,
    pub name: String,
    pub reasoning_efforts: Vec<ReasoningEffort>,
}

impl Model {
    pub fn resolve_effort(&self, preferred: ReasoningEffort) -> Option<ReasoningEffort> {
        if self.reasoning_efforts.contains(&preferred) {
            Some(preferred)
        } else if self.reasoning_efforts.contains(&ReasoningEffort::Medium) {
            Some(ReasoningEffort::Medium)
        } else {
            self.reasoning_efforts.first().copied()
        }
    }
}

pub fn list_models(access_token: &str) -> Result<Vec<Model>> {
    list_models_at("https://api.openai.com/v1", access_token)
}

fn list_models_at(base: &str, access_token: &str) -> Result<Vec<Model>> {
    let response = client(Duration::from_secs(20))?
        .get(format!("{base}/models"))
        .bearer_auth(access_token)
        .send()
        .map_err(|error| transport_error(error.is_timeout()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(response_error(response, status));
    }
    let mut body = Vec::new();
    response
        .take(MAX_EVENT_BYTES as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|_| anyhow!("The model catalog connection was interrupted. Try again."))?;
    if body.len() > MAX_EVENT_BYTES {
        bail!("The model catalog was too large.");
    }
    let catalog: Value = serde_json::from_slice(&body)
        .map_err(|_| anyhow!("OpenAI returned an invalid model catalog."))?;
    let models = catalog
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("OpenAI returned an invalid model catalog."))?;
    models
        .iter()
        .filter(|model| model["visibility"] == "list")
        .map(|model| {
            let id = model["slug"].as_str().filter(|id| !id.is_empty());
            let name = model["display_name"]
                .as_str()
                .filter(|name| !name.is_empty());
            match (id, name) {
                (Some(id), Some(name)) => Ok(Model {
                    id: id.into(),
                    name: name.into(),
                    reasoning_efforts: match model
                        .get("supported_reasoning_levels")
                        .and_then(Value::as_array)
                    {
                        Some(levels) => [
                            ReasoningEffort::Low,
                            ReasoningEffort::Medium,
                            ReasoningEffort::High,
                        ]
                        .into_iter()
                        .filter(|effort| {
                            levels
                                .iter()
                                .any(|level| level["effort"] == effort.label().to_lowercase())
                        })
                        .collect(),
                        None => vec![],
                    },
                }),
                _ => bail!("OpenAI returned an invalid model catalog."),
            }
        })
        .collect()
}

pub fn stream_reply(
    access_token: &str,
    options: ReplyOptions<'_>,
    messages: &[Message],
    cancel: &AtomicBool,
    on_delta: impl FnMut(String),
) -> Result<()> {
    stream_reply_at(
        "https://api.openai.com/v1",
        access_token,
        options,
        messages,
        cancel,
        on_delta,
        Duration::from_secs(120),
    )
}

fn stream_reply_at(
    base: &str,
    access_token: &str,
    options: ReplyOptions<'_>,
    messages: &[Message],
    cancel: &AtomicBool,
    mut on_delta: impl FnMut(String),
    timeout: Duration,
) -> Result<()> {
    let model = options.model;
    if cancel.load(Ordering::Relaxed) {
        bail!("Response cancelled.");
    }
    if model.trim().is_empty() || messages.is_empty() {
        bail!("Choose a model and enter a message first.");
    }
    if messages
        .iter()
        .any(|message| !matches!(message.role.as_str(), "user" | "assistant" | "developer"))
    {
        bail!("This conversation contains an unsupported message role.");
    }
    let mut payload = json!({
        "model": model,
        "input": request_input(messages, cancel)?,
        "instructions": "You are a helpful assistant. Give clear, concise answers unless the user asks for detail.",
        "store": false,
        "stream": true,
    });
    if let Some(effort) = options.reasoning_effort {
        payload["reasoning"] = json!({ "effort": effort });
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| anyhow!("Could not start the response. Try again."))?;
    // The runtime and response stay scoped to this blocking call. Cancelling drops the HTTP
    // future and socket instead of leaving a background reader consuming the account's usage.
    runtime.block_on(async {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(timeout)
            .redirect(Policy::none())
            .pool_max_idle_per_host(0)
            .build()
            .map_err(|_| anyhow!("Could not initialize the secure OpenAI connection."))?;
        let cancellation = wait_for_cancellation(cancel);
        tokio::pin!(cancellation);
        let request = client
            .post(format!("{base}/responses"))
            .bearer_auth(access_token)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&payload)
            .send();
        let mut response = tokio::select! {
            biased;
            _ = &mut cancellation => bail!("Response cancelled."),
            response = request => response.map_err(|error| transport_error(error.is_timeout()))?,
        };
        let status = response.status();
        let mut decoder = SseDecoder::default();
        let mut error_body = Vec::new();
        loop {
            if cancel.load(Ordering::Relaxed) {
                bail!("Response cancelled.");
            }
            let chunk = tokio::select! {
                biased;
                _ = &mut cancellation => bail!("Response cancelled."),
                chunk = response.chunk() => chunk,
            };
            if !status.is_success() {
                if let Ok(Some(chunk)) = chunk {
                    let remaining = (MAX_ERROR_BYTES as usize).saturating_sub(error_body.len());
                    error_body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                    if error_body.len() < MAX_ERROR_BYTES as usize {
                        continue;
                    }
                }
                let error = serde_json::from_slice(&error_body).unwrap_or(Value::Null);
                return Err(api_error(Some(status), &error));
            }
            let chunk = chunk
                .map_err(|error| {
                    if error.is_timeout() {
                        transport_error(true)
                    } else {
                        anyhow!(
                            "The response connection was interrupted before completion. Try again."
                        )
                    }
                })?
                .ok_or_else(|| {
                    anyhow!("The response stream ended before response.completed. Try again.")
                })?;
            if decoder.feed(&chunk, cancel, &mut |text| {
                on_delta(text);
                Ok(())
            })? {
                return Ok(());
            }
        }
    })
}

fn request_input(messages: &[Message], cancel: &AtomicBool) -> Result<Vec<Value>> {
    let mut image_count = 0usize;
    let mut declared_bytes = 0u64;
    for message in messages {
        if !message.images.is_empty() && message.role != "user" {
            bail!("Images can only be attached to user messages.");
        }
        image_count = image_count.saturating_add(message.images.len());
        if image_count > MAX_REQUEST_IMAGES {
            bail!(
                "This conversation contains more than 32 images. Start a new chat or remove images."
            );
        }
        for image in &message.images {
            let metadata = fs::symlink_metadata(&image.path).map_err(|_| {
                anyhow!(
                    "An attached image is missing or unavailable. Remove it and attach it again."
                )
            })?;
            if !metadata.is_file() {
                bail!("An attached image is not a regular file. Remove it and attach it again.");
            }
            if metadata.len() > MAX_IMAGE_BYTES {
                bail!("An attached image exceeds 8 MiB. Attach a smaller image.");
            }
            declared_bytes += metadata.len();
            if declared_bytes > MAX_REQUEST_IMAGE_BYTES {
                bail!(
                    "This conversation exceeds 64 MiB of images. Start a new chat or remove images."
                );
            }
        }
    }
    let mut actual_bytes = 0u64;
    messages
        .iter()
        .map(|message| {
            if cancel.load(Ordering::Relaxed) {
                bail!("Response cancelled.");
            }
            if message.images.is_empty() {
                return Ok(json!({"role": message.role, "content": message.content}));
            }
            let mut content = Vec::with_capacity(message.images.len() + 1);
            if !message.content.is_empty() {
                content.push(json!({"type": "input_text", "text": message.content}));
            }
            for image in &message.images {
                content.push(read_image_input(image, &mut actual_bytes)?);
            }
            Ok(json!({"role": message.role, "content": content}))
        })
        .collect()
}

fn read_image_input(image: &ImageAttachment, actual_bytes: &mut u64) -> Result<Value> {
    let mut file = File::open(&image.path).map_err(|_| {
        anyhow!("An attached image is missing or unavailable. Remove it and attach it again.")
    })?;
    let before = file.metadata().map_err(|_| {
        anyhow!("An attached image could not be read. Remove it and attach it again.")
    })?;
    if !before.is_file() {
        bail!("An attached image is not a regular file. Remove it and attach it again.");
    }
    if before.len() > MAX_IMAGE_BYTES {
        bail!("An attached image exceeds 8 MiB. Attach a smaller image.");
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_IMAGE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| {
            anyhow!("An attached image could not be read. Remove it and attach it again.")
        })?;
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        bail!("An attached image exceeds 8 MiB. Attach a smaller image.");
    }
    let after = file.metadata().map_err(|_| {
        anyhow!("An attached image changed while being read. Remove it and attach it again.")
    })?;
    if bytes.len() as u64 != before.len()
        || before.len() != after.len()
        || before.modified().ok() != after.modified().ok()
    {
        bail!("An attached image changed while being read. Remove it and attach it again.");
    }
    *actual_bytes += bytes.len() as u64;
    if *actual_bytes > MAX_REQUEST_IMAGE_BYTES {
        bail!("This conversation exceeds 64 MiB of images. Start a new chat or remove images.");
    }
    let mime = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else {
        bail!("An attached image is not a PNG or JPEG. Remove it and attach it again.");
    };
    Ok(json!({
        "type": "input_image",
        "image_url": format!("data:{mime};base64,{}", STANDARD.encode(bytes)),
        "detail": "auto",
    }))
}

async fn wait_for_cancellation(cancel: &AtomicBool) {
    while !cancel.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[derive(Default)]
struct SseDecoder {
    line: Vec<u8>,
    data: String,
    event_name: String,
    skip_lf: bool,
    started: bool,
}

impl SseDecoder {
    fn feed(
        &mut self,
        chunk: &[u8],
        cancel: &AtomicBool,
        on_delta: &mut impl FnMut(String) -> Result<()>,
    ) -> Result<bool> {
        for &byte in chunk {
            if cancel.load(Ordering::Relaxed) {
                bail!("Response cancelled.");
            }
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            if matches!(byte, b'\r' | b'\n') {
                self.skip_lf = byte == b'\r';
                let line = std::mem::take(&mut self.line);
                if self.process_line(&line, on_delta)? {
                    return Ok(true);
                }
            } else {
                if self.line.len() == MAX_EVENT_BYTES {
                    bail!("OpenAI sent a streaming event that was too large.");
                }
                self.line.push(byte);
            }
        }
        Ok(false)
    }

    fn process_line(
        &mut self,
        line: &[u8],
        on_delta: &mut impl FnMut(String) -> Result<()>,
    ) -> Result<bool> {
        let mut line = std::str::from_utf8(line)
            .map_err(|_| anyhow!("OpenAI sent an invalid streaming event."))?;
        if !self.started {
            self.started = true;
            line = line.strip_prefix('\u{feff}').unwrap_or(line);
        }
        if line.is_empty() {
            if self.data.is_empty() {
                self.event_name.clear();
                return Ok(false);
            }
            if self.data.trim() != "[DONE]" {
                let event: Value = serde_json::from_str(&self.data)
                    .map_err(|_| anyhow!("OpenAI sent an invalid streaming event."))?;
                let kind = event
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or(&self.event_name);
                match kind {
                    "response.output_text.delta" => {
                        let delta = event
                            .get("delta")
                            .and_then(Value::as_str)
                            .ok_or_else(|| anyhow!("OpenAI sent an invalid text event."))?;
                        on_delta(delta.into())?;
                    }
                    "response.completed" => return Ok(true),
                    "response.incomplete" => {
                        bail!("The response was incomplete. Try again or start a new chat.")
                    }
                    "response.failed" | "error" => return Err(api_error(None, &event)),
                    _ => {}
                }
            }
            self.data.clear();
            self.event_name.clear();
            return Ok(false);
        }
        if line.starts_with(':') {
            return Ok(false);
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "data" => {
                if self.data.len() + value.len() + 1 > MAX_EVENT_BYTES {
                    bail!("OpenAI sent a streaming event that was too large.");
                }
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(value);
            }
            "event" => {
                self.event_name.clear();
                self.event_name.push_str(value);
            }
            _ => {}
        }
        Ok(false)
    }
}

#[cfg(test)]
fn consume_sse(
    mut reader: impl Read,
    cancel: &AtomicBool,
    mut on_delta: impl FnMut(String) -> Result<()>,
) -> Result<()> {
    let mut decoder = SseDecoder::default();
    let mut chunk = [0; 4096];
    loop {
        if cancel.load(Ordering::Relaxed) {
            bail!("Response cancelled.");
        }
        let count = reader.read(&mut chunk).map_err(|_| {
            anyhow!("The response connection was interrupted before completion. Try again.")
        })?;
        if count == 0 {
            bail!("The response stream ended before response.completed. Try again.");
        }
        if decoder.feed(&chunk[..count], cancel, &mut on_delta)? {
            return Ok(());
        }
    }
}

fn client(timeout: Duration) -> Result<Client> {
    Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(timeout)
        .redirect(Policy::none())
        .build()
        .map_err(|_| anyhow!("Could not initialize the secure OpenAI connection."))
}

fn transport_error(timed_out: bool) -> anyhow::Error {
    if timed_out {
        anyhow!("The OpenAI request timed out. Try again.")
    } else {
        anyhow!("Could not connect to OpenAI. Check your connection and try again.")
    }
}

fn response_error(response: impl Read, status: StatusCode) -> anyhow::Error {
    let mut body = Vec::new();
    let _ = response.take(MAX_ERROR_BYTES).read_to_end(&mut body);
    let error = serde_json::from_slice(&body).unwrap_or(Value::Null);
    api_error(Some(status), &error)
}

fn api_error(status: Option<StatusCode>, body: &Value) -> anyhow::Error {
    let error = body
        .pointer("/response/error")
        .or_else(|| body.get("error"))
        .unwrap_or(body);
    match error.get("code").and_then(Value::as_str).unwrap_or("") {
        "subscription_sharing_user_not_eligible" => {
            anyhow!("ChatGPT plan usage is unavailable for this account, workspace, or policy.")
        }
        "subscription_sharing_usage_limit_exceeded" => anyhow!(
            "This app has reached a ChatGPT plan usage limit. Review usage at https://chatgpt.com/settings/usage before trying again."
        ),
        "subscription_sharing_usage_unavailable" | "subscription_sharing_user_unavailable" => {
            anyhow!(
                "ChatGPT plan usage is temporarily unavailable. Keep your account connected and try again later."
            )
        }
        "subscription_sharing_unsupported_capability" => anyhow!(
            "The selected model or request feature is unsupported for ChatGPT plan usage. Choose another model or start a new chat."
        ),
        "subscription_sharing_route_not_supported" => {
            anyhow!("This request route is unavailable for ChatGPT plan usage.")
        }
        "chatpass_v2_scope_not_authorized" | "chatpass_v2_invalid_authorization_context" => {
            anyhow!(
                "ChatGPT plan permission was not accepted. Check the app registration and sign-in grant."
            )
        }
        "subscription_sharing_invalid_user" => anyhow!(
            "Your ChatGPT identity could not be verified. Reconnect your account if this continues."
        ),
        _ => match status.map(|status| status.as_u16()) {
            Some(401) => anyhow!(
                "ChatGPT sign-in or plan permission was not accepted. Reconnect the selected account."
            ),
            Some(403) => {
                anyhow!("A ChatGPT account, region, or workspace policy prevents this request.")
            }
            Some(429) => anyhow!("OpenAI is limiting requests. Wait before trying again."),
            Some(503) => anyhow!(
                "ChatGPT plan routing is temporarily unavailable. Keep your account connected and try again later."
            ),
            Some(status) => {
                anyhow!("OpenAI could not complete the request (HTTP {status}). Try again later.")
            }
            None => anyhow!("OpenAI could not complete this response. Try again later."),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::{
        fs::{self, File},
        io::{BufRead, BufReader, Cursor, Write},
        net::TcpListener,
        path::Path,
        sync::{Arc, atomic::Ordering, mpsc},
        thread,
        time::Instant,
    };
    use tempfile::tempdir;

    fn server(
        status: u16,
        body: &'static str,
        delay: Duration,
    ) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                headers.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            let mut payload = vec![0; length];
            reader.read_exact(&mut payload).unwrap();
            let _ = tx.send(format!("{headers}{}", String::from_utf8(payload).unwrap()));
            thread::sleep(delay);
            let response = format!(
                "HTTP/1.1 {status} Test\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes());
        });
        (base, rx)
    }

    #[test]
    fn catalog_preserves_account_order_and_excludes_hidden_models() {
        let (base, request) = server(
            200,
            r#"{"models":[{"slug":"fast","display_name":"Fast","visibility":"list"},{"slug":"hidden","display_name":"Hidden","visibility":"hide"},{"slug":"deep","display_name":"Deep","visibility":"list"}]}"#,
            Duration::ZERO,
        );
        let models = list_models_at(&base, "private-token").unwrap();
        assert_eq!(
            models
                .iter()
                .map(|m| (m.id.as_str(), m.name.as_str()))
                .collect::<Vec<_>>(),
            vec![("fast", "Fast"), ("deep", "Deep")]
        );
        let request = request.recv().unwrap();
        assert!(request.starts_with("GET /v1/models HTTP/1.1\r\n"));
        assert!(
            request
                .to_lowercase()
                .contains("authorization: bearer private-token\r\n")
        );
    }

    #[test]
    fn inference_sends_the_selected_effort_and_omits_it_when_unavailable() {
        for (effort, expected) in [
            (Some(ReasoningEffort::Low), json!({"effort": "low"})),
            (Some(ReasoningEffort::Medium), json!({"effort": "medium"})),
            (Some(ReasoningEffort::High), json!({"effort": "high"})),
            (None, Value::Null),
        ] {
            let (base, request) = server(
                200,
                "data: {\"type\":\"response.completed\"}\n\n",
                Duration::ZERO,
            );
            stream_reply_at(
                &base,
                "private-token",
                ReplyOptions {
                    model: "fast",
                    reasoning_effort: effort,
                },
                &[Message {
                    role: "user".into(),
                    content: "Hi".into(),
                    images: vec![],
                }],
                &AtomicBool::new(false),
                |_| {},
                Duration::from_secs(2),
            )
            .unwrap();
            let request = request.recv().unwrap();
            let payload: Value =
                serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
            assert_eq!(
                payload.get("reasoning"),
                if expected.is_null() {
                    None
                } else {
                    Some(&expected)
                }
            );
        }
    }

    #[test]
    fn catalog_limits_efforts_and_resolves_unsupported_selections() {
        let (base, _) = server(
            200,
            r#"{"models":[
            {"slug":"limited","display_name":"Limited","visibility":"list","supported_reasoning_levels":[{"effort":"high"},{"effort":"xhigh"}]},
            {"slug":"plain","display_name":"Plain","visibility":"list","supported_reasoning_levels":[]},
            {"slug":"standard","display_name":"Standard","visibility":"list","supported_reasoning_levels":[{"effort":"low"},{"effort":"medium"},{"effort":"high"}]},
            {"slug":"unknown","display_name":"Unknown","visibility":"list"}
        ]}"#,
            Duration::ZERO,
        );
        let models = list_models_at(&base, "private-token").unwrap();
        assert_eq!(models[0].reasoning_efforts, vec![ReasoningEffort::High]);
        assert_eq!(
            models[0].resolve_effort(ReasoningEffort::Low),
            Some(ReasoningEffort::High)
        );
        assert_eq!(models[1].resolve_effort(ReasoningEffort::High), None);
        assert_eq!(
            models[2].resolve_effort(ReasoningEffort::Low),
            Some(ReasoningEffort::Low)
        );
        assert_eq!(
            models[2].resolve_effort(ReasoningEffort::High),
            Some(ReasoningEffort::High)
        );
        assert_eq!(models[3].resolve_effort(ReasoningEffort::Medium), None);
    }

    #[test]
    fn inference_sends_subscription_compatible_history_and_streams_text() {
        let (base, request) = server(
            200,
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
            Duration::ZERO,
        );
        let messages = vec![
            Message {
                role: "user".into(),
                content: "Hello?".into(),
                images: vec![],
            },
            Message {
                role: "assistant".into(),
                content: "Earlier answer".into(),
                images: vec![],
            },
            Message {
                role: "user".into(),
                content: "Continue".into(),
                images: vec![],
            },
        ];
        let mut output = String::new();
        stream_reply_at(
            &base,
            "private-token",
            ReplyOptions {
                model: "fast",
                reasoning_effort: None,
            },
            &messages,
            &AtomicBool::new(false),
            |text| output.push_str(&text),
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(output, "Hello");
        let request = request.recv().unwrap();
        assert!(request.starts_with("POST /v1/responses HTTP/1.1\r\n"));
        let payload: Value =
            serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(
            payload["input"],
            json!([{"role":"user","content":"Hello?"},{"role":"assistant","content":"Earlier answer"},{"role":"user","content":"Continue"}])
        );
        assert_eq!(payload["store"], false);
        assert_eq!(payload["stream"], true);
        assert_eq!(payload["model"], "fast");
        assert!(payload["instructions"].is_string());
        assert_eq!(payload.as_object().unwrap().len(), 5);
    }

    fn message_with_images(role: &str, content: &str, paths: &[(&str, &Path)]) -> Message {
        serde_json::from_value(json!({
            "role": role,
            "content": content,
            "images": paths.iter().map(|(name, path)| json!({"name": name, "path": path})).collect::<Vec<_>>()
        }))
        .unwrap()
    }

    fn image_payload(messages: &[Message]) -> Value {
        let (base, request) = server(
            200,
            "data: {\"type\":\"response.completed\"}\n\n",
            Duration::ZERO,
        );
        stream_reply_at(
            &base,
            "private-token",
            ReplyOptions {
                model: "fast",
                reasoning_effort: None,
            },
            messages,
            &AtomicBool::new(false),
            |_| {},
            Duration::from_secs(2),
        )
        .unwrap();
        let request = request.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(request.starts_with("POST /v1/responses HTTP/1.1\r\n"));
        serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap()
    }

    #[test]
    fn image_input_sends_text_and_multiple_image_data_urls_without_local_metadata() {
        let directory = tempdir().unwrap();
        let png = directory.path().join("private-png-location");
        let jpeg = directory.path().join("private-jpeg-location");
        fs::write(&png, b"\x89PNG\r\n\x1a\n").unwrap();
        fs::write(&jpeg, b"\xff\xd8\xff\xe0").unwrap();
        let payload = image_payload(&[message_with_images(
            "user",
            "Compare these",
            &[("private-photo-name", &png), ("second-private-name", &jpeg)],
        )]);
        assert_eq!(
            payload["input"],
            json!([{"role":"user","content":[
                {"type":"input_text","text":"Compare these"},
                {"type":"input_image","image_url":"data:image/png;base64,iVBORw0KGgo=","detail":"auto"},
                {"type":"input_image","image_url":"data:image/jpeg;base64,/9j/4A==","detail":"auto"}
            ]}])
        );
        assert_eq!(payload["store"], false);
        assert_eq!(payload["stream"], true);
        assert_eq!(payload.as_object().unwrap().len(), 5);
        let encoded = payload.to_string();
        assert!(!encoded.contains("private-"));
        assert!(!encoded.contains("images"));
        assert!(!encoded.contains(directory.path().to_str().unwrap()));
    }

    #[test]
    fn image_input_replays_earlier_images_in_follow_up_context() {
        let directory = tempdir().unwrap();
        let image = directory.path().join("earlier.png");
        fs::write(&image, b"\x89PNG\r\n\x1a\n").unwrap();
        let payload = image_payload(&[
            message_with_images("user", "Describe this", &[("earlier.png", &image)]),
            message_with_images("assistant", "A picture.", &[]),
            message_with_images("user", "What color is it?", &[]),
        ]);
        assert_eq!(
            payload["input"],
            json!([
                {"role":"user","content":[
                    {"type":"input_text","text":"Describe this"},
                    {"type":"input_image","image_url":"data:image/png;base64,iVBORw0KGgo=","detail":"auto"}
                ]},
                {"role":"assistant","content":"A picture."},
                {"role":"user","content":"What color is it?"}
            ])
        );
    }

    #[test]
    fn image_input_allows_an_image_only_user_message() {
        let directory = tempdir().unwrap();
        let image = directory.path().join("image.png");
        fs::write(&image, b"\x89PNG\r\n\x1a\n").unwrap();
        let payload = image_payload(&[message_with_images("user", "", &[("image.png", &image)])]);
        assert_eq!(
            payload["input"],
            json!([{"role":"user","content":[
                {"type":"input_image","image_url":"data:image/png;base64,iVBORw0KGgo=","detail":"auto"}
            ]}])
        );
    }

    fn assert_image_rejected_before_network(messages: &[Message], expected: &str) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let error = stream_reply_at(
            &base,
            "private-token",
            ReplyOptions {
                model: "fast",
                reasoning_effort: None,
            },
            messages,
            &AtomicBool::new(false),
            |_| panic!("invalid image request emitted text"),
            Duration::from_millis(100),
        )
        .unwrap_err()
        .to_string();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert!(error.contains(expected), "{error}");
        assert!(!error.contains("private-token"));
        assert!(!error.contains("private-location"));
    }

    #[test]
    fn image_input_rejects_missing_nonregular_and_invalid_files_before_network() {
        let directory = tempdir().unwrap();
        let missing = directory.path().join("private-location-missing.png");
        let invalid = directory.path().join("private-location-invalid.png");
        fs::write(&invalid, b"private-token is not an image").unwrap();
        for (path, expected) in [
            (missing.as_path(), "attach"),
            (directory.path(), "regular file"),
            (invalid.as_path(), "PNG or JPEG"),
        ] {
            assert_image_rejected_before_network(
                &[message_with_images(
                    "user",
                    "Describe",
                    &[("private-token", path)],
                )],
                expected,
            );
        }
    }

    #[test]
    fn image_input_rejects_images_on_assistant_and_developer_messages() {
        let directory = tempdir().unwrap();
        let image = directory.path().join("private-location.png");
        fs::write(&image, b"\x89PNG\r\n\x1a\n").unwrap();
        for role in ["assistant", "developer"] {
            assert_image_rejected_before_network(
                &[message_with_images(
                    role,
                    "Describe",
                    &[("private-token", &image)],
                )],
                "user messages",
            );
        }
    }

    #[test]
    fn image_input_rejects_file_count_and_total_byte_limits_before_network() {
        let directory = tempdir().unwrap();
        let large = directory.path().join("private-location-large.png");
        let mut file = File::create(&large).unwrap();
        file.write_all(b"\x89PNG\r\n\x1a\n").unwrap();
        file.set_len(8 * 1024 * 1024 + 1).unwrap();
        assert_image_rejected_before_network(
            &[message_with_images(
                "user",
                "Describe",
                &[("private-token", &large)],
            )],
            "8 MiB",
        );
        file.set_len(8 * 1024 * 1024).unwrap();
        assert_image_rejected_before_network(
            &[
                message_with_images("user", "Earlier", &[("private-token", large.as_path()); 4]),
                message_with_images("user", "Describe", &[("private-token", large.as_path()); 5]),
            ],
            "64 MiB",
        );
        file.set_len(8).unwrap();
        assert_image_rejected_before_network(
            &[
                message_with_images("user", "Earlier", &[("private-token", large.as_path()); 16]),
                message_with_images(
                    "user",
                    "Describe",
                    &[("private-token", large.as_path()); 17],
                ),
            ],
            "32 images",
        );
        let payload = image_payload(&[message_with_images(
            "user",
            "Describe",
            &[("image.png", large.as_path()); 32],
        )]);
        assert_eq!(payload["input"][0]["content"].as_array().unwrap().len(), 33);
    }

    struct Fragmented<'a>(&'a [u8]);
    impl Read for Fragmented<'_> {
        fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
            if self.0.is_empty() {
                return Ok(0);
            }
            output[0] = self.0[0];
            self.0 = &self.0[1..];
            Ok(1)
        }
    }

    #[test]
    fn sse_handles_byte_fragments_multiline_data_comments_and_crlf() {
        let fixture = ": heartbeat\r\nevent: response.output_text.delta\r\ndata: {\"type\":\"response.output_text.delta\",\r\ndata: \"delta\":\"café 🙂\"}\r\n\r\ndata: {\"type\":\"response.completed\"}\r\n\r\n";
        let mut output = String::new();
        consume_sse(
            Fragmented(fixture.as_bytes()),
            &AtomicBool::new(false),
            |text| {
                output.push_str(&text);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(output, "café 🙂");
    }

    #[test]
    fn sse_supports_bom_and_carriage_return_only_lines() {
        let fixture = "\u{feff}event: response.output_text.delta\rdata: {\"delta\":\"Hello\"}\r\revent: response.completed\rdata: {}\r\r";
        let mut output = String::new();
        consume_sse(
            Fragmented(fixture.as_bytes()),
            &AtomicBool::new(false),
            |text| {
                output.push_str(&text);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(output, "Hello");
    }

    #[test]
    fn partial_output_does_not_turn_usage_failure_into_success() {
        let fixture = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Partial\"}\n\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"subscription_sharing_usage_limit_exceeded\",\"message\":\"private-token\"}}}\n\n";
        let mut output = String::new();
        let error = consume_sse(Cursor::new(fixture), &AtomicBool::new(false), |text| {
            output.push_str(&text);
            Ok(())
        })
        .unwrap_err()
        .to_string();
        assert_eq!(output, "Partial");
        assert!(error.contains("https://chatgpt.com/settings/usage"));
        assert!(!error.contains("private-token"));
    }

    #[test]
    fn interrupted_incomplete_and_explicit_error_streams_are_failures() {
        for (fixture, expected) in [
            (
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Partial\"}\n\n",
                "ended",
            ),
            ("data: [DONE]\n\n", "ended"),
            ("data: {\"type\":\"response.incomplete\"}\n\n", "incomplete"),
            (
                "event: error\ndata: {\"code\":\"subscription_sharing_usage_unavailable\",\"message\":\"private-token\"}\n\n",
                "try again later",
            ),
        ] {
            let error = consume_sse(Cursor::new(fixture), &AtomicBool::new(false), |_| Ok(()))
                .unwrap_err()
                .to_string();
            assert!(error.contains(expected), "{error}");
            assert!(!error.contains("private-token"));
        }
    }

    #[test]
    fn eligibility_failure_is_actionable_without_echoing_response_body() {
        let (base, _) = server(
            403,
            r#"{"error":{"code":"subscription_sharing_user_not_eligible","message":"private-token"}}"#,
            Duration::ZERO,
        );
        let error = list_models_at(&base, "private-token")
            .unwrap_err()
            .to_string();
        assert!(error.contains("unavailable for this account"));
        assert!(!error.contains("private-token"));
    }

    #[test]
    fn cancellation_returns_while_server_is_stalled() {
        let (base, received) = server(
            200,
            "data: {\"type\":\"response.completed\"}\n\n",
            Duration::from_secs(2),
        );
        let cancel = Arc::new(AtomicBool::new(false));
        let signal = cancel.clone();
        thread::spawn(move || {
            received.recv().unwrap();
            signal.store(true, Ordering::Relaxed);
        });
        let start = Instant::now();
        let error = stream_reply_at(
            &base,
            "private-token",
            ReplyOptions {
                model: "fast",
                reasoning_effort: None,
            },
            &[Message {
                role: "user".into(),
                content: "Hi".into(),
                images: vec![],
            }],
            &cancel,
            |_| panic!("cancelled request emitted output"),
            Duration::from_secs(3),
        )
        .unwrap_err()
        .to_string();
        assert!(error.to_lowercase().contains("cancel"));
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn cancellation_closes_http_connection_during_silent_or_heartbeat_streams() {
        for heartbeat in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}/v1", listener.local_addr().unwrap());
            let (started_tx, started_rx) = mpsc::channel();
            let (closed_tx, closed_rx) = mpsc::channel();
            let server = thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(socket.try_clone().unwrap());
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse::<usize>().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                drop(reader);
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 1000000\r\n\r\n").unwrap();
                if heartbeat {
                    socket.write_all(b": heartbeat\n\n").unwrap();
                }
                socket.flush().unwrap();
                started_tx.send(()).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_millis(700)))
                    .unwrap();
                let closed = match socket.read(&mut [0]) {
                    Ok(0) => true,
                    Err(error) => matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                    ),
                    _ => false,
                };
                closed_tx.send(closed).unwrap();
            });
            let cancel = Arc::new(AtomicBool::new(false));
            let signal = cancel.clone();
            let canceller = thread::spawn(move || {
                started_rx.recv().unwrap();
                // Cancel while the response body is pending, after local headers have arrived.
                thread::sleep(Duration::from_millis(80));
                signal.store(true, Ordering::Relaxed);
            });
            let result = stream_reply_at(
                &base,
                "private-token",
                ReplyOptions {
                    model: "fast",
                    reasoning_effort: None,
                },
                &[Message {
                    role: "user".into(),
                    content: "Hi".into(),
                    images: vec![],
                }],
                &cancel,
                |_| panic!("a heartbeat emitted answer text"),
                Duration::from_secs(3),
            );
            assert!(result.unwrap_err().to_string().contains("cancel"));
            let closed = closed_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            canceller.join().unwrap();
            server.join().unwrap();
            assert!(
                closed,
                "HTTP connection remained open after cancellation (heartbeat={heartbeat})"
            );
        }
    }

    #[test]
    fn response_timeout_is_bounded_and_does_not_expose_credentials() {
        let (base, _request) = server(200, "", Duration::from_secs(2));
        let start = Instant::now();
        let error = stream_reply_at(
            &base,
            "private-token",
            ReplyOptions {
                model: "fast",
                reasoning_effort: None,
            },
            &[Message {
                role: "user".into(),
                content: "Hi".into(),
                images: vec![],
            }],
            &AtomicBool::new(false),
            |_| {},
            Duration::from_millis(100),
        )
        .unwrap_err()
        .to_string();
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(error.contains("timed out"), "{error}");
        assert!(!error.contains("private-token"));
    }

    #[test]
    fn malformed_and_oversized_stream_events_fail_safely() {
        let large = format!("data: {}\n\n", "x".repeat(1_048_577));
        for (fixture, expected) in [
            ("data: invalid-private-token\n\n", "invalid"),
            (large.as_str(), "too large"),
        ] {
            let error = consume_sse(Cursor::new(fixture), &AtomicBool::new(false), |_| Ok(()))
                .unwrap_err()
                .to_string();
            assert!(error.contains(expected), "{error}");
            assert!(!error.contains("private-token"));
        }
    }
}
