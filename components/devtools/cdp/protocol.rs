/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Helpers for building [Chrome DevTools Protocol](https://chromedevtools.github.io/devtools-protocol/)
//! messages: converting Servo-internal values to their CDP JSON counterparts
//! and for constructing events.

use devtools_traits::{ConsoleLogLevel, DebuggerValue, ObjectPreview};
use http::HeaderMap;
use serde_json::{Value, json};

/// The standard JSON-RPC error codes used by the CDP.
pub(crate) const ERROR_METHOD_NOT_FOUND: i64 = -32601;
pub(crate) const ERROR_INVALID_REQUEST: i64 = -32600;
pub(crate) const ERROR_SERVER_ERROR: i64 = -32000;

/// A method invocation error that is reported back to the CDP client.
pub(crate) struct CdpError {
    pub code: i64,
    pub message: String,
}

impl CdpError {
    pub(crate) fn method_not_found(method: &str) -> Self {
        Self {
            code: ERROR_METHOD_NOT_FOUND,
            message: format!("'{method}' wasn't found"),
        }
    }

    pub(crate) fn server(message: impl Into<String>) -> Self {
        Self {
            code: ERROR_SERVER_ERROR,
            message: message.into(),
        }
    }

    pub(crate) fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: ERROR_INVALID_REQUEST,
            message: message.into(),
        }
    }

    /// Serializes this error into the `error` member of a CDP reply.
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "code": self.code,
            "message": self.message,
        })
    }
}

/// Builds a CDP event message, i.e. a message without an `id`.
pub(crate) fn event(method: &str, params: Value) -> Value {
    json!({
        "method": method,
        "params": params,
    })
}

/// Converts a [`DebuggerValue`] into a CDP `RemoteObject`. Mirrors the way
/// Chromium serializes evaluation results, including its quirks for
/// non-finite numbers (which are sent as strings).
pub(crate) fn debugger_value_to_remote_object(value: &DebuggerValue) -> Value {
    match value {
        DebuggerValue::VoidValue => json!({ "type": "undefined" }),
        DebuggerValue::NullValue(_) => json!({
            "type": "object",
            "subtype": "null",
            "value": Value::Null,
        }),
        DebuggerValue::BooleanValue(boolean) => json!({ "type": "boolean", "value": boolean }),
        DebuggerValue::NumberValue(number) => {
            if number.is_finite() {
                json!({ "type": "number", "value": number })
            } else {
                // `Infinity` and `NaN` cannot be represented in JSON.
                let description = if number.is_nan() {
                    "NaN"
                } else if *number > 0.0 {
                    "Infinity"
                } else {
                    "-Infinity"
                };
                json!({ "type": "number", "value": description, "unserializableValue": description })
            }
        },
        DebuggerValue::StringValue(string) => {
            json!({ "type": "string", "value": string, "description": string })
        },
        DebuggerValue::ObjectValue {
            class,
            preview,
            own_property_length,
            ..
        } => {
            let mut object = json!({
                "type": "object",
                "className": class,
                "description": class,
            });
            if let Some(length) = own_property_length {
                object["ownPropertiesLength"] = json!(length);
            }
            if let Some(preview) = preview {
                object["preview"] = object_preview_to_json(preview);
            }
            object
        },
    }
}

