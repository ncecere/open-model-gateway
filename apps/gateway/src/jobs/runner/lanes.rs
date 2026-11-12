//! Dispatch planning: one lane of unclaimed lines per model, the route a
//! claimed line is pinned to, worker permits and the batch's demand report.
use super::*;

/// The route a claimed line is pinned to.
#[derive(Clone, Copy, Debug)]
pub(super) struct Pin {
    pub(super) deployment: Uuid,
    /// vLLM `priority` hint (route setting), sent on this line only.
    pub(super) priority: Option<i32>,
    pub(super) max_concurrency: i32,
}

/// Outcome of planning a waiting line's route.
pub(super) enum Planned {
    Route(Deployment),
    /// Every route serving the line is cooling down, or planning hit a
    /// transient storage/timeout failure: wait and plan again, do not fail.
    CoolingDown,
    /// Nothing serves the line (it fails like an interactive request).
    Nothing,
}

/// The next line of a lane, read and planned.
pub(super) struct Head {
    pub(super) n: i32,
    raw: Vec<u8>,
    pub(super) execution: Uuid,
    pub(super) request: Option<BatchRequest>,
    pub(super) plan: Option<lines::LinePlan>,
    pub(super) route: Option<Deployment>,
    /// Planned while every route was cooling down.
    pub(super) cooling: bool,
    pub(super) planned: Option<Instant>,
}

/// The unclaimed lines of one model, read in order from the batch's copy.
pub(super) struct Lane {
    pub(super) lines: VecDeque<i32>,
    reader: Option<LineReader>,
    read: i32,
    pub(super) head: Option<Head>,
}
impl Lane {
    /// One pass over the batch's copy: unclaimed line numbers per model
    /// (metadata only; lines are read again when they are due).
    pub(super) async fn scan(
        files: &crate::filestore::FileStorage,
        work: Uuid,
        workspace: Uuid,
        endpoint: BatchEndpoint,
        claimed: &HashSet<i32>,
    ) -> Result<Vec<Lane>, InferenceError> {
        let mut reader = LineReader::stored(files, work, workspace, WORK_LINE_BYTES)
            .await
            .map_err(|_| InferenceError::Storage)?;
        let mut lanes: Vec<Lane> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        let mut n: i32 = -1;
        while let Some(raw) = reader.next_request().await? {
            n += 1;
            if claimed.contains(&n) {
                continue;
            }
            let model = lines::parse_work_line(&raw, endpoint)
                .map(|l| l.request.model().to_owned())
                .unwrap_or_default();
            let i = *index.entry(model).or_insert_with(|| {
                lanes.push(Lane {
                    lines: VecDeque::new(),
                    reader: None,
                    read: -1,
                    head: None,
                });
                lanes.len() - 1
            });
            lanes[i].lines.push_back(n);
        }
        Ok(lanes)
    }
    /// Read (and parse) the lane's next line if it is not loaded yet.
    pub(super) async fn load(
        &mut self,
        files: &crate::filestore::FileStorage,
        work: Uuid,
        workspace: Uuid,
        endpoint: BatchEndpoint,
    ) -> Result<(), InferenceError> {
        if self.head.is_some() {
            return Ok(());
        }
        let Some(&n) = self.lines.front() else {
            return Ok(());
        };
        if self.reader.is_none() {
            self.reader = Some(
                LineReader::stored(files, work, workspace, WORK_LINE_BYTES)
                    .await
                    .map_err(|_| InferenceError::Storage)?,
            );
            self.read = -1;
        }
        let reader = self.reader.as_mut().ok_or(InferenceError::Storage)?;
        while self.read < n {
            let raw = reader
                .next_request()
                .await?
                .ok_or(InferenceError::Storage)?;
            self.read += 1;
            if self.read == n {
                let line = lines::parse_work_line(&raw, endpoint).ok();
                let plan = line.as_ref().and_then(|l| l.plan);
                let request = line.map(|l| l.request);
                self.head = Some(Head {
                    n,
                    raw,
                    execution: Uuid::now_v7(),
                    request,
                    plan,
                    route: None,
                    cooling: false,
                    planned: None,
                });
                return Ok(());
            }
        }
        Err(InferenceError::Storage)
    }
    /// Remove the head (claimed, or claimed elsewhere).
    pub(super) fn take(&mut self) -> Option<Head> {
        self.lines.pop_front();
        self.head.take()
    }
}

/// This batch's and this process's worker permits, if both are free.
pub(super) fn permits(
    per_batch: &Arc<Semaphore>,
    pool: &Arc<Semaphore>,
) -> Option<(
    tokio::sync::OwnedSemaphorePermit,
    tokio::sync::OwnedSemaphorePermit,
)> {
    let local = per_batch.clone().try_acquire_owned().ok()?;
    let global = pool.clone().try_acquire_owned().ok()?;
    Some((local, global))
}

/// Why a line waits when no worker permit is free: the route's own batch
/// limit when it is reached (all batches), otherwise the gateway's workers.
pub(super) fn wait_reason_without_permits(
    route_running: Option<i64>,
    max_concurrency: i32,
) -> Pause {
    match route_running {
        Some(n) if n >= i64::from(max_concurrency) => Pause::Concurrency,
        _ => Pause::Workers,
    }
}

/// Merge one lane's state into the batch's demand on a route.
pub(super) fn note(
    report: &mut BTreeMap<Uuid, WaitRow>,
    deployment: Uuid,
    waiting: i32,
    reason: Option<Pause>,
    ready: bool,
) {
    let row = report.entry(deployment).or_insert(WaitRow {
        deployment,
        waiting_lines: 0,
        reason,
        ready: false,
    });
    row.waiting_lines = row.waiting_lines.saturating_add(waiting.max(0));
    // A route this batch is dispatching to shows no reason.
    if row.reason.is_some() {
        row.reason = reason;
    }
    row.ready |= ready;
}

pub(super) fn spawn_line(
    tasks: &mut tokio::task::JoinSet<()>,
    line: &Arc<LineRun>,
    records: &mpsc::Sender<Record>,
    head: Head,
    pin: Option<Pin>,
    permits: (
        tokio::sync::OwnedSemaphorePermit,
        tokio::sync::OwnedSemaphorePermit,
    ),
) {
    let (line, records) = (line.clone(), records.clone());
    tasks.spawn(async move {
        let _permits = permits;
        line.run(head.n, head.raw, head.execution, pin, records)
            .await;
    });
}
