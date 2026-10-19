use crate::AppError;
use crate::trace_file::{open_segment_trace_file, open_trace_file};
use crate::trace_state::{TracePending, TraceState, TraceWriterState};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

mod event;
#[cfg(test)]
use event::write_event;
use event::{
    meta_fields, prepare_event, with_exclusive_file, write_event_locked, write_meta_header_locked,
};

#[cfg(test)]
use crate::trace_file::segment_path_for_dir;
pub(crate) use crate::trace_file::{ensure_trace_dir, process_start_ms};

pub(crate) const MAX_TRACE_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TRACE_EVENT_BYTES: usize = 1024 * 1024;
static EVENT_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Default)]
pub struct TraceConfig {
    strict: bool,
    state: Arc<TraceState>,
    native: Option<Arc<NativeTrace>>,
}

struct NativeTrace {
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

    pub fn build(
        explicit_path: Option<PathBuf>,
        session_segment_dir: Option<PathBuf>,
        strict: bool,
    ) -> Result<Self, AppError> {
        if strict && explicit_path.is_none() && session_segment_dir.is_none() {
            return Err(AppError::invalid_input_with_suggestion(
                "--trace-strict requires --trace or an active trace-enabled session",
                "Provide --trace <path>, start a session with tracing, or remove --trace-strict.",
            ));
        }
        let (pending, writer) = match explicit_path {
            Some(path) => match open_trace_file(&path) {
                Ok(file) => (
                    TracePending::File(path),
                    TraceWriterState::Open(Arc::new(Mutex::new(file))),
                ),
                Err(err) if strict || err.code() == "INVALID_ARGS" => return Err(err),
                Err(err) => {
                    tracing::warn!("trace open failed: {err}");
                    (TracePending::File(path), TraceWriterState::Failed)
                }
            },
            None => match session_segment_dir {
                Some(dir) => (TracePending::SegmentDir(dir), TraceWriterState::Unopened),
                None => (TracePending::None, TraceWriterState::Unopened),
            },
        };
        Ok(Self {
            native: None,
            strict,
            state: Arc::new(TraceState {
                pending,
                writer: Arc::new(Mutex::new(writer)),
                meta_written: AtomicBool::new(false),
            }),
        })
    }

    pub fn emit(
        &self,
        event: &str,
        session_id: Option<&str>,
        fields: Value,
    ) -> Result<(), AppError> {
        self.emit_lazy(event, session_id, || fields)
    }

    pub fn emit_lazy(
        &self,
        event: &str,
        session_id: Option<&str>,
        fields: impl FnOnce() -> Value,
    ) -> Result<(), AppError> {
        if let Some(native) = &self.native {
            let result = (|| {
                let mut meta = native
                    .meta_written
                    .lock()
                    .map_err(|_| AppError::Internal("trace sink lock poisoned".into()))?;
                if !*meta {
                    native.sink.append(
                        &prepare_event("trace.meta", session_id, meta_fields())?,
                        crate::Deadline::standard()?,
                        MAX_TRACE_FILE_BYTES,
                    )?;
                    *meta = true;
                }
                native.sink.append(
                    &prepare_event(event, session_id, fields())?,
                    crate::Deadline::standard()?,
                    MAX_TRACE_FILE_BYTES,
                )
            })();
            return match result {
                Err(error) if !self.strict => {
                    tracing::warn!(code = error.code(), "native trace append failed");
                    Ok(())
                }
                other => other,
            };
        }
        let writer = match self.ensure_writer()? {
            Some(writer) => writer,
            None => return Ok(()),
        };
        match writer
            .lock()
            .map_err(|_| AppError::Internal("trace writer lock poisoned".into()))
            .and_then(|mut file| {
                with_exclusive_file(&mut file, |file| {
                    self.ensure_meta_if_needed(file, session_id)?;
                    write_event_locked(file, event, session_id, fields())
                })
            }) {
            Ok(()) => Ok(()),
            Err(err) if self.strict => Err(err),
            Err(err) => {
                tracing::warn!("trace write failed: {err}");
                Ok(())
            }
        }
    }

    fn ensure_writer(&self) -> Result<Option<Arc<Mutex<std::fs::File>>>, AppError> {
        let mut writer = self
            .state
            .writer
            .lock()
            .map_err(|_| AppError::Internal("trace writer lock poisoned".into()))?;
        match &*writer {
            TraceWriterState::Open(file) => return Ok(Some(file.clone())),
            TraceWriterState::Failed => return Ok(None),
            TraceWriterState::Unopened => {}
        }
        let open_result = match &self.state.pending {
            TracePending::None => {
                *writer = TraceWriterState::Failed;
                return Ok(None);
            }
            TracePending::File(path) => open_trace_file(path),
            TracePending::SegmentDir(dir) => open_segment_trace_file(dir),
        };
        match open_result {
            Ok(file) => {
                let file = Arc::new(Mutex::new(file));
                *writer = TraceWriterState::Open(file.clone());
                Ok(Some(file))
            }
            Err(err) if self.strict => Err(err),
            Err(err) if err.code() == "INVALID_ARGS" => Err(err),
            Err(err) => {
                tracing::warn!("trace open failed: {err}");
                *writer = TraceWriterState::Failed;
                Ok(None)
            }
        }
    }

    pub(crate) fn has_sink(&self) -> bool {
        if self.is_native() {
            return true;
        }
        if matches!(self.state.pending, TracePending::None) {
            return false;
        }
        match self.state.writer.lock() {
            Ok(writer) => !matches!(*writer, TraceWriterState::Failed),
            Err(_) => true,
        }
    }

    pub(crate) fn pending_file_path(&self) -> Option<&Path> {
        match &self.state.pending {
            TracePending::File(path) => Some(path),
            _ => None,
        }
    }

    pub(crate) fn clone_with_session_segment(
        &self,
        session_segment_dir: Option<PathBuf>,
    ) -> Result<Self, AppError> {
        if self.is_native() || (self.pending_file_path().is_some() && self.has_sink()) {
            return Ok(self.clone());
        }
        match session_segment_dir {
            Some(dir) => Self::build(None, Some(dir), self.strict),
            None => Ok(Self {
                strict: self.strict,
                native: None,
                state: Arc::new(TraceState::default()),
            }),
        }
    }

    fn ensure_meta_if_needed(
        &self,
        file: &mut std::fs::File,
        session_id: Option<&str>,
    ) -> Result<(), AppError> {
        if self.state.meta_written.load(Ordering::Relaxed) {
            return Ok(());
        }
        if file.metadata()?.len() > 0 {
            self.state.meta_written.store(true, Ordering::Relaxed);
            return Ok(());
        }
        write_meta_header_locked(file, session_id)?;
        self.state.meta_written.store(true, Ordering::Relaxed);
        Ok(())
    }
}

#[cfg(test)]
#[path = "trace_tests.rs"]
mod tests;
