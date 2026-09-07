/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! A [Chrome DevTools Protocol](https://chromedevtools.github.io/devtools-protocol/)
//! (CDP) server for Servo.
//!
//! CDP is the protocol spoken by Chrome's remote debugging interface and is
//! compatible with many automation and debugging tools. The server exposes
//! the browser over a WebSocket, supporting the `Browser`, `Target`, `Page`,
//! `Runtime`, `Network`, `Log`, `Console`, `DOM`, `Emulation`, `Performance`
//! and `Schema` domains (with some methods being no-ops so that clients
//! behave as they do against Chrome).
//!
//! The server is enabled with the `remote_debugging_enabled` preference or
//! the `--remote-debugging-port` command line argument of servoshell, which
//! mirrors the equivalent Chromium flag.

mod dispatch;
mod protocol;
mod websocket;

use std::io::Write as _;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use devtools_traits::{
    ChromeToDevtoolsControlMsg, ConsoleMessage, DebuggerValue, DevtoolsControlMsg,
    DevtoolsPageInfo, NavigationState, NetworkEvent, ScriptToDevtoolsControlMsg, WorkerId,
    get_time_stamp,
};
use embedder_traits::EmbedderProxy;
use log::{info, warn};
use rustc_hash::FxHashMap;
use serde_json::{Value, json};
use servo_base::generic_channel::GenericSender;
use servo_base::id::{BrowsingContextId, PipelineId, WebViewId};
use servo_config::pref;
use servo_url::ServoUrl;
use uuid::Uuid;

use crate::cdp::protocol::{
    CdpError, console_log_level_to_cdp_level, console_log_level_to_cdp_type,
    debugger_value_to_remote_object, event, headers_to_cdp_json, mime_type_from_headers,
    stack_trace_to_json,
};
use crate::cdp::websocket::{WsMessage, WsReceiver, WsStream, WsWriter};

/// How long to wait for a script thread to reply to an `Eval` or DOM request
/// before giving up and reporting an error to the CDP client.
const EVALUATE_TIMEOUT: Duration = Duration::from_secs(60);

/// A shared handle to the CDP server, used to feed browser events into it.
#[derive(Clone)]
pub(crate) struct CdpHandle(Arc<Mutex<CdpServer>>);

impl CdpHandle {
    /// Feeds a browser event into the CDP server, which turns the events that
    /// the connected clients subscribed to into CDP events.
    pub(crate) fn handle_control_msg(&self, msg: &DevtoolsControlMsg) {
        self.0.lock().unwrap().handle_control_msg(msg);
    }
}

/// Starts the CDP server if the `remote_debugging_enabled` preference is set.
/// Returns a handle for feeding browser events into the server, or `None`
/// when the server is disabled or could not be started.
pub(crate) fn start(embedder: EmbedderProxy) -> Option<CdpHandle> {
    if !pref!(remote_debugging_enabled) {
        return None;
    }

    let address = parse_listen_address(&pref!(remote_debugging_listen_address));
    let listener = match TcpListener::bind(address) {
        Ok(listener) => listener,
        Err(error) => {
            warn!("Could not start CDP server on {address}: {error}");
            return None;
        },
    };
    let actual_address = listener.local_addr().ok()?;
    let browser_id = Uuid::new_v4().simple().to_string();
    let web_socket_debugger_url =
        format!("ws://{actual_address}/devtools/browser/{browser_id}");
    // This line is the standard discovery mechanism used by automation
    // tooling (Puppeteer, Playwright, Selenium 4, chrome-remote-interface…).
    println!("DevTools listening on {web_socket_debugger_url}");
    info!("CDP server listening on {actual_address}");

    let server = Arc::new(Mutex::new(CdpServer::new(
        embedder,
        web_socket_debugger_url,
        browser_id,
    )));
    let handle = CdpHandle(server.clone());
    thread::Builder::new()
        .name("CdpAcceptor".to_owned())
        .spawn(move || acceptor_loop(listener, server))
        .expect("Thread spawning failed");

    Some(handle)
}

/// Extracts the Chromium major version from the `Chrome/` token of a
/// user-agent string, falling back to "0" when the user-agent does not
/// identify as Chromium. Used to keep CDP version reporting consistent
/// with the user-agent.
pub(crate) fn chrome_major_version(user_agent: &str) -> &str {
    user_agent
        .split("Chrome/")
        .nth(1)
        .and_then(|rest| rest.split('.').next())
        .unwrap_or("0")
}

/// Resolves the listen address preference, which accepts either an
/// `address:port` pair or a bare port number.
fn parse_listen_address(listen_address: &str) -> SocketAddr {
    if listen_address.is_empty() {
        return SocketAddr::from((Ipv4Addr::LOCALHOST, 9222));
    }
    if let Ok(address) = SocketAddr::from_str(listen_address) {
        return address;
    }
    if let Ok(port) = listen_address.parse::<u16>() {
        return SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    }
    SocketAddr::from((Ipv4Addr::LOCALHOST, 9222))
}

/// Accepts incoming TCP connections. WebSocket upgrade requests are handed
/// over to the CDP server; the HTTP discovery endpoints (`/json/version`,
/// `/json/list`) are answered inline.
fn acceptor_loop(listener: TcpListener, server: Arc<Mutex<CdpServer>>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            continue;
        };
        println!("CDP: accepted a TCP connection");
        let server = Arc::clone(&server);
        // Handle every connection on its own thread: a silent or slow
        // client must never block the accept loop. (When the loop did
        // block, the whole debugging port became unreachable.)
        let _ = thread::Builder::new()
            .name("CdpSetup".to_owned())
            .spawn(move || {
                if let Err(error) = handle_incoming_connection(stream, &server) {
                    println!("CDP: connection rejected: {error}");
                }
            });
    }
}