/// Converts an [`ObjectPreview`] into the CDP `ObjectPreview` structure.
fn object_preview_to_json(preview: &ObjectPreview) -> Value {
    let properties: Vec<Value> = preview
        .own_properties
        .as_ref()
        .map(|properties| {
            properties
                .iter()
                .map(|property| {
                    json!({
                        "name": property.name,
                        "type": remote_object_type(&property.value),
                        "value": debugger_value_to_json_leaf(&property.value),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    json!({
        "type": preview.kind,
        "overflow": false,
        "properties": properties,
    })
}

/// A simplified [`DebuggerValue`] to JSON conversion used inside previews,
/// where nested objects are represented by their class name only.
fn debugger_value_to_json_leaf(value: &DebuggerValue) -> Value {
    match value {
        DebuggerValue::VoidValue | DebuggerValue::NullValue(_) => Value::Null,
        DebuggerValue::BooleanValue(boolean) => json!(boolean),
        DebuggerValue::NumberValue(number) => json!(number),
        DebuggerValue::StringValue(string) => json!(string),
        DebuggerValue::ObjectValue { class, .. } => json!(class),
    }
}

/// Returns the CDP remote object type name for a [`DebuggerValue`].
fn remote_object_type(value: &DebuggerValue) -> &'static str {
    match value {
        DebuggerValue::VoidValue => "undefined",
        DebuggerValue::NullValue(_) => "object",
        DebuggerValue::BooleanValue(_) => "boolean",
        DebuggerValue::NumberValue(_) => "number",
        DebuggerValue::StringValue(_) => "string",
        DebuggerValue::ObjectValue { .. } => "object",
    }
}

/// Converts a `Runtime.evaluate` reply into a CDP `RemoteObject` and an
/// optional `exceptionDetails` structure.
pub(crate) fn evaluate_reply_to_remote_object(
    value: &DebuggerValue,
    has_exception: bool,
    exception_message: Option<&str>,
) -> (Value, Option<Value>) {
    if has_exception {
        let description = exception_message.unwrap_or("Exception");
        let exception_details = json!({
            "exceptionId": 1,
            "text": "Uncaught",
            "lineNumber": 0,
            "columnNumber": 0,
            "exception": {
                "type": "object",
                "subtype": "error",
                "className": "Error",
                "description": description,
            },
        });
        (json!({ "type": "object", "subtype": "error" }), Some(exception_details))
    } else {
        (debugger_value_to_remote_object(value), None)
    }
}

/// Converts Servo console log levels to the CDP `Runtime.consoleAPICalled`
/// message type.
pub(crate) fn console_log_level_to_cdp_type(level: &ConsoleLogLevel) -> &'static str {
    match level {
        ConsoleLogLevel::Log => "log",
        ConsoleLogLevel::Debug => "debug",
        ConsoleLogLevel::Info => "info",
        ConsoleLogLevel::Warn => "warning",
        ConsoleLogLevel::Error => "error",
        ConsoleLogLevel::Trace => "trace",
        ConsoleLogLevel::Dir => "dir",
    }
}

/// Converts the same log level to the severity used by `Log.entryAdded`.
pub(crate) fn console_log_level_to_cdp_level(level: &ConsoleLogLevel) -> &'static str {
    match level {
        ConsoleLogLevel::Error => "error",
        ConsoleLogLevel::Warn => "warning",
        _ => "info",
    }
}

/// Serializes an HTTP [`HeaderMap`] as a JSON object with lower-cased header
/// names, the way the CDP `Network` domain represents headers.
pub(crate) fn headers_to_cdp_json(headers: Option<&HeaderMap>) -> Value {
    let mut map = serde_json::Map::new();
    if let Some(headers) = headers {
        for (name, value) in headers.iter() {
            let Ok(value) = value.to_str() else {
                continue;
            };
            let name = name.as_str().to_ascii_lowercase();
            match map.get_mut(&name) {
                // Repeated headers are joined with ", " like Chromium does.
                Some(Value::String(existing)) => {
                    let joined = format!("{existing}, {value}");
                    map.insert(name, Value::String(joined));
                },
                _ => {
                    map.insert(name, Value::String(value.to_owned()));
                },
            }
        }
    }
    Value::Object(map)
}

/// Extracts the MIME type from a `Content-Type` header, if any.
pub(crate) fn mime_type_from_headers(headers: Option<&HeaderMap>) -> String {
    headers
        .and_then(|headers| headers.get("content-type"))
        .and_then(|value| value.to_str().ok())
        .map(|value| value.split(';').next().unwrap_or_default().trim().to_owned())
        .unwrap_or_default()
}

/// Serializes a CDP `StackTrace` from Servo stack frame information.
pub(crate) fn stack_trace_to_json(
    frames: Option<&[devtools_traits::StackFrame]>,
) -> Option<Value> {
    let frames = frames?;
    if frames.is_empty() {
        return None;
    }
    let call_frames: Vec<Value> = frames
        .iter()
        .map(|frame| {
            json!({
                "functionName": frame.function_name,
                "scriptId": "0",
                // CDP stack frames are zero-based.
                "url": frame.filename,
                "lineNumber": frame.line_number.saturating_sub(1),
                "columnNumber": frame.column_number.saturating_sub(1),
            })
        })
        .collect();
    Some(json!({ "callFrames": call_frames }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_debugger_value_to_remote_object_primitives() {
        assert_eq!(
            debugger_value_to_remote_object(&DebuggerValue::VoidValue),
            json!({ "type": "undefined" })
        );
        assert_eq!(
            debugger_value_to_remote_object(&DebuggerValue::BooleanValue(true)),
            json!({ "type": "boolean", "value": true })
        );
        assert_eq!(
            debugger_value_to_remote_object(&DebuggerValue::NumberValue(42.0)),
            json!({ "type": "number", "value": 42.0 })
        );
        assert_eq!(
            debugger_value_to_remote_object(&DebuggerValue::StringValue("hi".into())),
            json!({ "type": "string", "value": "hi", "description": "hi" })
        );
    }

    #[test]
    fn test_debugger_value_non_finite_numbers() {
        assert_eq!(
            debugger_value_to_remote_object(&DebuggerValue::NumberValue(f64::NAN)),
            json!({ "type": "number", "value": "NaN", "unserializableValue": "NaN" })
        );
        assert_eq!(
            debugger_value_to_remote_object(&DebuggerValue::NumberValue(f64::INFINITY)),
            json!({
                "type": "number",
                "value": "Infinity",
                "unserializableValue": "Infinity"
            })
        );
    }

    #[test]
    fn test_debugger_value_null() {
        assert_eq!(
            debugger_value_to_remote_object(&DebuggerValue::NullValue(true)),
            json!({ "type": "object", "subtype": "null", "value": Value::Null })
        );
    }

    #[test]
    fn test_evaluate_reply_exception() {
        let (object, exception) = evaluate_reply_to_remote_object(
            &DebuggerValue::VoidValue,
            true,
            Some("ReferenceError: foo is not defined"),
        );
        assert_eq!(exception.unwrap()["text"], "Uncaught");
        assert_eq!(object["subtype"], "error");
    }

    #[test]
    fn test_console_log_level_mapping() {
        assert_eq!(console_log_level_to_cdp_type(&ConsoleLogLevel::Warn), "warning");
        assert_eq!(console_log_level_to_cdp_type(&ConsoleLogLevel::Error), "error");
        assert_eq!(console_log_level_to_cdp_level(&ConsoleLogLevel::Error), "error");
        assert_eq!(console_log_level_to_cdp_level(&ConsoleLogLevel::Log), "info");
    }

    #[test]
    fn test_headers_to_cdp_json() {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "text/html".parse().unwrap());
        headers.append("set-cookie", "a=1".parse().unwrap());
        headers.append("set-cookie", "b=2".parse().unwrap());

        let json = headers_to_cdp_json(Some(&headers));
        assert_eq!(json["content-type"], "text/html");
        assert_eq!(json["set-cookie"], "a=1, b=2");
    }

    #[test]
    fn test_mime_type_from_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "text/html; charset=utf-8".parse().unwrap());
        assert_eq!(mime_type_from_headers(Some(&headers)), "text/html");
        assert_eq!(mime_type_from_headers(None), "");
    }
}
