use super::*;

#[derive(Debug, Default)]
struct NativeArtifactSink {
    events: Mutex<Vec<serde_json::Value>>,
    artifacts: Mutex<Vec<(crate::TraceArtifactKind, String, Vec<u8>)>>,
    fail: bool,
    fail_event: Option<&'static str>,
}

impl crate::TraceSink for NativeArtifactSink {
    fn append(
        &self,
        event: &serde_json::Value,
        _deadline: crate::Deadline,
        _budget: u64,
    ) -> Result<(), AppError> {
        if self.fail_event.is_some_and(|kind| event["event"] == kind) {
            return Err(AppError::Internal("unavailable".into()));
        }
        self.events.lock().unwrap().push(event.clone());
        Ok(())
    }

    fn store_artifact(
        &self,
        artifact: crate::TraceArtifact<'_>,
        limits: crate::TraceArtifactLimits,
    ) -> Result<String, AppError> {
        if self.fail {
            return Err(AppError::Internal("unavailable".into()));
        }
        let mut items = self.artifacts.lock().unwrap();
        let same_kind = items
            .iter()
            .filter(|item| item.0 == artifact.kind)
            .collect::<Vec<_>>();
        let bytes = same_kind
            .iter()
            .map(|item| item.2.len() as u64)
            .sum::<u64>();
        let maximum = match artifact.kind {
            crate::TraceArtifactKind::Screenshot => limits.screenshot_bytes,
            crate::TraceArtifactKind::RefMap => limits.refmap_bytes,
        };
        if bytes + artifact.bytes.len() as u64 > maximum
            || (artifact.kind == crate::TraceArtifactKind::Screenshot
                && same_kind.len() >= limits.screenshot_count as usize)
        {
            return Err(AppError::invalid_input("budget"));
        }
        items.push((artifact.kind, artifact.name.into(), artifact.bytes.into()));
        Ok(format!("opaque:{}", artifact.name))
    }
}

#[test]
fn native_sink_captures_screenshots_and_refmaps_without_session_artifact_files() {
    let (_home, _lock) = setup_artifacts_test();
    let manifest = start_session(StartSessionOptions {
        trace: SessionTraceMode::On,
        artifacts: ArtifactsMode::Full,
        ..Default::default()
    })
    .unwrap();
    let sink = std::sync::Arc::new(NativeArtifactSink::default());
    let context =
        CommandContext::new_with_trace_sink(Some(manifest.id.clone()), sink.clone(), true, true)
            .unwrap();
    let child = context.for_batch_item(None).unwrap();
    run_ref_action(&child, &png_adapter(), 1).unwrap();
    let store = RefStore::for_session(Some(&manifest.id)).unwrap();
    copy_refmap_if_full(&child, &store, "fixture", &crate::refs::RefMap::new()).unwrap();
    let items = sink.artifacts.lock().unwrap();
    assert!(
        items
            .iter()
            .any(|item| item.0 == crate::TraceArtifactKind::Screenshot && item.2 == MINI_PNG)
    );
    assert!(
        items
            .iter()
            .any(|item| item.0 == crate::TraceArtifactKind::RefMap)
    );
    assert!(!store.trace_dir().join("screens").exists());
    assert!(!store.trace_dir().join("refmaps").exists());
    assert!(!store.trace_dir().join(".artifact-usage.json").exists());
    assert!(sink.events.lock().unwrap().iter().any(|event| {
        event["screenshot_pre"]
            .as_str()
            .is_some_and(|value| value.starts_with("opaque:"))
    }));
}

#[test]
fn failing_native_artifact_sink_never_reopens_session_file_adapter() {
    let (_home, _lock) = setup_artifacts_test();
    let manifest = start_session(StartSessionOptions {
        trace: SessionTraceMode::On,
        artifacts: ArtifactsMode::Full,
        ..Default::default()
    })
    .unwrap();
    let sink = std::sync::Arc::new(NativeArtifactSink {
        fail: true,
        ..Default::default()
    });
    let context =
        CommandContext::new_with_trace_sink(Some(manifest.id.clone()), sink.clone(), true, true)
            .unwrap();
    run_ref_action(&context, &png_adapter(), 1).unwrap();
    let store = RefStore::for_session(Some(&manifest.id)).unwrap();
    copy_refmap_if_full(&context, &store, "fixture", &crate::refs::RefMap::new()).unwrap();
    assert!(!store.trace_dir().join("screens").exists());
    assert!(!store.trace_dir().join("refmaps").exists());
    assert!(sink.events.lock().unwrap().iter().any(|event| event["skipped_pre"] == "sink_failed" || event["skipped"] == "sink_failed"));
}

#[test]
fn native_sink_receives_existing_aggregate_screenshot_limits() {
    let (_home, _lock) = setup_artifacts_test();
    set_test_budgets(1024, 1, 1024);
    let sink = std::sync::Arc::new(NativeArtifactSink::default());
    let context = CommandContext::new_with_trace_sink(None, sink.clone(), false, true).unwrap();
    run_ref_action(&context, &png_adapter(), 1).unwrap();
    assert_eq!(sink.artifacts.lock().unwrap().len(), 1);
    assert!(
        sink.events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event["skipped_post"] == "sink_failed")
    );
    clear_test_budgets();
}

#[test]
fn strict_native_sink_failure_after_fake_dispatch_keeps_unsafe_delivery() {
    let (_home, _lock) = setup_artifacts_test();
    let sink = std::sync::Arc::new(NativeArtifactSink {
        fail_event: Some("action.dispatch.ok"),
        ..Default::default()
    });
    let context = CommandContext::new_with_trace_sink(None, sink.clone(), true, false).unwrap();
    let error = run_ref_action(&context, &png_adapter(), 1).unwrap_err();
    let AppError::Adapter(error) = error else {
        panic!("delivery error expected");
    };
    assert_eq!(
        error.disposition,
        crate::DeliverySemantics::delivered_unverified()
    );
    assert!(
        sink.events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event["event"] == "action.dispatch.start")
    );
}

#[test]
fn changed_child_session_keeps_original_sink_and_never_opens_trace_segments() {
    let (_home, _lock) = setup_artifacts_test();
    let first = start_session(StartSessionOptions::default()).unwrap();
    let second = start_session(StartSessionOptions {
        trace: SessionTraceMode::On,
        artifacts: ArtifactsMode::Full,
        ..Default::default()
    })
    .unwrap();
    let sink = std::sync::Arc::new(NativeArtifactSink::default());
    let context =
        CommandContext::new_with_trace_sink(Some(first.id), sink.clone(), true, true).unwrap();
    let child = context.for_batch_item(Some(second.id.clone())).unwrap();
    child.trace("child.event", json!({})).unwrap();
    run_ref_action(&child, &png_adapter(), 1).unwrap();
    let trace_dir = RefStore::for_session(Some(&second.id)).unwrap().trace_dir();
    let files = std::fs::read_dir(&trace_dir)
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(files, 0);
    assert!(
        sink.events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event["event"] == "child.event" && event["session_id"] == second.id)
    );
}

#[test]
fn disabled_native_artifacts_never_call_sink_artifact_port() {
    let (_home, _lock) = setup_artifacts_test();
    let sink = std::sync::Arc::new(NativeArtifactSink::default());
    let context = CommandContext::new_with_trace_sink(None, sink.clone(), true, false).unwrap();
    run_ref_action(&context, &png_adapter(), 1).unwrap();
    assert!(sink.artifacts.lock().unwrap().is_empty());
}
