use super::CommandContext;
use crate::AppError;
use serde_json::{Value, json};
use std::cell::Cell;
use std::time::Instant;

/// Emits `command.start` on construction and `command.end` on `complete`.
/// The `Drop` guard emits a fallback `command.end` for a scope abandoned
/// without `complete` (normal early return, or a panic under the unwind
/// test profile). It cannot fire on a genuine panic in the release binary:
/// `[profile.release]` uses `panic = "abort"`, which terminates without
/// unwinding `Drop`. A panicked release command therefore leaves an
/// unpaired `command.start`, which the trace reader tolerates as an
/// `unpaired_command` warning rather than a lost record.
pub struct CommandScope<'a> {
    context: &'a CommandContext,
    command: &'static str,
    success_disposition: crate::DeliverySemantics,
    started: Instant,
    finished: Cell<bool>,
    _deadline_scope: crate::deadline::DeadlineScope,
}

impl<'a> CommandScope<'a> {
    pub(super) fn new(
        context: &'a CommandContext,
        command: &'static str,
        success_disposition: crate::DeliverySemantics,
        deadline_scope: crate::deadline::DeadlineScope,
    ) -> Self {
        Self {
            context,
            command,
            success_disposition,
            started: Instant::now(),
            finished: Cell::new(false),
            _deadline_scope: deadline_scope,
        }
    }

    pub fn complete(self, result: &Result<Value, AppError>) -> Result<(), AppError> {
        self.finished.set(true);
        let emitted = match result {
            Ok(_) => self.emit_end(true, None),
            Err(err) => self.emit_end(false, Some(err.code())),
        };
        emitted.map_err(|error| {
            super::trace_error_with_disposition(
                error,
                super::result_disposition(result, self.success_disposition),
            )
        })
    }

    fn emit_end(&self, ok: bool, code: Option<&str>) -> Result<(), AppError> {
        let mut fields = json!({
            "command": self.command,
            "ok": ok,
            "duration_ms": self.started.elapsed().as_millis(),
        });
        if let Some(code) = code {
            fields["code"] = json!(code);
        }
        self.context.trace("command.end", fields)
    }
}

impl Drop for CommandScope<'_> {
    fn drop(&mut self) {
        if self.finished.get() {
            return;
        }
        let _ = self.emit_end(false, Some("INTERNAL"));
    }
}
