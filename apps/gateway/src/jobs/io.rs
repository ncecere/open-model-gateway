//! Streaming file I/O of batches through the encrypted file store
//! (`filestore::FileStorage`): JSONL readers over stored files or provider
//! bodies, and lazily created, streamed writers for result files. Content is
//! held one line (or one channel item) at a time and never logged.
use std::collections::VecDeque;

use axum::body::Bytes;
use futures_util::StreamExt;
use serde_json::Value;
use uuid::Uuid;

use super::lines::Lines;
use crate::{
    filestore::{FileError, FileStorage, FileStoreError, NewFile, Purpose, QuotaMode, StoredFile},
    inference::error::InferenceError,
};

/// Largest result line read back (a full response; embeddings can be big).
pub(crate) const RESULT_LINE_BYTES: usize = 64 * 1024 * 1024;

/// A source of byte chunks.
pub(crate) enum Source {
    Stored(crate::filestore::ByteStream),
    Upstream(super::types::ByteStream),
}

/// Reads lines from a byte stream, one at a time.
pub(crate) struct LineReader {
    source: Source,
    lines: Lines,
    queue: VecDeque<Vec<u8>>,
    done: bool,
}
impl LineReader {
    pub(crate) fn new(source: Source, max_line: usize) -> Self {
        Self {
            source,
            lines: Lines::with_limit(max_line),
            queue: VecDeque::new(),
            done: false,
        }
    }
    /// Open a stored file of this workspace.
    pub(crate) async fn stored(
        files: &FileStorage,
        id: Uuid,
        workspace: Uuid,
        max_line: usize,
    ) -> Result<Self, FileError> {
        let (_, stream) = files.open(id, Some(workspace)).await?;
        Ok(Self::new(Source::Stored(stream), max_line))
    }
    /// The next line (without `\n`), `None` at the end. Any stream error is
    /// a failure of the whole object.
    pub(crate) async fn next(&mut self) -> Result<Option<Vec<u8>>, InferenceError> {
        loop {
            if let Some(line) = self.queue.pop_front() {
                return Ok(Some(line));
            }
            if self.done {
                return Ok(None);
            }
            let chunk = match &mut self.source {
                Source::Stored(s) => match s.next().await {
                    Some(Ok(c)) => Some(c),
                    Some(Err(_)) => return Err(InferenceError::Storage),
                    None => None,
                },
                Source::Upstream(s) => match s.next().await {
                    Some(Ok(c)) => Some(c),
                    Some(Err(e)) => return Err(e),
                    None => None,
                },
            };
            match chunk {
                Some(data) => {
                    let lines = self
                        .lines
                        .push(&data)
                        .map_err(|_| InferenceError::InvalidUpstream)?;
                    self.queue.extend(lines);
                }
                None => {
                    self.done = true;
                    self.queue.extend(self.lines.finish());
                }
            }
        }
    }
    /// The next non-blank line.
    pub(crate) async fn next_request(&mut self) -> Result<Option<Vec<u8>>, InferenceError> {
        while let Some(line) = self.next().await? {
            if !super::lines::is_blank(&line) {
                return Ok(Some(line));
            }
        }
        Ok(None)
    }
}

type Sender = tokio::sync::mpsc::Sender<Result<Bytes, FileStoreError>>;

/// A stored file written from a channel while the producer runs.
pub(crate) struct FileSink {
    tx: Option<Sender>,
    task: tokio::task::JoinHandle<Result<StoredFile, FileError>>,
}
impl FileSink {
    pub(crate) fn open(files: &FileStorage, new: NewFile) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<Bytes, FileStoreError>>(8);
        let body: crate::filestore::ByteStream = Box::pin(async_stream::stream! {
            while let Some(item) = rx.recv().await {
                yield item;
            }
        });
        let files = files.clone();
        let task = tokio::spawn(async move { files.create(new, body).await });
        Self { tx: Some(tx), task }
    }
    pub(crate) async fn write(&mut self, bytes: Vec<u8>) -> Result<(), InferenceError> {
        let tx = self.tx.as_ref().ok_or(InferenceError::Storage)?;
        if tx.send(Ok(Bytes::from(bytes))).await.is_err() {
            // The store ended the upload early (an error).
            return Err(InferenceError::Storage);
        }
        Ok(())
    }
    pub(crate) async fn finish(mut self) -> Result<StoredFile, InferenceError> {
        drop(self.tx.take());
        match (&mut self.task).await {
            Ok(Ok(file)) => Ok(file),
            _ => Err(InferenceError::Storage),
        }
    }
    /// Abandon the upload (the partial object is removed).
    pub(crate) async fn abort(mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Err(FileStoreError::Source)).await;
        }
        let _ = (&mut self.task).await;
    }
}

/// A result file that exists only once its first line is written.
pub(crate) struct LazySink {
    files: FileStorage,
    template: NewFile,
    sink: Option<FileSink>,
    pub(crate) lines: u64,
}
impl LazySink {
    pub(crate) fn new(files: &FileStorage, template: NewFile) -> Self {
        Self {
            files: files.clone(),
            template,
            sink: None,
            lines: 0,
        }
    }
    pub(crate) async fn line(&mut self, value: &Value) -> Result<(), InferenceError> {
        let mut bytes = serde_json::to_vec(value).map_err(|_| InferenceError::Storage)?;
        bytes.push(b'\n');
        self.raw(bytes).await
    }
    pub(crate) async fn raw(&mut self, bytes: Vec<u8>) -> Result<(), InferenceError> {
        let sink = match &mut self.sink {
            Some(s) => s,
            None => self
                .sink
                .insert(FileSink::open(&self.files, self.template.clone())),
        };
        sink.write(bytes).await?;
        self.lines += 1;
        Ok(())
    }
    /// The stored file id, `None` when nothing was written.
    pub(crate) async fn finish(self) -> Result<Option<Uuid>, InferenceError> {
        match self.sink {
            Some(s) => Ok(Some(s.finish().await?.id)),
            None => Ok(None),
        }
    }
    pub(crate) async fn abort(self) {
        if let Some(s) = self.sink {
            s.abort().await;
        }
    }
}

/// Template of a batch's result file (`batch_output`, listed by `/v1/files`)
/// or internal file (the private copy and result segments: `batch_output`
/// stored without an API purpose, so `/v1/files` never lists or serves it).
/// Both count toward the workspace's storage but never fail on its quota: the
/// work was already admitted (and paid for).
pub(crate) fn batch_file(
    internal: bool,
    workspace: Uuid,
    api_key: Uuid,
    user: Option<Uuid>,
    filename: String,
) -> NewFile {
    let base = if internal {
        NewFile::internal(Purpose::BatchOutput, Some(workspace))
    } else {
        NewFile::new(Purpose::BatchOutput, Some(workspace))
    };
    NewFile {
        created_by_api_key_id: Some(api_key),
        created_by_user_id: user,
        filename: Some(filename),
        content_type: Some("application/jsonl".into()),
        quota: QuotaMode::CountOnly,
        ..base
    }
}
