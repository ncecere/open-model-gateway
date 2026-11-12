//! Finished lines stored in encrypted result segments as they complete.
use super::*;

/// Store finished lines in encrypted segments as they complete.
#[cfg_attr(not(test), expect(clippy::cognitive_complexity, reason = "#499"))]
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
                    let mut bytes = serde_json::to_vec(&json!({"n": r.line, "e": r.error, "r": r.value}))
                        .map_err(|_| InferenceError::Storage)?;
                    bytes.push(b'\n');
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
        let mut sink = LazySink::new(
            &files,
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
                if store::insert_segment(&store, job.id, job.workspace_id, next_seq, file, &held)
                    .await
                    .is_err()
                {
                    let _ = files.delete(file, Some(job.workspace_id)).await;
                    tracing::warn!(job_id = %job.id, "batch segment not recorded; its lines are reported unavailable");
                } else {
                    next_seq += 1;
                }
            }
            _ => {
                tracing::warn!(job_id = %job.id, "batch segment not stored; its lines are reported unavailable");
            }
        }
    }
    Ok(())
}
