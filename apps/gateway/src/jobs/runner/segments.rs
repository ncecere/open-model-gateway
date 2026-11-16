//! Finished lines stored in encrypted result segments as they complete.
use super::*;

/// One finished line as a JSON record, newline-terminated.
fn encode_record(r: &Record) -> Result<Vec<u8>, InferenceError> {
    let mut bytes = serde_json::to_vec(&json!({"n": r.line, "e": r.error, "r": r.value}))
        .map_err(|_| InferenceError::Storage)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Writes one segment file and records it under the next segment number
/// (taken in the database, inside the fence). A segment that is not stored or
/// recorded leaves its lines reported unavailable; one the run is no longer
/// allowed to record is deleted and ends the run's work.
async fn store_segment(
    store: &Store,
    files: &crate::filestore::FileStorage,
    job: &JobRow,
    state: &RunState,
    part: u32,
    body: Vec<u8>,
    held: &[i32],
) {
    let mut sink = LazySink::new(
        files,
        io::batch_file(
            true,
            job.workspace_id,
            job.api_key_id,
            job.user_id,
            format!(
                "batch_{}_part_{}_{part}.jsonl",
                job.id.simple(),
                state.fence.epoch
            ),
        ),
    );
    let stored = async {
        sink.raw(body).await?;
        sink.finish().await
    }
    .await;
    match stored {
        Ok(Some(file)) => {
            let recorded = state
                .fence
                .insert_segment(store, job.workspace_id, file, held)
                .await;
            if !matches!(state.written(recorded), Ok(Some(_))) {
                let _ = files.delete(file, Some(job.workspace_id)).await;
                tracing::warn!(job_id = %job.id, "batch segment not recorded; its lines are reported unavailable");
            }
        }
        _ => {
            tracing::warn!(job_id = %job.id, "batch segment not stored; its lines are reported unavailable");
        }
    }
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
        };
        if !flush || lines.is_empty() {
            continue;
        }
        let body = std::mem::take(&mut buffer);
        let held = std::mem::take(&mut lines);
        // A run the fence refused stores nothing more; what it holds is
        // dropped (the owner reports those lines unavailable).
        if !state.fenced.load(Ordering::SeqCst) {
            store_segment(&store, &files, &job, &state, part, body, &held).await;
            part += 1;
        }
    }
    Ok(())
}