/// Handles a single incoming TCP connection, which is either an HTTP request
/// to a discovery endpoint or a WebSocket upgrade.
fn handle_incoming_connection(
    mut stream: TcpStream,
    server: &Arc<Mutex<CdpServer>>,
) -> Result<(), std::io::Error> {
    // Bound the discovery/handshake phase: a client that connects but
    // never sends a request must not tie up its handling thread forever.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let (path, headers) = websocket::read_http_request(&mut stream)?;

    let is_websocket_upgrade = headers
        .iter()
        .any(|(name, value)| name == "upgrade" && value.eq_ignore_ascii_case("websocket")) &&
        headers
            .iter()
            .any(|(name, value)| name == "connection" && value.to_ascii_lowercase().contains("upgrade"));

    if !is_websocket_upgrade {
        return handle_http_discovery_request(stream, server, &path, &headers);
    }

    let Some(key) = headers
        .iter()
        .find(|(name, _)| name == "sec-websocket-key")
        .map(|(_, value)| value.clone())
    else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "missing Sec-WebSocket-Key in WebSocket handshake",
        ));
    };

    let ws_stream = WsStream::accept_with_key(stream, &key)?;
    println!("CDP: WebSocket handshake completed");
    register_connection(server, ws_stream);
    Ok(())
}

/// Answers the HTTP discovery endpoints that Chromium exposes on the remote
/// debugging port:
/// - `GET /json/version` — browser metadata,
/// - `GET /json` and `GET /json/list` — the list of inspectable targets.
fn handle_http_discovery_request(
    mut stream: TcpStream,
    server: &Arc<Mutex<CdpServer>>,
    path: &str,
    headers: &[(String, String)],
) -> Result<(), std::io::Error> {
    let host = headers
        .iter()
        .find(|(name, _)| name == "host")
        .map(|(_, value)| value.clone())
        .unwrap_or_else(|| "127.0.0.1".to_owned());

    let (status, body) = {
        let server = server.lock().unwrap();
        match path {
            "/json/version" => ("200 OK", server.version_json(&host)),
            "/json" | "/json/list" => ("200 OK", server.target_list_json(&host)),
            _ => (
                "404 Not Found",
                json!({ "error": "Unknown endpoint" }).to_string(),
            ),
        }
    };

    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()?;
    Ok(())
}

/// Registers a new WebSocket connection with the server and spawns its
/// reader thread, which lives as long as the connection does.
fn register_connection(server: &Arc<Mutex<CdpServer>>, ws_stream: WsStream) {
    ws_stream.clear_read_timeout();
    let (receiver, writer) = ws_stream.split();
    let connection_id = {
        let mut server = server.lock().unwrap();
        let connection_id = server.next_connection_id;
        server.next_connection_id += 1;
        server.connections.insert(
            connection_id,
            CdpConnection {
                writer,
                auto_attach: false,
                target_discovery: false,
            },
        );
        server.flush();
        connection_id
    };

    // The client handler thread needs an owned handle; clone it here, since
    // capturing the reference inside the closure would not be `'static`.
    let server = Arc::clone(server);
    println!("CDP: connection {connection_id} registered, spawning client handler");
    thread::Builder::new()
        .name("CdpClientHandler".to_owned())
        .spawn(move || client_handler_loop(server, connection_id, receiver))
        .expect("Thread spawning failed");
}

/// Reads messages from a client connection until it closes, dispatching
/// every JSON message to the server.
fn client_handler_loop(
    server: Arc<Mutex<CdpServer>>,
    connection_id: u64,
    mut receiver: WsReceiver,
) {
    println!("CDP: client handler for connection {connection_id} started");
    loop {
        match receiver.read_message() {
            Ok(Some(WsMessage::Text(text))) => {
                println!("CDP: connection {connection_id} received a message ({} bytes)", text.len());
                let Ok(message) = serde_json::from_str::<Value>(&text) else {
                    println!("CDP: connection {connection_id} sent a malformed message");
                    continue;
                };
                server.lock().unwrap().handle_client_message(connection_id, &message);
            },
            // Binary, ping and pong frames are handled by the receiver.
            Ok(Some(_)) => {},
            Ok(None) => {
                println!("CDP: connection {connection_id} closed by the client");
                break;
            },
            Err(error) => {
                println!("CDP: connection {connection_id} read error: {error}");
                break;
            },
        }
    }
    server.lock().unwrap().remove_connection(connection_id);
    println!("CDP: connection {connection_id} removed");
}

