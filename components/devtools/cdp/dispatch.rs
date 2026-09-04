/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Dispatching of client-to-browser CDP messages ("commands") for the
//! [`CdpServer`]. Commands are addressed either to the browser (no session)
//! or to a session created with `Target.attachToTarget`.

use std::io::Cursor;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use devtools_traits::{DevtoolScriptControlMsg, EvaluateJSReply, NodeInfo};
use embedder_traits::WebDriverCommandMsg;
use euclid::Rect;
use image::{DynamicImage, ImageFormat};
use serde_json::{Value, json};
use servo_base::generic_channel;
use servo_base::id::PipelineId;
use servo_config::pref;
use servo_url::ServoUrl;

use super::origin_of;
use crate::cdp::protocol::{CdpError, event, evaluate_reply_to_remote_object};
use crate::cdp::{CdpServer, DomNodeRef, EVALUATE_TIMEOUT, chrome_major_version};

/// How long to wait for the embedder to run a browser-level automation
/// command (like taking a screenshot) before giving up.
const AUTOMATION_COMMAND_TIMEOUT: Duration = Duration::from_secs(15);

/// Methods of the `Emulation` domain that are accepted as no-ops so that
/// clients observe Chrome-like behavior.
const EMULATION_NOOP_METHODS: &[&str] = &[
    "Emulation.setDeviceMetricsOverride",
    "Emulation.clearDeviceMetricsOverride",
    "Emulation.setUserAgentOverride",
    "Emulation.setEmulatedMedia",
    "Emulation.setTouchEmulationEnabled",
    "Emulation.setScriptExecutionDisabled",
    "Emulation.setScrollbarsHidden",
    "Emulation.setCPUThrottlingRate",
    "Emulation.setAutoDarkModeOverride",
    "Emulation.setLocaleOverride",
    "Emulation.setTimezoneOverride",
    "Emulation.setVirtualTimePolicy",
    "Emulation.setAutomationOverride",
];

/// Methods that are accepted as no-ops, either because Servo already behaves
/// the way the method asks for or because the behavior cannot be changed at
/// runtime. Accepted for client compatibility.
const SESSION_NOOP_METHODS: &[&str] = &[
    "Page.setLifecycleEventsEnabled",
    "Page.setBypassCSP",
    "Page.setInterceptFileChooserDialog",
    "Page.setAdBlockingEnabled",
    "Page.setDownloadBehavior",
    "Runtime.setAsyncCallStackDepth",
    "Runtime.setMaxCallStackSizeToCapture",
    "Network.setBypassServiceWorker",
    "Network.setCacheDisabled",
    "Network.setAttachDebugStack",
    "Network.setUserAgentOverride",
    "Network.setAcceptedEncodings",
    "Network.clearAcceptedEncodingsOverride",
    "Network.addInterception",
    "Network.removeInterception",
    "Network.continueInterceptedRequest",
    "Log.startViolationsReport",
    "Log.stopViolationsReport",
    "DOM.setInspectedNode",
    "DOM.discardSearchResults",
    "Performance.setTime",
    "Performance.setTimezone",
];

impl CdpServer {
    /// Handles a message received from a CDP client on the given connection.
    pub(super) fn handle_client_message(&mut self, connection_id: u64, message: &Value) {
        let id = message.get("id").and_then(Value::as_i64).map(Value::from);
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let params = message
            .get("params")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let session_id = message
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned);

        // The legacy (non-flat) way of addressing session messages: the
        // session-scoped message is carried as a JSON string inside
        // `Target.sendMessageToTarget`.
        if method == "Target.sendMessageToTarget" {
            let inner_message = params
                .get("message")
                .and_then(Value::as_str)
                .and_then(|text| serde_json::from_str::<Value>(text).ok());
            match (id, inner_message) {
                (_, Some(inner)) => self.handle_client_message(connection_id, &inner),
                (Some(id), None) => self.send_error_to_connection(
                    connection_id,
                    id,
                    CdpError::invalid_params("invalid 'message' parameter"),
                ),
                (None, None) => {},
            }
            return;
        }

