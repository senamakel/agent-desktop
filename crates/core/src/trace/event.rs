use super::{EVENT_SEQ, MAX_TRACE_EVENT_BYTES, MAX_TRACE_FILE_BYTES};
use crate::AppError;
use crate::trace_file::process_start_ms;
use crate::trace_sanitize::sanitize_trace_value;
use serde_json::{Map, Value, json};
use std::io::Write;
use std::sync::atomic::Ordering;

pub(super) fn write_meta_header_locked(
    file: &mut std::fs::File,
    session_id: Option<&str>,
) -> Result<(), AppError> {
    write_event_locked(file, "trace.meta", session_id, meta_fields())
}

pub(super) fn meta_fields() -> Value {
    json!({ "schema": 1, "version": env!("CARGO_PKG_VERSION"),
        "os": std::env::consts::OS, "pid": std::process::id(),
        "proc_start_ms": process_start_ms() })
}

#[cfg(test)]
pub(super) fn write_event(
    file: &mut std::fs::File,
    event: &str,
    session_id: Option<&str>,
    fields: Value,
) -> Result<(), AppError> {
    with_exclusive_file(file, |file| {
        write_event_locked(file, event, session_id, fields)
    })
}

pub(super) fn prepare_event(
    event: &str,
    session_id: Option<&str>,
    fields: Value,
) -> Result<Value, AppError> {
    let envelope_bytes = event
        .len()
        .saturating_mul(6)
        .saturating_add(session_id.map_or(0, |session| session.len().saturating_mul(6)))
        .saturating_add(512);
    if envelope_bytes >= MAX_TRACE_EVENT_BYTES
        || !value_fits(&fields, MAX_TRACE_EVENT_BYTES - envelope_bytes)
    {
        return Err(AppError::invalid_input_with_suggestion(
            "Trace event exceeds the maximum supported event size",
            "Emit bounded metadata and omit raw application content from trace fields.",
        ));
    }
    let mut body = match sanitize_trace_value(fields) {
        Value::Object(fields) => fields,
        _ => Map::new(),
    };
    body.insert("event".to_string(), json!(event));
    body.insert(
        "ts_ms".to_string(),
        json!(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|err| AppError::Internal(err.to_string()))?
                .as_millis()
        ),
    );
    body.insert(
        "seq".to_string(),
        json!(EVENT_SEQ.fetch_add(1, Ordering::Relaxed)),
    );
    body.insert("writer_pid".to_string(), json!(std::process::id()));
    body.insert(
        "writer_proc_start_ms".to_string(),
        json!(process_start_ms()),
    );
    if let Some(sid) = session_id {
        body.insert("session_id".to_string(), json!(sid));
    }
    Ok(Value::Object(body))
}

pub(super) fn write_event_locked(
    file: &mut std::fs::File,
    event: &str,
    session_id: Option<&str>,
    fields: Value,
) -> Result<(), AppError> {
    let body = prepare_event(event, session_id, fields)?;
    let mut line = Vec::new();
    serde_json::to_writer(&mut line, &body)?;
    line.push(b'\n');
    reject_oversized_trace(file, line.len() as u64)?;
    file.write_all(&line).map_err(AppError::from)
}

fn value_fits(value: &Value, max_bytes: usize) -> bool {
    fn visit(value: &Value, remaining: &mut usize) -> bool {
        let fixed = match value {
            Value::Null => 4,
            Value::Bool(_) => 5,
            Value::Number(_) => 32,
            Value::String(string) => string.len().saturating_mul(6),
            Value::Array(values) => {
                if !take(remaining, values.len()) {
                    return false;
                }
                return values.iter().all(|value| visit(value, remaining));
            }
            Value::Object(values) => {
                for (key, value) in values {
                    if !take(remaining, key.len().saturating_mul(6).saturating_add(4))
                        || !visit(value, remaining)
                    {
                        return false;
                    }
                }
                return true;
            }
        };
        take(remaining, fixed)
    }

    fn take(remaining: &mut usize, amount: usize) -> bool {
        let Some(next) = remaining.checked_sub(amount) else {
            return false;
        };
        *remaining = next;
        true
    }

    let mut remaining = max_bytes;
    visit(value, &mut remaining)
}

fn reject_oversized_trace(file: &std::fs::File, incoming: u64) -> Result<(), AppError> {
    let len = file.metadata()?.len();
    if len.saturating_add(incoming) <= MAX_TRACE_FILE_BYTES {
        return Ok(());
    }
    Err(AppError::invalid_input_with_suggestion(
        "Trace file reached the maximum supported size",
        "Start a new --trace file or rotate the existing trace before retrying.",
    ))
}

fn with_exclusive_file<T>(
    file: &mut std::fs::File,
    operation: impl FnOnce(&mut std::fs::File) -> Result<T, AppError>,
) -> Result<T, AppError> {
    file.lock().map_err(AppError::from)?;
    let result = operation(file);
    let unlock = file.unlock().map_err(AppError::from);
    match result {
        Ok(value) => {
            unlock?;
            Ok(value)
        }
        Err(error) => Err(error),
    }
}