/// The state of a WebSocket connection to a CDP client.
struct CdpConnection {
    /// The sending half of the WebSocket.
    writer: WsWriter,
    /// Whether the client asked for automatic target attachment via
    /// `Target.setAutoAttach`.
    auto_attach: bool,
    /// Whether the client asked for target discovery notifications via
    /// `Target.setDiscoverTargets`.
    target_discovery: bool,
}

/// A CDP session, which ties a connection to an inspectable target. This is
/// the "flat" session model of the modern protocol: all messages are sent
/// over the same WebSocket and demultiplexed with a `sessionId` member.
struct CdpSession {
    connection_id: u64,
    target_id: BrowsingContextId,
    page_enabled: bool,
    runtime_enabled: bool,
    network_enabled: bool,
    log_enabled: bool,
    console_enabled: bool,
    dom_enabled: bool,
}

/// An inspectable top-level browsing context ("page target" in CDP terms).
struct CdpTarget {
    /// The `targetId` string handed out to clients.
    target_id_string: String,
    /// The WebView this browsing context belongs to.
    webview_id: WebViewId,
    title: String,
    url: String,
    loading: bool,
    current_pipeline: Option<PipelineId>,
}

/// A script global ("execution context" in CDP terms).
struct CdpPipeline {
    browsing_context_id: BrowsingContextId,
    execution_context_id: i64,
    url: String,
    script_sender: Option<GenericSender<devtools_traits::DevtoolScriptControlMsg>>,
}

/// A DOM node reference used by the `DOM` domain to resolve `nodeId`s back
/// to script-side nodes.
struct DomNodeRef {
    pipeline_id: PipelineId,
    unique_id: String,
}

/// The maximum number of bytes of a response body that is kept in memory for
/// `Network.getResponseBody`. Larger bodies are dropped.
const MAX_CAPTURED_BODY_SIZE: usize = 20 * 1024 * 1024;

/// The maximum number of finished network requests to remember. Oldest
/// entries are dropped beyond this, mirroring how the Chrome DevTools
/// frontend keeps a bounded buffer.
const MAX_REMEMBERED_NETWORK_REQUESTS: usize = 500;

/// The state of a network request that is in flight, used to correlate
/// request and response events.
struct NetworkRequestState {
    browsing_context_id: BrowsingContextId,
    url: String,
    finished: bool,
    /// The response body, when the fetch layer captured one for devtools.
    body: Option<Vec<u8>>,
    /// The MIME type of the response, from the `Content-Type` header.
    mime_type: String,
}

/// The CDP server state. All access happens behind the mutex inside
/// [`CdpHandle`] or the acceptor thread.
pub(crate) struct CdpServer {
    /// The point in time the server started, used for CDP `timestamp`
    /// members, which are monotonically increasing seconds with an arbitrary
    /// epoch.
    started_at: Instant,
    next_connection_id: u64,
    next_session_number: u64,
    next_execution_context_id: i64,
    next_node_id: i64,
    next_target_number: u64,
    connections: FxHashMap<u64, CdpConnection>,
    sessions: FxHashMap<String, CdpSession>,
    targets: FxHashMap<BrowsingContextId, CdpTarget>,
    target_ids: FxHashMap<String, BrowsingContextId>,
    pipelines: FxHashMap<PipelineId, CdpPipeline>,
    network_requests: FxHashMap<String, NetworkRequestState>,
    dom_nodes: FxHashMap<i64, DomNodeRef>,
    /// WebViews that were created through `Target.createTarget` but whose
    /// browsing context has not been observed yet. Maps the webview id to
    /// the target id string that was already handed out to the client.
    pending_targets: FxHashMap<WebViewId, String>,
    /// `Target.createTarget` replies held back until the new browsing
    /// context registers (`NewGlobal`), so the client never receives a
    /// target id that cannot be attached to yet.
    pending_create_replies: FxHashMap<WebViewId, (u64, Value)>,
    /// Messages queued for delivery to connections, flushed at the end of
    /// each entry point to keep mutex critical sections free of I/O.
    outbox: Vec<(u64, Value)>,
    /// Handle to the embedder, used to run browser-level automation
    /// commands (screenshots, creating and closing webviews, focusing).
    embedder: EmbedderProxy,
    web_socket_debugger_url: String,
    browser_id: String,
}

impl CdpServer {
    fn new(
        embedder: EmbedderProxy,
        web_socket_debugger_url: String,
        browser_id: String,
    ) -> Self {
        Self {
            started_at: Instant::now(),
            next_connection_id: 0,
            next_session_number: 0,
            next_execution_context_id: 0,
            next_node_id: 0,
            next_target_number: 0,
            connections: FxHashMap::default(),
            sessions: FxHashMap::default(),
            targets: FxHashMap::default(),
            target_ids: FxHashMap::default(),
            pipelines: FxHashMap::default(),
            network_requests: FxHashMap::default(),
            dom_nodes: FxHashMap::default(),
            pending_targets: FxHashMap::default(),
            pending_create_replies: FxHashMap::default(),
            outbox: Vec::new(),
            embedder,
            web_socket_debugger_url,
            browser_id,
        }
    }

