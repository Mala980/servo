/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Dispatching of client-to-browser CDP messages ("commands") for the
//! [`CdpServer`]. Commands are addressed either to the browser (no session)
//! or to a session created with `Target.attachToTarget`.

use devtools_traits::{DevtoolScriptControlMsg, EvaluateJSReply, NodeInfo};
use serde_json::{Value, json};
use servo_base::generic_channel;
use servo_base::id::PipelineId;
use servo_config::pref;
use servo_url::ServoUrl;

use super::origin_of;
use crate::cdp::protocol::{CdpError, event, evaluate_reply_to_remote_object};
use crate::cdp::{CdpServer, DomNodeRef, EVALUATE_TIMEOUT};

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
                self.send_reply_to_connection(
                    connection_id,
                    id,
                    json!({
                        "protocolVersion": "1.3",
                        "product": format!("Servo/{}", env!("CARGO_PKG_VERSION")),
                        "revision": "@unknown",
                        "userAgent": user_agent,
                        "jsVersion": "0.0.0",
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
            "Target.createTarget" | "Target.closeTarget" | "Target.exposeDevToolsProtocol" => {
                let Some(id) = id else { return };
                self.send_error_to_connection(
                    connection_id,
                    id,
                    CdpError::server(format!("{method} is not supported by Servo")),
                );
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
            "Page.captureScreenshot" | "Page.printToPDF" | "Page.startScreencast" => {
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
            "Runtime.discardConsoleEntries" => {
                self.send_session_reply(connection_id, id, session_id, json!({}));
            },
            "Runtime.compileScript" => {
                self.send_session_reply(connection_id, id, session_id, json!([]));
            },
            "Runtime.callFunctionOn" => {
                self.send_session_error(
                    connection_id,
                    id,
                    session_id,
                    CdpError::server("Runtime.callFunctionOn is not supported by Servo"),
                );
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
            "Network.getResponseBody" | "Network.getCookies" | "Network.setRequestInterception" => {
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
            "DOM.getBoxModel" | "DOM.resolveNode" | "DOM.getNodeForLocation" => {
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
                CdpError::server("Target is not ready for evaluation"),
            );
            return;
        };

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
}
