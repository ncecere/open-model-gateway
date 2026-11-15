//! Finished lines stored in encrypted result segments as they complete.
use super::*;

/// One finished line as a JSON record, newline-terminated.
fn encode_record(r: &Record) -> Result<Vec<u8>, InferenceError> {
    let mut bytes = serde_json::to_vec(&json!({"n": r.line, "e": r.error, "r": r.value}))
        .map_err(|_| InferenceError::Storage)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Writes one segment file and records it; returns whether it was recorded
/// (the next segment number is then taken). A segment that is not stored or
/// recorded leaves its lines reported unavailable.
async fn store_segment(
    store: &Store,
    files: &crate::filestore::FileStorage,
    job: &JobRow,
    next_seq: i32,
    body: Vec<u8>,
    held: &[i32],
) -> bool {
    let mut sink = LazySink::new(
        files,
        io::batch_file(
            true,
            job.workspace_id,
            job.api_key_id,
            job.user_id,
            format!("batch_{}_part_{next_seq}.jsonl", job.id.simple()),
        ),
    );
    let stored = async {
        sink.raw(body).await?;
        sink.finish().await
    }
    .await;
    match stored {
        Ok(Some(file)) => {
            if store::insert_segment(store, job.id, job.workspace_id, next_seq, file, held)
                .await
                .is_err()
            {
                let _ = files.delete(file, Some(job.workspace_id)).await;
                tracing::warn!(job_id = %job.id, "batch segment not recorded; its lines are reported unavailable");
                false
            } else {
                true
            }
        }
        _ => {
            tracing::warn!(job_id = %job.id, "batch segment not stored; its lines are reported unavailable");
            false
        }
    }
}

/// Store finished lines in encrypted segments as they complete.
pub(super) async fn write_segments(
    store: Store,
    files: crate::filestore::FileStorage,
    job: JobRow,
    mut records: mpsc::Receiver<Record>,
) -> Result<(), InferenceError> {
    let mut next_seq = store::segments(&store, job.id)
        .await
        .map_err(|_| InferenceError::Storage)?
        .last()
        .map_or(0, |s| s.0 + 1);
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
        if store_segment(&store, &files, &job, next_seq, body, &held).await {
            next_seq += 1;
        }
    }
    Ok(())
}