    /// The CDP `timestamp` of "now": monotonically increasing seconds since
    /// the server started.
    fn timestamp_now(&self) -> f64 {
        self.started_at.elapsed().as_secs_f64()
    }

    /// Queues a message for a single connection.
    fn send_to_connection(&mut self, connection_id: u64, message: Value) {
        self.outbox.push((connection_id, message));
    }

    /// Queues an error reply for a single connection.
    fn send_error_to_connection(&mut self, connection_id: u64, id: Value, error: CdpError) {
        self.send_to_connection(
            connection_id,
            json!({ "id": id, "error": error.to_json() }),
        );
    }

    /// Queues a reply for a single connection.
    fn send_reply_to_connection(&mut self, connection_id: u64, id: Value, result: Value) {
        self.send_to_connection(connection_id, json!({ "id": id, "result": result }));
    }

    /// Queues an event for every session attached to the given target for
    /// which `filter` returns true.
    fn send_to_target_sessions<F>(&mut self, target_id: BrowsingContextId, filter: F, event: Value)
    where
        F: Fn(&CdpSession) -> bool,
    {
        let session_ids: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, session)| session.target_id == target_id && filter(session))
            .map(|(id, _)| id.clone())
            .collect();
        for session_id in session_ids {
            let connection_id = self.sessions[&session_id].connection_id;
            let mut event = event.clone();
            event["sessionId"] = json!(session_id);
            self.send_to_connection(connection_id, event);
        }
    }

    /// Delivers all queued messages.
    fn flush(&mut self) {
        let queued = self.outbox.len();
        if queued > 0 {
            println!("CDP: flushing {queued} queued message(s)");
        }
        for (connection_id, message) in std::mem::take(&mut self.outbox) {
            let Some(connection) = self.connections.get_mut(&connection_id) else {
                println!("CDP: dropping queued message for unknown connection {connection_id}");
                continue;
            };
            let Ok(text) = serde_json::to_string(&message) else {
                continue;
            };
            let written_len = text.len();
            let written_head: String = text.chars().take(120).collect();
            if let Err(error) = connection.writer.write_message(&WsMessage::Text(text)) {
                println!("CDP: failed to write CDP message to connection {connection_id}: {error}");
            } else {
                println!("CDP: wrote {written_len} byte(s) to connection {connection_id}: {written_head}");
            }
        }
    }

    /// Removes a connection and closes all of its sessions.
    fn remove_connection(&mut self, connection_id: u64) {
        self.connections.remove(&connection_id);
        let detached_sessions: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, session)| session.connection_id == connection_id)
            .map(|(id, _)| id.clone())
            .collect();
        for session_id in detached_sessions {
            self.sessions.remove(&session_id);
        }
    }

    /// Feeds a browser event into the server.
    fn handle_control_msg(&mut self, msg: &DevtoolsControlMsg) {
        match msg {
            DevtoolsControlMsg::FromScript(script_msg) => self.handle_script_msg(script_msg),
            DevtoolsControlMsg::FromChrome(chrome_msg) => self.handle_chrome_msg(chrome_msg),
            DevtoolsControlMsg::ClientExited => {},
        }
        self.flush();
    }

    fn handle_chrome_msg(&mut self, msg: &ChromeToDevtoolsControlMsg) {
        match msg {
            ChromeToDevtoolsControlMsg::NetworkEvent(request_id, network_event) => {
                self.handle_network_event(request_id, network_event);
            },
            ChromeToDevtoolsControlMsg::ServerExitMsg => {
                let connection_ids: Vec<u64> = self.connections.keys().copied().collect();
                for connection_id in connection_ids {
                    self.remove_connection(connection_id);
                }
            },
            ChromeToDevtoolsControlMsg::AddClient(_) |
            ChromeToDevtoolsControlMsg::CollectMemoryReport(_) => {},
        }
    }

    fn handle_script_msg(&mut self, msg: &ScriptToDevtoolsControlMsg) {
        match msg {
            ScriptToDevtoolsControlMsg::NewGlobal(ids, script_sender, page_info) => {
                self.handle_new_global(*ids, script_sender, page_info);
            },
            ScriptToDevtoolsControlMsg::Navigate(browsing_context_id, state) => {
                self.handle_navigate(*browsing_context_id, state);
            },
            ScriptToDevtoolsControlMsg::ConsoleAPI(pipeline_id, console_message, worker_id) => {
                if worker_id.is_none() {
                    self.handle_console_api(*pipeline_id, console_message);
                }
            },
            ScriptToDevtoolsControlMsg::ClearConsole(pipeline_id, worker_id) => {
                if worker_id.is_none() {
                    self.handle_clear_console(*pipeline_id);
                }
            },
            ScriptToDevtoolsControlMsg::TitleChanged(pipeline_id, title) => {
                self.handle_title_changed(*pipeline_id, title);
            },
            ScriptToDevtoolsControlMsg::ReportPageError(pipeline_id, page_error) => {
                let Some(pipeline) = self.pipelines.get(pipeline_id) else {
                    return;
                };
                let browsing_context_id = pipeline.browsing_context_id;
                let entry = json!({
                    "source": "javascript",
                    "level": "error",
                    "text": page_error.error_message,
                    "timestamp": page_error.time_stamp,
                    "url": page_error.source_name,
                    "lineNumber": page_error.line_number.saturating_sub(1),
                });
                self.send_to_target_sessions(
                    browsing_context_id,
                    |session| session.log_enabled,
                    event("Log.entryAdded", entry),
                );
            },
            _ => {},
        }
    }

    fn handle_new_global(
        &mut self,
        ids: (BrowsingContextId, PipelineId, Option<WorkerId>, WebViewId),
        script_sender: &GenericSender<devtools_traits::DevtoolScriptControlMsg>,
        page_info: &DevtoolsPageInfo,
    ) {
        let (browsing_context_id, pipeline_id, worker_id, webview_id) = ids;
        if worker_id.is_some() {
            return;
        }

        let execution_context_id = self.next_execution_context_id;
        self.next_execution_context_id += 1;

        // A target id may have been pre-assigned by `Target.createTarget`,
        // which hands the string out before the browsing context exists.
        let pre_assigned_target_id = self.pending_targets.remove(&webview_id);
        let previous_pipeline = if let Some(target) = self.targets.get_mut(&browsing_context_id) {
            let previous = target.current_pipeline;
            target.current_pipeline = Some(pipeline_id);
            target.title = page_info.title.clone();
            target.url = page_info.url.to_string();
            previous
        } else {
            let pre_assigned = pre_assigned_target_id.is_some();
            let target_id_string = pre_assigned_target_id.unwrap_or_else(|| {
                let target_id_string = format!("page-{}", self.next_target_number);
                self.next_target_number += 1;
                target_id_string
            });
            let reply_target_id = target_id_string.clone();
            self.target_ids
                .insert(target_id_string.clone(), browsing_context_id);
            self.targets.insert(
                browsing_context_id,
                CdpTarget {
                    target_id_string,
                    webview_id,
                    title: page_info.title.clone(),
                    url: page_info.url.to_string(),
                    loading: false,
                    current_pipeline: Some(pipeline_id),
                },
            );
            // The target is attachable from this point on: release the
            // `Target.createTarget` reply that was held back for it.
            if pre_assigned &&
                let Some((connection_id, message_id)) = self.pending_create_replies.remove(&webview_id)
            {
                self.send_reply_to_connection(
                    connection_id,
                    message_id,
                    json!({ "targetId": reply_target_id }),
                );
            }
            None
        };

        self.pipelines.insert(
            pipeline_id,
            CdpPipeline {
                browsing_context_id,
                execution_context_id,
                url: page_info.url.to_string(),
                script_sender: Some(script_sender.clone()),
            },
        );

        // Notify connections that asked for target discovery about the new
        // (or first observed) target.
        if previous_pipeline.is_none() {
            let target_info = self.target_info_json(browsing_context_id);
            let connection_ids: Vec<u64> = self
                .connections
                .iter()
                .filter(|(_, connection)| connection.target_discovery)
                .map(|(id, _)| *id)
                .collect();
            for connection_id in connection_ids {
                self.send_to_connection(
                    connection_id,
                    event("Target.targetCreated", json!({ "targetInfo": target_info })),
                );
            }
        }

        // Destroy the execution context of the replaced pipeline and create
        // the new one for every attached session.
        if let Some(previous_pipeline) = previous_pipeline &&
            previous_pipeline != pipeline_id
        {
            let previous = self.pipelines.remove(&previous_pipeline);
            if let Some(previous) = previous {
                self.send_to_target_sessions(
                    previous.browsing_context_id,
                    |session| session.runtime_enabled,
                    event(
                        "Runtime.executionContextDestroyed",
                        json!({ "executionContextId": previous.execution_context_id }),
                    ),
                );
            }
        }

        let origin = self
            .pipelines
            .get(&pipeline_id)
            .map(|pipeline| origin_of(&pipeline.url))
            .unwrap_or_else(|| "about:blank".to_owned());
        let target_id_string = self.targets[&browsing_context_id].target_id_string.clone();
        self.send_to_target_sessions(
            browsing_context_id,
            |session| session.runtime_enabled,
            event(
                "Runtime.executionContextCreated",
                json!({
                    "context": {
                        "id": execution_context_id,
                        "origin": origin,
                        "name": page_info.title,
                        "uniqueId": format!("{pipeline_id:?}"),
                        "isDefault": true,
                        "auxData": {
                            "isDefault": true,
                            "frameId": target_id_string,
                            "type": "default",
                        },
                    },
                }),
            ),
        );

        // Auto-attach clients to newly discovered targets.
        if previous_pipeline.is_none() {
            let connection_ids: Vec<u64> = self
                .connections
                .iter()
                .filter(|(_, connection)| connection.auto_attach)
                .map(|(id, _)| *id)
                .collect();
            for connection_id in connection_ids {
                self.attach_session(connection_id, browsing_context_id);
            }
        }
    }

    fn handle_navigate(&mut self, browsing_context_id: BrowsingContextId, state: &NavigationState) {
        let Some(target) = self.targets.get_mut(&browsing_context_id) else {
            return;
        };
        match state {
            NavigationState::Start(url) => {
                target.loading = true;
                target.url = url.to_string();
                let target_id_string = target.target_id_string.clone();
                let target_info = self.target_info_json(browsing_context_id);
                self.send_to_target_sessions(
                    browsing_context_id,
                    |session| session.page_enabled,
                    event(
                        "Page.frameStartedLoading",
                        json!({ "frameId": target_id_string }),
                    ),
                );
                self.send_to_target_sessions(
                    browsing_context_id,
                    |session| session.page_enabled,
                    event("Target.targetInfoChanged", json!({ "targetInfo": target_info })),
                );
            },
            NavigationState::Stop(pipeline_id, page_info) => {
                target.loading = false;
                target.title = page_info.title.clone();
                target.url = page_info.url.to_string();
                target.current_pipeline = Some(*pipeline_id);
                let target_id_string = target.target_id_string.clone();

                // Make sure the pipeline is known even if the `NewGlobal`
                // message has not been processed yet. Without a script
                // channel, evaluation is unavailable until the global shows
                // up.
                if !self.pipelines.contains_key(pipeline_id) {
                    let execution_context_id = self.next_execution_context_id;
                    self.next_execution_context_id += 1;
                    self.pipelines.insert(
                        *pipeline_id,
                        CdpPipeline {
                            browsing_context_id,
                            execution_context_id,
                            url: page_info.url.to_string(),
                            script_sender: None,
                        },
                    );
                }

                let origin = origin_of(&page_info.url.to_string());
                self.send_to_target_sessions(
                    browsing_context_id,
                    |session| session.page_enabled,
                    event(
                        "Page.frameNavigated",
                        json!({
                            "frame": {
                                "id": target_id_string,
                                "loaderId": "",
                                "url": page_info.url.to_string(),
                                "domainAndRegistry": "",
                                "securityOrigin": origin,
                                "mimeType": "text/html",
                            },
                            "type": "Navigation",
                        }),
                    ),
                );
                self.send_to_target_sessions(
                    browsing_context_id,
                    |session| session.page_enabled,
                    event(
                        "Page.frameStoppedLoading",
                        json!({ "frameId": target_id_string }),
                    ),
                );
                self.send_to_target_sessions(
                    browsing_context_id,
                    |session| session.page_enabled,
                    event(
                        "Page.loadEventFired",
                        json!({ "timestamp": get_time_stamp() as f64 / 1000.0 }),
                    ),
                );
            },
        }
    }

    fn handle_console_api(&mut self, pipeline_id: PipelineId, console_message: &ConsoleMessage) {
        let Some(pipeline) = self.pipelines.get(&pipeline_id) else {
            return;
        };
        let browsing_context_id = pipeline.browsing_context_id;
        let execution_context_id = pipeline.execution_context_id;

        let arguments: Vec<Value> = console_message
            .arguments
            .iter()
            .map(debugger_value_to_remote_object)
            .collect();
        let text = console_message
            .arguments
            .iter()
            .map(debugger_value_to_text)
            .collect::<Vec<String>>()
            .join(" ");

        let mut console_called = json!({
            "type": console_log_level_to_cdp_type(&console_message.fields.level),
            "args": arguments,
            "executionContextId": execution_context_id,
            "timestamp": console_message.fields.time_stamp as f64 / 1000.0,
        });
        if let Some(stack_trace) = stack_trace_to_json(console_message.stacktrace.as_deref()) {
            console_called["stackTrace"] = stack_trace.clone();
        }
        self.send_to_target_sessions(
            browsing_context_id,
            |session| session.runtime_enabled,
            event("Runtime.consoleAPICalled", console_called),
        );

        let mut entry = json!({
            "source": "console-api",
            "level": console_log_level_to_cdp_level(&console_message.fields.level),
            "text": text,
            "timestamp": console_message.fields.time_stamp,
            "url": console_message.fields.filename,
            "lineNumber": console_message.fields.line_number.saturating_sub(1),
        });
        if let Some(stack_trace) = stack_trace_to_json(console_message.stacktrace.as_deref()) {
            entry["stackTrace"] = stack_trace;
        }
        self.send_to_target_sessions(
            browsing_context_id,
            |session| session.log_enabled,
            event("Log.entryAdded", entry),
        );
    }

    fn handle_clear_console(&mut self, pipeline_id: PipelineId) {
        let Some(pipeline) = self.pipelines.get(&pipeline_id) else {
            return;
        };
        let browsing_context_id = pipeline.browsing_context_id;
        self.send_to_target_sessions(
            browsing_context_id,
            |session| session.runtime_enabled,
            event("Runtime.consoleCleared", json!({})),
        );
    }

    fn handle_title_changed(&mut self, pipeline_id: PipelineId, title: &str) {
        let Some(pipeline) = self.pipelines.get(&pipeline_id) else {
            return;
        };
        let browsing_context_id = pipeline.browsing_context_id;
        let Some(target) = self.targets.get_mut(&browsing_context_id) else {
            return;
        };
        target.title = title.to_owned();
        let target_info = self.target_info_json(browsing_context_id);
        self.send_to_target_sessions(
            browsing_context_id,
            |session| session.runtime_enabled || session.page_enabled,
            event("Target.targetInfoChanged", json!({ "targetInfo": target_info })),
        );
    }

    fn handle_network_event(&mut self, request_id: &str, network_event: &NetworkEvent) {
        match network_event {
            NetworkEvent::HttpRequest(http_request) => {
                let browsing_context_id = http_request.browsing_context_id;
                if !self.targets.contains_key(&browsing_context_id) {
                    return;
                }
                let url = http_request.url.to_string();
                let wall_time = http_request.time_stamp as f64 / 1000.0;
                let request_json = json!({
                    "url": url,
                    "method": http_request.method.as_str(),
                    "headers": headers_to_cdp_json(Some(&http_request.headers)),
                });
                self.network_requests.insert(
                    request_id.to_owned(),
                    NetworkRequestState {
                        browsing_context_id,
                        url: url.clone(),
                        finished: false,
                        body: None,
                        mime_type: String::new(),
                    },
                );
                self.send_to_target_sessions(
                    browsing_context_id,
                    |session| session.network_enabled,
                    event(
                        "Network.requestWillBeSent",
                        json!({
                            "requestId": request_id,
                            "loaderId": "",
                            "documentURL": url,
                            "request": request_json,
                            "timestamp": self.timestamp_now(),
                            "wallTime": wall_time,
                            "initiator": { "type": "other" },
                            "redirectHasExtraInfoLoaded": false,
                        }),
                    ),
                );
            },
            NetworkEvent::HttpRequestUpdate(_) => {
                // Redirect and cookie updates are not mapped to the CDP.
            },
            NetworkEvent::HttpResponse(http_response) => {
                let Some(state) = self.network_requests.get_mut(request_id) else {
                    return;
                };
                if state.finished {
                    return;
                }
                state.finished = true;
                let browsing_context_id = state.browsing_context_id;
                let url = state.url.clone();
                let status = http_response
                    .status
                    .try_code()
                    .map(|code| code.as_u16())
                    .unwrap_or(0);
                let headers = headers_to_cdp_json(http_response.headers.as_ref());
                let mime_type = mime_type_from_headers(http_response.headers.as_ref());
                let from_disk_cache = http_response.from_cache;
                let frame_id = self
                    .targets
                    .get(&browsing_context_id)
                    .map(|target| target.target_id_string.clone())
                    .unwrap_or_default();

                // Keep the body (up to a limit) so that
                // `Network.getResponseBody` can serve it later.
                state.body = http_response
                    .body
                    .as_ref()
                    .filter(|body| body.len() <= MAX_CAPTURED_BODY_SIZE)
                    .map(|body| body.to_vec());
                state.mime_type = mime_type.clone();

                // Prune old finished requests to keep memory bounded.
                if self.network_requests.len() > MAX_REMEMBERED_NETWORK_REQUESTS * 2 {
                    let finished_ids: Vec<String> = self
                        .network_requests
                        .iter()
                        .filter(|(_, request)| request.finished)
                        .take(self.network_requests.len() - MAX_REMEMBERED_NETWORK_REQUESTS)
                        .map(|(id, _)| id.clone())
                        .collect();
                    for id in finished_ids {
                        self.network_requests.remove(&id);
                    }
                }

                self.send_to_target_sessions(
                    browsing_context_id,
                    |session| session.network_enabled,
                    event(
                        "Network.responseReceived",
                        json!({
                            "requestId": request_id,
                            "loaderId": "",
                            "frameId": frame_id,
                            "timestamp": self.timestamp_now(),
                            "type": "Other",
                            "response": {
                                "url": url,
                                "status": status,
                                "statusText": status_text_for(status),
                                "headers": headers,
                                "mimeType": mime_type,
                                "connectionReused": true,
                                "connectionId": 0,
                                "encodedDataLength": -1,
                                "fromDiskCache": from_disk_cache,
                                "fromServiceWorker": false,
                            },
                        }),
                    ),
                );

                if status == 0 {
                    self.send_to_target_sessions(
                        browsing_context_id,
                        |session| session.network_enabled,
                        event(
                            "Network.loadingFailed",
                            json!({
                                "requestId": request_id,
                                "timestamp": self.timestamp_now(),
                                "type": "Other",
                                "errorText": "net::ERR_FAILED",
                                "canceled": false,
                            }),
                        ),
                    );
                } else {
                    self.send_to_target_sessions(
                        browsing_context_id,
                        |session| session.network_enabled,
                        event(
                            "Network.loadingFinished",
                            json!({
                                "requestId": request_id,
                                "timestamp": self.timestamp_now(),
                                "encodedDataLength": -1,
                            }),
                        ),
                    );
                }
            },
            NetworkEvent::SecurityInfo(_) => {},
        }
    }

    /// Attaches a new session between a connection and a target, emitting the
    /// `Target.attachedToTarget` event that introduces the session.
    fn attach_session(&mut self, connection_id: u64, target_id: BrowsingContextId) {
        // Do not attach the same target twice for the same connection.
        if self.sessions.values().any(|session| {
            session.connection_id == connection_id && session.target_id == target_id
        }) {
            return;
        }

        let session_number = self.next_session_number;
        self.next_session_number += 1;
        let session_id = session_number.to_string();
        let target_info = self.target_info_json(target_id);

        self.sessions.insert(
            session_id.clone(),
            CdpSession {
                connection_id,
                target_id,
                page_enabled: false,
                runtime_enabled: false,
                network_enabled: false,
                log_enabled: false,
                console_enabled: false,
                dom_enabled: false,
            },
        );

        let mut attached = event(
            "Target.attachedToTarget",
            json!({
                "sessionId": session_id,
                "targetInfo": target_info,
                "waitingForDebugger": false,
            }),
        );
        attached["sessionId"] = json!(session_id);
        self.send_to_connection(connection_id, attached);
    }

    /// Serializes the `Target.TargetInfo` for a target.
    fn target_info_json(&self, target_id: BrowsingContextId) -> Value {
        let Some(target) = self.targets.get(&target_id) else {
            return json!({});
        };
        let attached = self
            .sessions
            .values()
            .any(|session| session.target_id == target_id);
        json!({
            "targetId": target.target_id_string,
            "type": "page",
            "title": target.title,
            "url": target.url,
            "attached": attached,
            "canAccessOpener": false,
            "browserContextId": self.browser_id,
        })
    }

    /// The `Browser.getVersion` reply payload as a JSON string.
    fn version_json(&self, host: &str) -> String {
        let user_agent = pref!(user_agent);
        let chrome_version = chrome_major_version(&user_agent);
        json!({
            "Browser": format!("Chrome/{chrome_version}.0.0.0"),
            "Protocol-Version": "1.3",
            "User-Agent": user_agent,
            "V8-Version": format!("{chrome_version}.0.0.0"),
            "WebKit-Version": "537.36 (@0)",
            "jsVersion": format!("{chrome_version}.0.0.0"),
            "webSocketDebuggerUrl": self.web_socket_debugger_url,
            "host": host,
        })
        .to_string()
    }

    /// The `/json/list` payload: all inspectable page targets.
    fn target_list_json(&self, host: &str) -> String {
        let targets: Vec<Value> = self
            .targets
            .values()
            .map(|target| {
                json!({
                    "description": "",
                    "devtoolsFrontendUrl": format!(
                        "devtools://devtools/bundled/inspector.html?ws={host}/devtools/page/{}",
                        target.target_id_string
                    ),
                    "id": target.target_id_string,
                    "title": target.title,
                    "type": "page",
                    "url": target.url,
                    "webSocketDebuggerUrl":
                        format!("ws://{host}/devtools/page/{}", target.target_id_string),
                })
            })
            .collect();
        Value::Array(targets).to_string()
    }
}

