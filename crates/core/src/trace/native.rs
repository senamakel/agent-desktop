use super::{MAX_TRACE_FILE_BYTES, TraceConfig, TraceState, meta_fields, prepare_event};
use crate::AppError;
use std::sync::{Arc, Mutex};

pub(super) struct NativeTrace {
    sink: Arc<dyn crate::TraceSink>,
    meta_written: Mutex<bool>,
}

impl std::fmt::Debug for NativeTrace {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeTrace")
            .finish_non_exhaustive()
    }
}

impl TraceConfig {
    pub(crate) fn with_sink(sink: Arc<dyn crate::TraceSink>, strict: bool) -> Self {
        Self {
            strict,
            state: Arc::new(TraceState::default()),
            native: Some(Arc::new(NativeTrace {
                sink,
                meta_written: Mutex::new(false),
            })),
        }
    }

    pub(crate) fn is_native(&self) -> bool {
        self.native.is_some()
    }

    pub(crate) fn artifact(
        &self,
        artifact: crate::TraceArtifact<'_>,
    ) -> Option<Result<String, AppError>> {
        self.native.as_ref().map(|native| {
            let limits = crate::trace_artifact_budget::limits();
            let maximum = match artifact.kind {
                crate::TraceArtifactKind::Screenshot => limits.screenshot_bytes,
                crate::TraceArtifactKind::RefMap => limits.refmap_bytes,
            };
            if artifact.deadline.is_expired() || artifact.bytes.len() as u64 > maximum {
                return Err(AppError::invalid_input(
                    "Trace artifact exceeds its budget or deadline",
                ));
            }
            let reference = native.sink.store_artifact(
                artifact,
                crate::TraceArtifactLimits {
                    screenshot_bytes: limits.screenshot_bytes,
                    screenshot_count: limits.screenshot_count,
                    refmap_bytes: limits.refmap_bytes,
                },
            )?;
            if reference.is_empty()
                || reference.len() > 4096
                || reference.chars().any(char::is_control)
            {
                return Err(AppError::invalid_input(
                    "Trace sink returned an invalid artifact reference",
                ));
            }
            Ok(reference)
        })
    }
}

impl NativeTrace {
    pub(super) fn emit(
        &self,
        strict: bool,
        event: &str,
        session_id: Option<&str>,
        fields: impl FnOnce() -> serde_json::Value,
    ) -> Result<(), AppError> {
        let result = (|| {
            let mut meta = self
                .meta_written
                .lock()
                .map_err(|_| AppError::Internal("trace sink lock poisoned".into()))?;
            if !*meta {
                self.sink.append(
                    &prepare_event("trace.meta", session_id, meta_fields())?,
                    crate::Deadline::standard()?,
                    MAX_TRACE_FILE_BYTES,
                )?;
                *meta = true;
            }
            self.sink.append(
                &prepare_event(event, session_id, fields())?,
                crate::Deadline::standard()?,
                MAX_TRACE_FILE_BYTES,
            )
        })();
        match result {
            Err(error) if !strict => {
                tracing::warn!(code = error.code(), "native trace append failed");
                Ok(())
            }
            other => other,
        }
    }
}
