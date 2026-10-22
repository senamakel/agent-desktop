use super::*;

impl CommandContext {
    /// Select a native trace sink before file/session trace writers are opened.
    ///
    /// The sink captures trusted authority and survives child/session changes.
    /// Other session state (reference stores, leases) retains its normal behavior.
    /// Artifacts are optional; enabled artifacts go exclusively to this sink.
    pub fn new_with_trace_sink(
        session_id: Option<String>,
        sink: std::sync::Arc<dyn crate::TraceSink>,
        strict: bool,
        artifacts_full: bool,
    ) -> Result<Self, AppError> {
        if let Some(id) = session_id.as_deref() {
            validate_session_id(id)?;
        }
        let session = acquire_session_scope(session_id, None)?;
        let cursor_overlay =
            session::cursor_overlay_for_session(session.as_ref().map(|scope| scope.id.as_str()))?;
        Ok(Self {
            session,
            trace: TraceConfig::with_sink(sink, strict),
            artifacts_full,
            options: CommandOptions {
                cursor_overlay,
                ..CommandOptions::default()
            },
            ..Self::default()
        })
    }

    pub(crate) fn native_trace_sink(&self) -> bool {
        self.trace.is_native()
    }

    pub(crate) fn trace_artifact(
        &self,
        artifact: crate::TraceArtifact<'_>,
    ) -> Option<Result<String, AppError>> {
        self.trace.artifact(artifact)
    }
}