/// Converts a [`DebuggerValue`] to a plain-text rendering, used for
/// `Log.entryAdded` text members.
fn debugger_value_to_text(value: &DebuggerValue) -> String {
    match value {
        DebuggerValue::VoidValue => "undefined".to_owned(),
        DebuggerValue::NullValue(_) => "null".to_owned(),
        DebuggerValue::BooleanValue(boolean) => boolean.to_string(),
        DebuggerValue::NumberValue(number) => {
            if number.is_finite() {
                number.to_string()
            } else if number.is_nan() {
                "NaN".to_owned()
            } else if *number > 0.0 {
                "Infinity".to_owned()
            } else {
                "-Infinity".to_owned()
            }
        },
        DebuggerValue::StringValue(string) => string.clone(),
        DebuggerValue::ObjectValue { class, .. } => class.clone(),
    }
}

/// Derives the origin ("scheme://host[:port]") of a URL string for
/// `Runtime.executionContextCreated`.
fn origin_of(url: &str) -> String {
    ServoUrl::parse(url)
        .map(|url| url.origin().ascii_serialization().into_owned())
        .unwrap_or_else(|_| "about:blank".to_owned())
}

/// The HTTP status text for well-known status codes, empty otherwise.
fn status_text_for(status: u16) -> String {
    match status {
        200 => "OK".to_owned(),
        204 => "No Content".to_owned(),
        301 => "Moved Permanently".to_owned(),
        302 => "Found".to_owned(),
        304 => "Not Modified".to_owned(),
        307 => "Temporary Redirect".to_owned(),
        308 => "Permanent Redirect".to_owned(),
        400 => "Bad Request".to_owned(),
        401 => "Unauthorized".to_owned(),
        403 => "Forbidden".to_owned(),
        404 => "Not Found".to_owned(),
        500 => "Internal Server Error".to_owned(),
        502 => "Bad Gateway".to_owned(),
        503 => "Service Unavailable".to_owned(),
        _ => String::new(),
    }
}
