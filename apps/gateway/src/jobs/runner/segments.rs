//! Finished lines stored in encrypted result segments as they complete.
use super::*;

/// One finished line as a JSON record, newline-terminated.
pub(super) fn encode_record(r: &Record) -> Result<Vec<u8>, InferenceError> {
    let mut bytes = serde_json::to_vec(&json!({"n": r.line, "e": r.error, "r": r.value}))
        .map_err(|_| InferenceError::Storage)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Writes one segment file and records it under the next segment number
/// (taken in the database, inside the fence). A segment that is not stored or
/// recorded leaves its lines reported unavailable; one the run is no longer
/// allowed to record is deleted and ends the run's work.
pub(super) async fn store_segment(
    store: &Store,
    files: &crate::filestore::FileStorage,
    job: &JobRow,
    state: &RunState,
    part: impl std::fmt::Display,
    body: Vec<u8>,
    held: super::super::fence::SegmentLines<'_>,
) -> FenceResult<()> {
    let template = io::batch_file(
        true,
        job.workspace_id,
        job.api_key_id,
        job.user_id,
        format!(
            "batch_{}_part_{}_{part}.jsonl",
            job.id.simple(),
            state.fence.epoch
        ),
    );
    let stream = futures_util::stream::once(async move { Ok(axum::body::Bytes::from(body)) });
    let file = files
        .create(template, Box::pin(stream))
        .await
        .map_err(|_| FenceError::Storage)?
        .id;
    let recorded = state
        .fence
        .insert_segment(store, job.workspace_id, file, held)
        .await;
    if matches!(recorded, Err(FenceError::Stale | FenceError::Storage)) {
        let _ = files.delete(file, Some(job.workspace_id)).await;
    }
    recorded.map(|_| ())
}

/// Store finished lines in encrypted segments as they complete.
pub(super) async fn write_segments(
    store: Store,
    files: crate::filestore::FileStorage,
    job: JobRow,
    state: Arc<RunState>,
    mut records: mpsc::Receiver<Record>,
) -> Result<(), InferenceError> {
    let mut part = 0u32;
    let mut buffer: Vec<u8> = Vec::new();
    let mut lines: Vec<i32> = Vec::new();
    let mut open = true;
    while open {
        let deadline = tokio::time::sleep(SEGMENT_AGE);
        tokio::pin!(deadline);
        let flush = tokio::select! {
            record = records.recv() => match record {
                Some(r) => {
                    let bytes = encode_record(&r)?;
                    buffer.extend_from_slice(&bytes);
                    lines.push(r.line);
                    lines.len() >= SEGMENT_LINES || buffer.len() >= SEGMENT_BYTES
                }
                None => {
                    open = false;
                    true
                }
            },
            _ = &mut deadline => true,
            // The lease is in doubt: store what is held before a takeover
            // can refuse it.
            _ = state.doubt.notified() => true,
        };
        if !flush || lines.is_empty() {
            continue;
        }
        let body = std::mem::take(&mut buffer);
        let held = std::mem::take(&mut lines);
        // A run the fence refused stores nothing more; what it holds is
        // dropped (the owner reports those lines unavailable).
        if !state.fenced.load(Ordering::SeqCst) {
            let result = store_segment(
                &store,
                &files,
                &job,
                &state,
                part,
                body,
                super::super::fence::SegmentLines::Failed(&held),
            )
            .await;
            if state.written(result).is_err() {
                tracing::warn!(job_id = %job.id, "batch error segment unavailable");
            }
            part += 1;
        }
    }
    Ok(())
}

/// The segment writer of one run, driven by tests.
#[cfg(test)]
pub(crate) struct Probe {
    state: Arc<RunState>,
    records: mpsc::Sender<Record>,
    writer: tokio::task::JoinHandle<Result<(), InferenceError>>,
}
#[cfg(test)]
impl Probe {
    pub(crate) fn start(
        store: Store,
        files: crate::filestore::FileStorage,
        job: JobRow,
        fence: BatchFence,
    ) -> Self {
        let state = Arc::new(RunState::new(fence));
        let (records, receiver) = mpsc::channel(8);
        let writer = tokio::spawn(write_segments(store, files, job, state.clone(), receiver));
        Self {
            state,
            records,
            writer,
        }
    }
    pub(crate) async fn finished(&self, line: i32) {
        let value = json!({"custom_id": line.to_string()});
        self.records
            .send(Record {
                line,
                error: true,
                value,
            })
            .await
            .unwrap();
    }
    /// The lease is in doubt (a renewal failed).
    pub(crate) fn doubt(&self) {
        self.state.doubt.notify_one();
    }
    pub(crate) fn fenced(&self) -> bool {
        self.state.fenced.load(Ordering::SeqCst)
    }
    pub(crate) async fn close(self) {
        drop(self.records);
        self.writer.await.unwrap().unwrap();
    }
}