        match session_id {
            Some(session_id) => {
                self.handle_session_message(connection_id, id, &method, &params, &session_id)
            },
            None => self.handle_browser_message(connection_id, id, &method, &params),
        }
        self.flush();
    }

    /// Sends a reply to a session-scoped command. The reply echoes the
    /// `sessionId`, as required by the flat protocol.
    fn send_session_reply(
        &mut self,
        connection_id: u64,
        id: Option<Value>,
        session_id: &str,
        result: Value,
    ) {
        let Some(id) = id else { return };
        let mut reply = json!({ "id": id, "result": result });
        reply["sessionId"] = json!(session_id);
        self.send_to_connection(connection_id, reply);
    }

    /// Sends an error reply to a session-scoped command.
    fn send_session_error(
        &mut self,
        connection_id: u64,
        id: Option<Value>,
        session_id: &str,
        error: CdpError,
    ) {
        let Some(id) = id else { return };
        let mut reply = json!({ "id": id, "error": error.to_json() });
        reply["sessionId"] = json!(session_id);
        self.send_to_connection(connection_id, reply);
    }

    /// Handles a browser-level command (one without a `sessionId`).
    fn handle_browser_message(
        &mut self,
        connection_id: u64,
        id: Option<Value>,
        method: &str,
        params: &Value,
    ) {
        match method {
            "Browser.getVersion" => {
                let Some(id) = id else { return };
                let user_agent = pref!(user_agent);
                // Report a Chromium product and a plausible V8 version so
                // that clients do not flag the browser as unsupported. The
                // version comes from the `Chrome/` token of the user-agent.
                let chrome_version = chrome_major_version(&user_agent);
                self.send_reply_to_connection(
                    connection_id,
                    id,
                    json!({
                        "protocolVersion": "1.3",
                        "product": format!("Chrome/{chrome_version}.0.0.0"),
                        "revision": format!("@{chrome_version}"),
                        "userAgent": user_agent,
                        "jsVersion": format!("{chrome_version}.0.0.0"),
                    }),
                );
            },
            "Browser.close" | "Browser.crash" => {
                // Ignored: the embedder decides when the browser exits, but
                // the reply is still sent so that clients do not hang.
                if let Some(id) = id {
                    self.send_reply_to_connection(connection_id, id, json!({}));
                }
            },
            "Browser.getWindowForTarget" => {
                let Some(id) = id else { return };
                self.send_reply_to_connection(
                    connection_id,
                    id,
                    json!({
                        "windowId": 1,
                        "bounds": {
                            "left": 0,
                            "top": 0,
                            "width": 0,
                            "height": 0,
                            "windowState": "normal",
                        },
                    }),
                );
            },
            "Browser.getProcessMetrics" => {
                let Some(id) = id else { return };
                self.send_reply_to_connection(
                    connection_id,
                    id,
                    json!({
                        "processMetrics": [],
                    }),
                );
            },
            "Target.getTargets" => {
                let Some(id) = id else { return };
                let target_ids: Vec<_> = self.targets.keys().copied().collect();
                let target_infos: Vec<Value> = target_ids
                    .iter()
                    .map(|target_id| self.target_info_json(*target_id))
                    .collect();
                self.send_reply_to_connection(
                    connection_id,
                    id,
                    json!({ "targetInfos": target_infos }),
                );
            },
            "Target.getTargetInfo" => {
                let Some(id) = id else { return };
                let target_info = params
                    .get("targetId")
                    .and_then(Value::as_str)
                    .and_then(|target_id| self.target_ids.get(target_id))
                    .map(|target_id| self.target_info_json(*target_id));
                self.send_reply_to_connection(
                    connection_id,
                    id,
                    json!({ "targetInfo": target_info }),
                );
            },
            "Target.setDiscoverTargets" => {
                let Some(id) = id else { return };
                let discover = params
                    .get("discover")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let target_ids: Vec<_> = self.targets.keys().copied().collect();
                if let Some(connection) = self.connections.get_mut(&connection_id) {
                    connection.target_discovery = discover;
                }
                for target_id in target_ids {
                    let target_info = self.target_info_json(target_id);
                    let method = if discover {
                        "Target.targetCreated"
                    } else {
                        "Target.targetRemoved"
                    };
                    self.send_to_connection(
                        connection_id,
                        event(method, json!({ "targetInfo": target_info })),
                    );
                }
                self.send_reply_to_connection(connection_id, id, json!({}));
            },
            "Target.attachToTarget" => {
                let Some(id) = id else { return };
                let flatten = params
                    .get("flatten")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if !flatten {
                    self.send_error_to_connection(
                        connection_id,
                        id,
                        CdpError::invalid_params("only the flat session mode is supported"),
                    );
                    return;
                }
                let Some(target_id) = params
                    .get("targetId")
                    .and_then(Value::as_str)
                    .and_then(|target_id| self.target_ids.get(target_id).copied())
                else {
                    self.send_error_to_connection(
                        connection_id,
                        id,
                        CdpError::server("No target with given id found"),
                    );
                    return;
                };
                self.attach_session(connection_id, target_id);
                // Reply with the newly created session id, which is the most
                // recent session created for this connection.
                let session_id = self.last_session_id_for_connection(connection_id);
                self.send_reply_to_connection(
                    connection_id,
                    id,
                    json!({ "sessionId": session_id }),
                );
            },
            "Target.setAutoAttach" => {
                let Some(id) = id else { return };
                let auto_attach = params
                    .get("autoAttach")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if let Some(connection) = self.connections.get_mut(&connection_id) {
                    connection.auto_attach = auto_attach;
                }
                if auto_attach {
                    let target_ids: Vec<_> = self.targets.keys().copied().collect();
                    for target_id in target_ids {
                        self.attach_session(connection_id, target_id);
                    }
                }
                self.send_reply_to_connection(connection_id, id, json!({}));
            },
            "Target.detachFromTarget" => {
                let Some(id) = id else { return };
                if let Some(session_id) = params.get("sessionId").and_then(Value::as_str) {
                    self.sessions.remove(session_id);
                }
                self.send_reply_to_connection(connection_id, id, json!({}));
            },
            "Target.createTarget" => {
                let Some(id) = id else { return };
                let url = params
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or("about:blank");
                let Ok(url) = ServoUrl::parse(url) else {
                    self.send_error_to_connection(
                        connection_id,
                        id,
                        CdpError::invalid_params("invalid 'url' parameter"),
                    );
                    return;
                };
                // Open a new tab through the embedder. The reply carries the
                // id of the new webview; the same id is used as the CDP
                // target id once the browsing context shows up.
                let Ok(Some((webview_id_sender, webview_id_receiver))) =
                    generic_channel::oneshot::<servo_base::id::WebViewId>()
                else {
                    self.send_error_to_connection(
                        connection_id,
                        id,
                        CdpError::server("Could not create a channel for the new target"),
                    );
                    return;
                };
                self.embedder
                    .send(embedder_traits::EmbedderMsg::WebDriverCommand(
                        WebDriverCommandMsg::NewWindow(
                            embedder_traits::NewWindowTypeHint::Tab,
                            webview_id_sender,
                            None,
                        ),
                    ));
                let Ok(webview_id) = webview_id_receiver.recv() else {
                    self.send_error_to_connection(
                        connection_id,
                        id,
                        CdpError::server("The embedder refused to create a new target"),
                    );
                    return;
                };
                let target_id_string = format!("page-{}", self.next_target_number);
                self.next_target_number += 1;
                self.pending_targets.insert(webview_id, target_id_string.clone());
                self.send_reply_to_connection(
                    connection_id,
                    id,
                    json!({ "targetId": target_id_string }),
                );
            },
            "Target.closeTarget" => {
                let Some(id) = id else { return };
                let Some(webview_id) = params
                    .get("targetId")
                    .and_then(Value::as_str)
                    .and_then(|target_id| {
                        self.targets
                            .values()
                            .find(|target| target.target_id_string == target_id)
                            .map(|target| target.webview_id)
                    })
                    .or_else(|| {
                        // The target may still be pending creation.
                        params
                            .get("targetId")
                            .and_then(Value::as_str)
                            .and_then(|target_id| {
                                self.pending_targets
                                    .iter()
                                    .find(|(_, pending_id)| *pending_id == target_id)
                                    .map(|(webview_id, _)| *webview_id)
                            })
                    })
                else {
                    self.send_error_to_connection(
                        connection_id,
                        id,
                        CdpError::server("No target with given id found"),
                    );
                    return;
                };
                let Ok(Some((close_ack_sender, close_ack_receiver))) =
                    generic_channel::oneshot::<()>()
                else {
                    self.send_error_to_connection(
                        connection_id,
                        id,
                        CdpError::server("Could not create a channel to close the target"),
                    );
                    return;
                };
                self.embedder
                    .send(embedder_traits::EmbedderMsg::WebDriverCommand(
                        WebDriverCommandMsg::CloseWebView(webview_id, close_ack_sender),
                    ));
                // The target is going away; drop any session attached to it
                // so that clients observe the destruction.
                if let Some(target_id_string) = params.get("targetId").and_then(Value::as_str) {
                    let sessions_to_remove: Vec<String> = self
                        .sessions
                        .iter()
                        .filter(|(_, session)| {
                            self.targets
                                .get(&session.target_id)
                                .is_some_and(|target| target.target_id_string == target_id_string)
                        })
                        .map(|(session_id, _)| session_id.clone())
                        .collect();
                    for session_id in sessions_to_remove {
                        self.sessions.remove(&session_id);
                    }
                }
                let _ = close_ack_receiver.recv();
                self.send_reply_to_connection(connection_id, id, json!({}));
            },
            "Target.activateTarget" => {
                let Some(id) = id else { return };
                let Some(webview_id) = params
                    .get("targetId")
                    .and_then(Value::as_str)
                    .and_then(|target_id| {
                        self.targets
                            .values()
                            .find(|target| target.target_id_string == target_id)
                            .map(|target| target.webview_id)
                    })
                else {
                    self.send_error_to_connection(
                        connection_id,
                        id,
                        CdpError::server("No target with given id found"),
                    );
                    return;
                };
                self.embedder
                    .send(embedder_traits::EmbedderMsg::WebDriverCommand(
                        WebDriverCommandMsg::FocusWebView(webview_id),
                    ));
                self.send_reply_to_connection(connection_id, id, json!({}));
            },
            "Target.exposeDevToolsProtocol" => {
                let Some(id) = id else { return };
                self.send_reply_to_connection(connection_id, id, json!({}));
            },
            method => {
                let Some(id) = id else { return };
                self.send_error_to_connection(
                    connection_id,
                    id,
                    CdpError::method_not_found(method),
                );
            },
        }
    }

    /// Returns the session id most recently created for a connection.
    fn last_session_id_for_connection(&self, connection_id: u64) -> String {
        self.sessions
            .iter()
            .filter(|(_, session)| session.connection_id == connection_id)
            .max_by_key(|(session_id, _)| session_id.parse::<u64>().unwrap_or(0))
            .map(|(session_id, _)| session_id.clone())
            .unwrap_or_default()
    }

    /// Handles a session-scoped command.
    fn handle_session_message(
        &mut self,
        connection_id: u64,
        id: Option<Value>,
        method: &str,
        params: &Value,
        session_id: &str,
    ) {
        let Some(session) = self.sessions.get(session_id) else {
            if let Some(id) = id {
                self.send_error_to_connection(
                    connection_id,
                    id,
                    CdpError::server("Session not found"),
                );
            }
            return;
        };
        if session.connection_id != connection_id {
            if let Some(id) = id {
                self.send_error_to_connection(
                    connection_id,
                    id,
                    CdpError::server("Session not found"),
                );
            }
            return;
        }
        let target_id = session.target_id;

        // The `Target` domain is valid inside sessions as well.
        if method.starts_with("Target.") {
            self.handle_browser_message(connection_id, id, method, params);
            return;
        }

        match method {
            "Page.enable" => {
                if let Some(session) = self.sessions.get_mut(session_id) {
                    session.page_enabled = true;
                }
                self.send_session_reply(connection_id, id, session_id, json!({}));
                self.emit_page_state(session_id, connection_id);
            },
            "Page.disable" => {
                if let Some(session) = self.sessions.get_mut(session_id) {
                    session.page_enabled = false;
                }
                self.send_session_reply(connection_id, id, session_id, json!({}));
            },
            "Page.navigate" => self.page_navigate(connection_id, id, session_id, params),
            "Page.reload" => {
                match self.send_to_script_of_target(target_id, |pipeline_id| {
                    DevtoolScriptControlMsg::Reload(pipeline_id)
                }) {
                    Ok(()) => self.send_session_reply(connection_id, id, session_id, json!({})),
                    Err(error) => {
                        self.send_session_error(connection_id, id, session_id, error)
                    },
                }
            },
            "Page.goBack" => {
                match self.send_to_script_of_target(target_id, DevtoolScriptControlMsg::GoBack) {
                    Ok(()) => self.send_session_reply(connection_id, id, session_id, json!({})),
                    Err(error) => {
                        self.send_session_error(connection_id, id, session_id, error)
                    },
                }
            },
            "Page.goForward" => {
                match self.send_to_script_of_target(target_id, DevtoolScriptControlMsg::GoForward)
                {
                    Ok(()) => self.send_session_reply(connection_id, id, session_id, json!({})),
                    Err(error) => {
                        self.send_session_error(connection_id, id, session_id, error)
                    },
                }
            },
            "Page.getNavigationHistory" => {
                let (url, title) = self
                    .targets
                    .get(&target_id)
                    .map(|target| (target.url.clone(), target.title.clone()))
                    .unwrap_or_default();
                self.send_session_reply(
                    connection_id,
                    id,
                    session_id,
                    json!({
                        "index": 0,
                        "entries": [
                            {
                                "id": 1,
                                "url": url,
                                "title": title,
                                "userTypedURL": url,
                            },
                        ],
                    }),
                );
            },
            "Page.getFrameTree" => {
                let Some(target) = self.targets.get(&target_id) else {
                    self.send_session_error(
                        connection_id,
                        id,
                        session_id,
                        CdpError::server("Target not found"),
                    );
                    return;
                };
                self.send_session_reply(
                    connection_id,
                    id,
                    session_id,
                    json!({
                        "frameTree": {
                            "frame": {
                                "id": target.target_id_string,
                                "loaderId": "",
                                "url": target.url,
                                "mimeType": "text/html",
                                "securityOrigin": origin_of(&target.url),
                            },
                            "childFrames": [],
                        },
                    }),
                );
            },
            "Page.getResourceTree" => {
                let Some(target) = self.targets.get(&target_id) else {
                    self.send_session_error(
                        connection_id,
                        id,
                        session_id,
                        CdpError::server("Target not found"),
                    );
                    return;
                };
                self.send_session_reply(
                    connection_id,
                    id,
                    session_id,
                    json!({
                        "frameTree": {
                            "frame": {
                                "id": target.target_id_string,
                                "loaderId": "",
                                "url": target.url,
                                "mimeType": "text/html",
                                "securityOrigin": origin_of(&target.url),
                            },
                            "resources": [],
                        },
                    }),
                );
            },
            "Page.bringToFront" => {
                let Some(webview_id) = self
                    .targets
                    .get(&target_id)
                    .map(|target| target.webview_id)
                else {
                    self.send_session_error(
                        connection_id,
                        id,
                        session_id,
                        CdpError::server("Target not found"),
                    );
                    return;
                };
                self.embedder
                    .send(embedder_traits::EmbedderMsg::WebDriverCommand(
                        WebDriverCommandMsg::FocusWebView(webview_id),
                    ));
                self.send_session_reply(connection_id, id, session_id, json!({}));
            },
            "Page.captureScreenshot" => {
                self.page_capture_screenshot(connection_id, id, session_id, params);
            },
            "Page.printToPDF" | "Page.startScreencast" | "Page.captureSnapshot" => {
                self.send_session_error(
                    connection_id,
                    id,
                    session_id,
                    CdpError::server(format!("{method} is not supported by Servo")),
                );
            },
            "Runtime.enable" => {
                if let Some(session) = self.sessions.get_mut(session_id) {
                    session.runtime_enabled = true;
                }
                self.send_session_reply(connection_id, id, session_id, json!({}));
                self.emit_execution_contexts(session_id, connection_id);
            },
            "Runtime.disable" => {
                if let Some(session) = self.sessions.get_mut(session_id) {
                    session.runtime_enabled = false;
                }
                self.send_session_reply(connection_id, id, session_id, json!({}));
            },
            "Runtime.evaluate" => {
                self.runtime_evaluate(connection_id, id, session_id, params);
            },
            "Runtime.runScript" => {
                // Script persistence is not supported; run the source right
                // away like `Runtime.evaluate` would.
                let mut params = params.clone();
                if params.get("source").and_then(Value::as_str).is_some() {
                    let source = params["source"].clone();
                    params["expression"] = source;
                }
                self.runtime_evaluate(connection_id, id, session_id, &params);
            },
            "Runtime.compileScript" => {
                // Scripts are not precompiled; hand out an unusable but
                // well-formed id so that clients can proceed.
                self.send_session_reply(connection_id, id, session_id, json!({ "scriptId": "0" }));
            },
            "Runtime.callFunctionOn" => {
                self.runtime_call_function_on(connection_id, id, session_id, params);
            },
            "Network.enable" => {
                if let Some(session) = self.sessions.get_mut(session_id) {
                    session.network_enabled = true;
                }
                self.send_session_reply(connection_id, id, session_id, json!({}));
            },
            "Network.disable" => {
                if let Some(session) = self.sessions.get_mut(session_id) {
                    session.network_enabled = false;
                }
                self.send_session_reply(connection_id, id, session_id, json!({}));
            },
            "Network.getResponseBody" => {
                self.network_get_response_body(connection_id, id, session_id, params);
            },
            "Network.getCookies" | "Storage.getCookies" => {
                self.send_session_reply(connection_id, id, session_id, json!({ "cookies": [] }));
            },
            "Network.setRequestInterception" => {
                self.send_session_error(
                    connection_id,
                    id,
                    session_id,
                    CdpError::server(format!("{method} is not supported by Servo")),
                );
            },
            "Log.enable" => {
                if let Some(session) = self.sessions.get_mut(session_id) {
                    session.log_enabled = true;
                }
                self.send_session_reply(connection_id, id, session_id, json!({}));
            },
            "Log.disable" | "Log.clear" => {
                self.send_session_reply(connection_id, id, session_id, json!({}));
            },
            "Console.enable" => {
                if let Some(session) = self.sessions.get_mut(session_id) {
                    session.console_enabled = true;
                }
                self.send_session_reply(connection_id, id, session_id, json!({}));
            },
            "Console.disable" | "Console.clearMessages" => {
                self.send_session_reply(connection_id, id, session_id, json!({}));
            },
            "DOM.enable" => {
                if let Some(session) = self.sessions.get_mut(session_id) {
                    session.dom_enabled = true;
                }
                self.send_session_reply(connection_id, id, session_id, json!({}));
            },
            "DOM.disable" => {
                if let Some(session) = self.sessions.get_mut(session_id) {
                    session.dom_enabled = false;
                }
                self.send_session_reply(connection_id, id, session_id, json!({}));
            },
            "DOM.getDocument" => {
                self.dom_get_document(connection_id, id, session_id, params);
            },
            "DOM.requestChildNodes" => {
                self.dom_request_child_nodes(connection_id, id, session_id, params);
            },
            "DOM.getBoxModel" => {
                self.dom_get_box_model(connection_id, id, session_id, params);
            },
            "DOM.resolveNode" | "DOM.getNodeForLocation" => {
                self.send_session_error(
                    connection_id,
                    id,
                    session_id,
                    CdpError::server(format!("{method} is not supported by Servo")),
                );
            },
            method if EMULATION_NOOP_METHODS.contains(&method) => {
                self.send_session_reply(connection_id, id, session_id, json!({}));
            },
            method if SESSION_NOOP_METHODS.contains(&method) => {
                warn!("Accepted automation method as a no-op: {method}");
                self.send_session_reply(connection_id, id, session_id, json!({}));
            },
            "Console.getMessages" => {
                self.send_session_reply(connection_id, id, session_id, json!({ "messages": [] }));
            },
            "Performance.enable" => {
                self.send_session_reply(connection_id, id, session_id, json!({}));
            },
            "Performance.getMetrics" => {
                self.send_session_reply(
                    connection_id,
                    id,
                    session_id,
                    json!({
                        "metrics": [
                            { "name": "Timestamp", "value": self.timestamp_now() },
                        ],
                    }),
                );
            },
            "Schema.getDomains" => {
                self.send_session_reply(
                    connection_id,
                    id,
                    session_id,
                    json!({ "domains": [] }),
                );
            },
            method => {
                self.send_session_error(
                    connection_id,
                    id,
                    session_id,
                    CdpError::method_not_found(method),
                );
            },
        }
    }

    /// Sends a message to the script thread of the current pipeline of a
    /// target. The message is built from the pipeline id.
    fn send_to_script_of_target<F>(
        &mut self,
        target_id: servo_base::id::BrowsingContextId,
        message: F,
    ) -> Result<(), CdpError>
    where
        F: FnOnce(PipelineId) -> DevtoolScriptControlMsg,
    {
        let Some(target) = self.targets.get(&target_id) else {
            return Err(CdpError::server("Target not found"));
        };
        let Some(pipeline_id) = target.current_pipeline else {
            return Err(CdpError::server("Target has no execution context"));
        };
        let Some(script_sender) = self
            .pipelines
            .get(&pipeline_id)
            .and_then(|pipeline| pipeline.script_sender.clone())
        else {
            return Err(CdpError::server("Target is not ready for evaluation"));
        };
        script_sender
            .send(message(pipeline_id))
            .map_err(|_| CdpError::server("Target is gone"))
    }

    /// Emits the current `Page` state (frame + loading flag) of the target of
    /// a session right after `Page.enable`, like Chromium does.
    fn emit_page_state(&mut self, session_id: &str, connection_id: u64) {
        let Some(session) = self.sessions.get(session_id) else {
            return;
        };
        let target_id = session.target_id;
        let Some(target) = self.targets.get(&target_id) else {
            return;
        };
        // Copy out everything needed from `target` so that the borrow of
        // `self` ends before messages are queued.
        let frame_id = target.target_id_string.clone();
        let url = target.url.clone();
        let loading = target.loading;

        let frame_navigated = event(
            "Page.frameNavigated",
            json!({
                "frame": {
                    "id": frame_id,
                    "loaderId": "",
                    "url": url,
                    "domainAndRegistry": "",
                    "securityOrigin": "about:blank",
                    "mimeType": "text/html",
                },
                "type": "Navigation",
            }),
        );
        let mut frame_navigated = frame_navigated;
        frame_navigated["sessionId"] = json!(session_id);
        self.send_to_connection(connection_id, frame_navigated);

        if loading {
            let frame_started = event(
                "Page.frameStartedLoading",
                json!({ "frameId": frame_id }),
            );
            let mut frame_started = frame_started;
            frame_started["sessionId"] = json!(session_id);
            self.send_to_connection(connection_id, frame_started);
        }
    }

    /// Emits `Runtime.executionContextCreated` for every known pipeline of
    /// the target of a session right after `Runtime.enable`.
    fn emit_execution_contexts(&mut self, session_id: &str, connection_id: u64) {
        let Some(session) = self.sessions.get(session_id) else {
            return;
        };
        let target_id = session.target_id;
        let pipelines: Vec<(PipelineId, i64, String)> = self
            .pipelines
            .iter()
            .filter(|(_, pipeline)| pipeline.browsing_context_id == target_id)
            .map(|(pipeline_id, pipeline)| {
                (pipeline_id, pipeline.execution_context_id, pipeline.url.clone())
            })
            .collect();

        for (pipeline_id, execution_context_id, url) in pipelines {
            let created = event(
                "Runtime.executionContextCreated",
                json!({
                    "context": {
                        "id": execution_context_id,
                        "origin": origin_of(&url),
                        "name": "",
                        "uniqueId": format!("{pipeline_id:?}"),
                        "isDefault": true,
                        "auxData": {
                            "isDefault": true,
                            "type": "default",
                        },
                    },
                }),
            );
            let mut created = created;
            created["sessionId"] = json!(session_id);
            self.send_to_connection(connection_id, created);
        }
    }

    /// Implements `Page.navigate`.
    fn page_navigate(
        &mut self,
        connection_id: u64,
        id: Option<Value>,
        session_id: &str,
        params: &Value,
    ) {
        let Some(target) = self.sessions.get(session_id).map(|session| session.target_id) else {
            return;
        };
        let Some(url_string) = params.get("url").and_then(Value::as_str) else {
            self.send_session_error(
                connection_id,
                id,
                session_id,
                CdpError::invalid_params("'url' parameter is required"),
            );
            return;
        };
        let frame_id = self
            .targets
            .get(&target_id)
            .map(|target| target.target_id_string.clone())
            .unwrap_or_default();

        let result = match ServoUrl::parse(url_string) {
            Ok(url) => {
                match self.send_to_script_of_target(target_id, move |pipeline_id| {
                    DevtoolScriptControlMsg::NavigateTo(pipeline_id, url)
                }) {
                    Ok(()) => json!({ "frameId": frame_id, "loaderId": "" }),
                    Err(error) => json!({ "frameId": frame_id, "errorText": error.message }),
                }
            },
            Err(_) => json!({ "frameId": frame_id, "errorText": "ERR_INVALID_URL" }),
        };
        self.send_session_reply(connection_id, id, session_id, result);
    }

    /// Implements `Runtime.evaluate` by delegating to the script thread,
    /// blocking until it replies.
    fn runtime_evaluate(
        &mut self,
        connection_id: u64,
        id: Option<Value>,
        session_id: &str,
        params: &Value,
    ) {
        let Some(expression) = params.get("expression").and_then(Value::as_str).map(str::to_owned)
        else {
            self.send_session_error(
                connection_id,
                id,
                session_id,
                CdpError::invalid_params("'expression' parameter is required"),
            );
            return;
        };

        let Some(target_id) = self.sessions.get(session_id).map(|session| session.target_id)
        else {
            return;
        };
        let Some(pipeline_id) = self
            .targets
            .get(&target_id)
            .and_then(|target| target.current_pipeline)
        else {
            self.send_session_error(
                connection_id,
                id,
                session_id,
                CdpError::server("Target has no execution context"),
            );
            return;
        };
        self.runtime_evaluate_in_pipeline(
            connection_id,
            id,
            session_id,
            pipeline_id,
            &expression,
        );
    }

    /// Evaluates an expression in the given pipeline, blocking until the
    /// script thread replies.
    fn runtime_evaluate_in_pipeline(
        &mut self,
        connection_id: u64,
        id: Option<Value>,
        session_id: &str,
        pipeline_id: PipelineId,
        expression: &str,
    ) {
        let Some(script_sender) = self
            .pipelines
            .get(&pipeline_id)
            .and_then(|pipeline| pipeline.script_sender.clone())
        else {
            self.send_session_error(
                connection_id,
                id,
                session_id,
                CdpError::server("Target is not ready for evaluation"),
            );
            return;
        };

        let expression = expression.to_owned();
        let Some((channel, port)) = generic_channel::channel::<EvaluateJSReply>() else {
            self.send_session_error(
                connection_id,
                id,
                session_id,
                CdpError::server("Could not create evaluation channel"),
            );
            return;
        };
        if script_sender
            .send(DevtoolScriptControlMsg::Eval(
                expression,
                pipeline_id,
                None,
                false,
                channel,
            ))
            .is_err()
        {
            self.send_session_error(
                connection_id,
                id,
                session_id,
                CdpError::server("Target is gone"),
            );
            return;
        }

        match port.try_recv_timeout(EVALUATE_TIMEOUT) {
            Ok(reply) => {
                let (result, exception_details) = evaluate_reply_to_remote_object(
                    &reply.value,
                    reply.has_exception,
                    reply.exception_message.as_deref(),
                );
                let mut result_json = json!({ "result": result });
                if let Some(exception_details) = exception_details {
                    result_json["exceptionDetails"] = exception_details;
                }
                self.send_session_reply(connection_id, id, session_id, result_json);
            },
            Err(_) => {
                self.send_session_error(
                    connection_id,
                    id,
                    session_id,
                    CdpError::server("Evaluation timed out"),
                );
            },
        }
    }

    /// Implements `DOM.getDocument` by walking the tree from the script
    /// thread up to `depth` levels deep.
    fn dom_get_document(
        &mut self,
        connection_id: u64,
        id: Option<Value>,
        session_id: &str,
        params: &Value,
    ) {
        let Some(target_id) = self.sessions.get(session_id).map(|session| session.target_id)
        else {
            return;
        };
        let Some(target) = self.targets.get(&target_id) else {
            self.send_session_error(connection_id, id, session_id, CdpError::server("Target not found"));
            return;
        };
        let Some(pipeline_id) = target.current_pipeline else {
            self.send_session_error(
                connection_id,
                id,
                session_id,
                CdpError::server("Target has no execution context"),
            );
            return;
        };
        let Some(script_sender) = self
            .pipelines
            .get(&pipeline_id)
            .and_then(|pipeline| pipeline.script_sender.clone())
        else {
            self.send_session_error(
                connection_id,
                id,
                session_id,
                CdpError::server("Target is not ready"),
            );
            return;
        };

        let Some((channel, port)) = generic_channel::channel::<Option<NodeInfo>>() else {
            self.send_session_error(
                connection_id,
                id,
                session_id,
                CdpError::server("Could not create DOM channel"),
            );
            return;
        };
        if script_sender
            .send(DevtoolScriptControlMsg::GetRootNode(pipeline_id, channel))
            .is_err()
        {
            self.send_session_error(
                connection_id,
                id,
                session_id,
                CdpError::server("Target is gone"),
            );
            return;
        }

        match port.try_recv_timeout(EVALUATE_TIMEOUT) {
            Ok(Some(root_node)) => {
                let depth = params.get("depth").and_then(Value::as_i64).unwrap_or(1);
                let root = self.dom_node_json(session_id, connection_id, pipeline_id, &root_node, depth as i32);
                self.send_session_reply(
                    connection_id,
                    id,
                    session_id,
                    json!({ "root": root }),
                );
            },
            Ok(None) => {
                self.send_session_error(
                    connection_id,
                    id,
                    session_id,
                    CdpError::server("Could not read the document"),
                );
            },
            Err(_) => {
                self.send_session_error(
                    connection_id,
                    id,
                    session_id,
                    CdpError::server("Reading the document timed out"),
                );
            },
        }
    }

    /// Recursively serializes a node and (up to `depth`) its children.
    fn dom_node_json(
        &mut self,
        session_id: &str,
        connection_id: u64,
        pipeline_id: PipelineId,
        node: &NodeInfo,
        depth: i32,
    ) -> Value {
        let node_id = self.next_node_id;
        self.next_node_id += 1;
        self.dom_nodes.insert(
            node_id,
            DomNodeRef {
                pipeline_id,
                unique_id: node.unique_id.clone(),
            },
        );

        let attributes: Vec<String> = node
            .attrs
            .iter()
            .flat_map(|attribute| [attribute.name.clone(), attribute.value.clone()])
            .collect();

        let mut json_node = json!({
            "nodeId": node_id,
            "backendNodeId": node_id,
            "nodeType": node.node_type,
            "nodeName": node.node_name,
            "nodeValue": node.node_value.clone().unwrap_or_default(),
            "childNodeCount": node.num_children,
            "attributes": attributes,
            "isDisplayed": node.is_displayed,
        });

        if depth > 0 && node.num_children > 0 {
            let children = self.dom_children_json(
                session_id,
                connection_id,
                pipeline_id,
                node.unique_id.clone(),
                depth,
            );
            if !children.is_empty() {
                json_node["children"] = json!(children);
            }
        }
        json_node
    }

    /// Fetches the children of a node (by its unique id) from the script
    /// thread and serializes them.
    fn dom_children_json(
        &mut self,
        session_id: &str,
        connection_id: u64,
        pipeline_id: PipelineId,
        unique_id: String,
        depth: i32,
    ) -> Vec<Value> {
        let Some(script_sender) = self
            .pipelines
            .get(&pipeline_id)
            .and_then(|pipeline| pipeline.script_sender.clone())
        else {
            return Vec::new();
        };
        let Some((channel, port)) = generic_channel::channel::<Option<Vec<NodeInfo>>>() else {
            return Vec::new();
        };
        if script_sender
            .send(DevtoolScriptControlMsg::GetChildren(pipeline_id, unique_id, channel))
            .is_err()
        {
            return Vec::new();
        };
        let Ok(Some(children)) = port.try_recv_timeout(EVALUATE_TIMEOUT) else {
            return Vec::new();
        };
        children
            .iter()
            .map(|child| self.dom_node_json(session_id, connection_id, pipeline_id, child, depth - 1))
            .collect()
    }

    /// Implements `DOM.requestChildNodes` by emitting a `DOM.setChildNodes`
    /// event for the children of the given node.
    fn dom_request_child_nodes(
        &mut self,
        connection_id: u64,
        id: Option<Value>,
        session_id: &str,
        params: &Value,
    ) {
        let Some(node_id) = params.get("nodeId").and_then(Value::as_i64) else {
            self.send_session_error(
                connection_id,
                id,
                session_id,
                CdpError::invalid_params("'nodeId' parameter is required"),
            );
            return;
        };
        let Some(node_ref) = self.dom_nodes.get(&node_id) else {
            self.send_session_error(
                connection_id,
                id,
                session_id,
                CdpError::server("Could not find node with given id"),
            );
            return;
        };
        let pipeline_id = node_ref.pipeline_id;
        let unique_id = node_ref.unique_id.clone();
        let depth = params.get("depth").and_then(Value::as_i64).unwrap_or(1).max(1) as i32;

        let children = self.dom_children_json(session_id, connection_id, pipeline_id, unique_id, depth);
        let mut event = event("DOM.setChildNodes", json!({ "parentId": node_id, "nodes": children }));
        event["sessionId"] = json!(session_id);
        self.send_to_connection(connection_id, event);
        self.send_session_reply(connection_id, id, session_id, json!({}));
    }

    /// Implements `Page.captureScreenshot` by asking the embedder to take a
    /// screenshot of the target's webview (the same path WebDriver uses),
    /// then encoding the result as a base64 PNG.
    fn page_capture_screenshot(
        &mut self,
        connection_id: u64,
        id: Option<Value>,
        session_id: &str,
        params: &Value,
    ) {
        let Some(webview_id) = self
            .sessions
            .get(session_id)
            .map(|session| session.target_id)
            .and_then(|target_id| self.targets.get(&target_id))
            .map(|target| target.webview_id)
        else {
            self.send_session_error(
                connection_id,
                id,
                session_id,
                CdpError::server("Target not found"),
            );
            return;
        };

        // The optional `clip` parameter is a rect in CSS pixels.
        let rect = params.get("clip").and_then(|clip| {
            let (x, y) = (clip.get("x")?.as_f64()? as f32, clip.get("y")?.as_f64()? as f32);
            let width = clip.get("width")?.as_f64()? as f32;
            let height = clip.get("height")?.as_f64()? as f32;
            Some(Rect::new(
                euclid::Point2D::new(x, y),
                euclid::Size2D::new(width, height),
            ))
        });

        let (result_sender, result_receiver) = crossbeam_channel::unbounded();
        self.embedder
            .send(embedder_traits::EmbedderMsg::WebDriverCommand(
                WebDriverCommandMsg::TakeScreenshot(webview_id, rect, result_sender),
            ));

        let rgba_image = match result_receiver.recv_timeout(AUTOMATION_COMMAND_TIMEOUT) {
            Ok(Ok(rgba_image)) => rgba_image,
            Ok(Err(error)) => {
                self.send_session_error(
                    connection_id,
                    id,
                    session_id,
                    CdpError::server(format!("Could not take a screenshot: {error:?}")),
                );
                return;
            },
            Err(_) => {
                self.send_session_error(
                    connection_id,
                    id,
                    session_id,
                    CdpError::server("Taking a screenshot timed out"),
                );
                return;
            },
        };

        let mut png_data = Cursor::new(Vec::new());
        if let Err(error) =
            DynamicImage::ImageRgba8(rgba_image).write_to(&mut png_data, ImageFormat::Png)
        {
            self.send_session_error(
                connection_id,
                id,
                session_id,
                CdpError::server(format!("Could not encode the screenshot: {error}")),
            );
            return;
        }
        self.send_session_reply(
            connection_id,
            id,
            session_id,
            json!({ "data": BASE64.encode(png_data.get_ref()) }),
        );
    }

    /// Implements `Network.getResponseBody` from the response bodies that the
    /// fetch layer captured for devtools.
    fn network_get_response_body(
        &mut self,
        connection_id: u64,
        id: Option<Value>,
        session_id: &str,
        params: &Value,
    ) {
        let Some(request_id) = params.get("requestId").and_then(Value::as_str) else {
            self.send_session_error(
                connection_id,
                id,
                session_id,
                CdpError::invalid_params("'requestId' parameter is required"),
            );
            return;
        };
        let Some(request) = self.network_requests.get(request_id) else {
            self.send_session_error(
                connection_id,
                id,
                session_id,
                CdpError::server("Request not found"),
            );
            return;
        };
        let Some(body) = &request.body else {
            self.send_session_error(
                connection_id,
                id,
                session_id,
                CdpError::server("Response body is not available"),
            );
            return;
        };

        let is_text = request.mime_type.starts_with("text/") ||
            matches!(
                request.mime_type.split(';').next().unwrap_or_default(),
                "application/json" |
                    "application/javascript" |
                    "application/xml" |
                    "application/xhtml+xml" |
                    "image/svg+xml" |
                    "application/x-www-form-urlencoded"
            );
        let (body, base64_encoded) = if is_text {
            (String::from_utf8_lossy(body).into_owned(), false)
        } else {
            (BASE64.encode(body), true)
        };
        self.send_session_reply(
            connection_id,
            id,
            session_id,
            json!({ "body": body, "base64Encoded": base64_encoded }),
        );
    }

    /// Implements `Runtime.callFunctionOn` for functions called in an
    /// execution context: the function declaration is applied to the
    /// JSON-serialized arguments and evaluated in the context.
    fn runtime_call_function_on(
        &mut self,
        connection_id: u64,
        id: Option<Value>,
        session_id: &str,
        params: &Value,
    ) {
        let Some(function) = params
            .get("functionDeclaration")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            self.send_session_error(
                connection_id,
                id,
                session_id,
                CdpError::invalid_params("'functionDeclaration' parameter is required"),
            );
            return;
        };

        // Only the execution-context flavor is supported: remote objects
        // cannot be addressed because Servo does not keep them alive.
        let Some(execution_context_id) = params.get("executionContextId").and_then(Value::as_i64)
        else {
            self.send_session_error(
                connection_id,
                id,
                session_id,
                CdpError::server(
                    "Runtime.callFunctionOn only supports the 'executionContextId' flavor",
                ),
            );
            return;
        };
        let Some(pipeline_id) = self
            .pipelines
            .iter()
            .find(|(_, pipeline)| pipeline.execution_context_id == execution_context_id)
            .map(|(pipeline_id, _)| *pipeline_id)
        else {
            self.send_session_error(
                connection_id,
                id,
                session_id,
                CdpError::server("Cannot find execution context with given id"),
            );
            return;
        };

        // Serialize the arguments as JavaScript literals.
        let arguments: Vec<String> = params
            .get("arguments")
            .and_then(Value::as_array)
            .map(|arguments| {
                arguments
                    .iter()
                    .map(cdp_value_to_js_source)
                    .collect()
            })
            .unwrap_or_default();
        let expression = format!(
            "({function}).apply(undefined, [{arguments}])",
            arguments = arguments.join(", ")
        );
        self.runtime_evaluate_in_pipeline(
            connection_id,
            id,
            session_id,
            pipeline_id,
            &expression,
        );
    }

    /// Implements `DOM.getBoxModel` using the layout information from the
    /// script thread. The box is anchored at the origin because the layout
    /// information does not include the absolute position of the node.
    fn dom_get_box_model(
        &mut self,
        connection_id: u64,
        id: Option<Value>,
        session_id: &str,
        params: &Value,
    ) {
        let Some(node_id) = params.get("nodeId").and_then(Value::as_i64) else {
            self.send_session_error(
                connection_id,
                id,
                session_id,
                CdpError::invalid_params("'nodeId' parameter is required"),
            );
            return;
        };
        let Some(node_ref) = self.dom_nodes.get(&node_id) else {
            self.send_session_error(
                connection_id,
                id,
                session_id,
                CdpError::server("Could not find node with given id"),
            );
            return;
        };
        let pipeline_id = node_ref.pipeline_id;
        let unique_id = node_ref.unique_id.clone();
        let Some(script_sender) = self
            .pipelines
            .get(&pipeline_id)
            .and_then(|pipeline| pipeline.script_sender.clone())
        else {
            self.send_session_error(
                connection_id,
                id,
                session_id,
                CdpError::server("Target is not ready"),
            );
            return;
        };
        let Some((channel, port)) =
            generic_channel::channel::<Option<(devtools_traits::ComputedNodeLayout, devtools_traits::AutoMargins)>>()
        else {
            self.send_session_error(
                connection_id,
                id,
                session_id,
                CdpError::server("Could not create layout channel"),
            );
            return;
        };
        if script_sender
            .send(DevtoolScriptControlMsg::GetLayout(pipeline_id, unique_id, channel))
            .is_err()
        {
            self.send_session_error(
                connection_id,
                id,
                session_id,
                CdpError::server("Target is gone"),
            );
            return;
        }

        match port.try_recv_timeout(EVALUATE_TIMEOUT) {
            Ok(Some((layout, _auto_margins))) => {
                let (width, height) = (layout.width, layout.height);
                let quad = [0.0f32, 0.0, width, 0.0, width, height, 0.0, height];
                self.send_session_reply(
                    connection_id,
                    id,
                    session_id,
                    json!({
                        "model": {
                            "content": quad,
                            "padding": quad,
                            "border": quad,
                            "margin": quad,
                            "width": width,
                            "height": height,
                        },
                    }),
                );
            },
            Ok(None) => {
                self.send_session_error(
                    connection_id,
                    id,
                    session_id,
                    CdpError::server("Could not compute the box model of the node"),
                );
            },
            Err(_) => {
                self.send_session_error(
                    connection_id,
                    id,
                    session_id,
                    CdpError::server("Computing the box model timed out"),
                );
            },
        }
    }
}

/// Serializes a CDP call-argument into a JavaScript literal that can be
/// spliced into an evaluated expression.
fn cdp_value_to_js_source(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(boolean) => boolean.to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(string) => serde_json::to_string(string).unwrap_or_default(),
        Value::Array(_) | Value::Object(_) => {
            serde_json::to_string(value).unwrap_or_default()
        },
    }
}
