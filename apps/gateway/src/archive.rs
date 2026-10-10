//! Operator-only archival of closed history months (migration 0033).
//!
//! `open-model-gateway archive partition <group> <YYYY-MM|legacy> --to DIR`
//! runs with the schema owner's credentials (DETACH and DROP require table
//! ownership; the runtime role has neither). In ONE transaction it:
//!   1. SHARE locks the month's partitions of every table in the group (writes
//!      to those partitions wait; admissions write the current month and
//!      continue) and refuses unless the month ended at least the retention
//!      before the current month, and, for `history`, no reservation in it
//!      is pending or unknown, no execution lacks a reservation or is still
//!      running, and every hour with history has a clean usage rollup;
//!   2. exports every partition with COPY (CSV with header) into a new
//!      directory, with a SHA-256 per file and a manifest (format
//!      `omg-archive-v1`, the schema lineage and exact sums) plus the
//!      manifest's own SHA-256, like `scripts/backup.py`;
//!   3. records `archived_partitions` and, for history, the archived
//!      reservations' budget contributions (`budget verify` adds them);
//!   4. takes the parents' ACCESS EXCLUSIVE locks in admission order
//!      (bounded by lock_timeout) and DETACHes the partitions (ledger,
//!      reservations, executions), then moves them to schema `omg_archive`
//!      or, with `--drop`, drops them.
//!
//! If anything fails, the transaction rolls back and the export directory is
//! removed: nothing is detached or recorded. Nothing else is modified.
use std::path::{Path, PathBuf};

use anyhow::Context;
use chrono::{DateTime, Datelike, Months, NaiveDate, TimeZone, Utc};
use futures::StreamExt;
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

pub const MANIFEST_FORMAT: &str = "omg-archive-v1";
/// Default hot retention in months (`GATEWAY_HISTORY_RETENTION_MONTHS`).
pub const DEFAULT_RETENTION_MONTHS: u32 = 25;
const RETENTION_VAR: &str = "GATEWAY_HISTORY_RETENTION_MONTHS";

pub fn retention_from_env() -> anyhow::Result<u32> {
    match std::env::var(RETENTION_VAR) {
        Err(std::env::VarError::NotPresent) => Ok(DEFAULT_RETENTION_MONTHS),
        Ok(v) => {
            let n: u32 = v
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid {RETENTION_VAR}"))?;
            anyhow::ensure!((1..=1200).contains(&n), "{RETENTION_VAR} must be 1..1200");
            Ok(n)
        }
        Err(_) => anyhow::bail!("invalid {RETENTION_VAR}"),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Group {
    /// Executions, reservations and ledger (archived together: the ledger
    /// references reservations, reservations reference executions).
    History,
    Audit,
    Storage,
}
impl Group {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "history" => Self::History,
            "audit" => Self::Audit,
            "storage" => Self::Storage,
            _ => return None,
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::History => "history",
            Self::Audit => "audit",
            Self::Storage => "storage",
        }
    }
    /// Parents in detach order (referencing tables first).
    fn parents(self) -> &'static [&'static str] {
        match self {
            Self::History => &[
                "monetary_ledger",
                "governance_reservations",
                "inference_executions",
            ],
            Self::Audit => &["audit_events"],
            Self::Storage => &["storage_usage_hours"],
        }
    }
}

/// A month `YYYY-MM` or the pre-partitioning `legacy` partition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Month {
    Month(NaiveDate),
    Legacy,
}
impl Month {
    pub fn parse(s: &str) -> Option<Self> {
        if s == "legacy" {
            return Some(Self::Legacy);
        }
        let (y, m) = s.split_once('-')?;
        if y.len() != 4 || m.len() != 2 {
            return None;
        }
        NaiveDate::from_ymd_opt(y.parse().ok()?, m.parse().ok()?, 1).map(Self::Month)
    }
    fn label(self) -> String {
        match self {
            Self::Month(d) => format!("{:04}-{:02}", d.year(), d.month()),
            Self::Legacy => "legacy".into(),
        }
    }
    fn suffix(self) -> String {
        match self {
            Self::Month(d) => format!("p{:04}_{:02}", d.year(), d.month()),
            Self::Legacy => "p_legacy".into(),
        }
    }
}

pub struct Request {
    pub group: Group,
    pub month: Month,
    pub to: PathBuf,
    pub drop: bool,
    pub retention_months: u32,
}

#[derive(Clone, Debug, Serialize)]
pub struct ArchivedFile {
    pub parent: String,
    pub partition: String,
    pub file: String,
    pub rows: i64,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub archive_id: Uuid,
    pub group: Group,
    pub month: String,
    pub directory: PathBuf,
    pub manifest_sha256: String,
    pub files: Vec<ArchivedFile>,
    pub disposition: &'static str,
}

/// Why an archive was refused (nothing changed).
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    Missing(String, String),
    WithinRetention(String, u32),
    OpenReservations(i64),
    OpenExecutions(i64),
    RollupsIncomplete(i64),
    AlreadyArchived(String),
}
impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(t, m) => write!(f, "{t} has no partition for {m}"),
            Self::WithinRetention(m, r) => write!(f, "month {m} is within the {r}-month retention"),
            Self::OpenReservations(n) => write!(
                f,
                "{n} reservation(s) of the month are pending or unknown; their holds must settle first"
            ),
            Self::OpenExecutions(n) => write!(
                f,
                "{n} execution(s) of the month are running or have no reservation (unknown cost)"
            ),
            Self::RollupsIncomplete(n) => write!(
                f,
                "{n} hour(s) of the month have no clean usage rollup yet; wait for the rollups job"
            ),
            Self::AlreadyArchived(m) => write!(f, "month {m} is already archived"),
        }
    }
}
impl std::error::Error for Refusal {}

struct Partition {
    parent: &'static str,
    name: String,
    /// `None`: the legacy partition (MINVALUE).
    lower: Option<DateTime<Utc>>,
    upper: DateTime<Utc>,
}

fn utc(d: NaiveDate) -> DateTime<Utc> {
    Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).expect("midnight"))
}

/// Archive one month of `group`. See the module documentation.
pub async fn archive(pool: &PgPool, req: &Request) -> anyhow::Result<Report> {
    archive_at(pool, req, None).await
}

/// [`archive`] judging retention as of `now` instead of the database clock
/// (tests: months after the legacy partition are always in the future).
#[cfg(any(test, feature = "integration-tests"))]
pub async fn archive_as_of(
    pool: &PgPool,
    req: &Request,
    now: DateTime<Utc>,
) -> anyhow::Result<Report> {
    archive_at(pool, req, Some(now)).await
}

async fn archive_at(
    pool: &PgPool,
    req: &Request,
    as_of: Option<DateTime<Utc>>,
) -> anyhow::Result<Report> {
    let id = Uuid::now_v7();
    let dir = req
        .to
        .join(format!("{}-{}-{id}", req.group.as_str(), req.month.label()));
    std::fs::create_dir_all(&req.to).context("creating the archive directory")?;
    std::fs::create_dir(&dir).context("creating the archive directory")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let mut conn = pool.acquire().await?.detach();
    let result = archive_in(&mut conn, req, id, &dir, as_of).await;
    if result.is_err() {
        let _ = sqlx::raw_sql("ROLLBACK").execute(&mut conn).await;
        let _ = std::fs::remove_dir_all(&dir);
    }
    use sqlx::Connection;
    let _ = conn.close().await;
    result
}

async fn archive_in(
    conn: &mut PgConnection,
    req: &Request,
    id: Uuid,
    dir: &Path,
    as_of: Option<DateTime<Utc>>,
) -> anyhow::Result<Report> {
    let month = req.month.label();
    // Locate the partitions before the transaction: its snapshot must be
    // taken only after their writers are locked out.
    let mut parts = Vec::new();
    for parent in req.group.parents() {
        let name = format!("{parent}_{}", req.month.suffix());
        let bounds: Option<(Option<DateTime<Utc>>, DateTime<Utc>)> = sqlx::query_as(
            "SELECT CASE WHEN isfinite(lower_bound) THEN lower_bound END,upper_bound FROM omg_partition_bounds($1) WHERE partition=$2",
        )
        .bind(parent)
        .bind(&name)
        .fetch_optional(&mut *conn)
        .await?;
        let Some((lower, upper)) = bounds else {
            return Err(Refusal::Missing((*parent).into(), month).into());
        };
        if let Month::Month(d) = req.month
            && (lower != Some(utc(d)) || upper != utc(d + Months::new(1)))
        {
            return Err(Refusal::Missing((*parent).into(), month).into());
        }
        parts.push(Partition {
            parent,
            name,
            lower,
            upper,
        });
    }
    // Writes to these partitions wait; everything else continues. LOCK
    // takes no snapshot, so the export below sees every committed row.
    let list = parts
        .iter()
        .rev()
        .map(|p| format!("public.{}", p.name))
        .collect::<Vec<_>>()
        .join(",");
    sqlx::raw_sql(&format!(
        "BEGIN ISOLATION LEVEL REPEATABLE READ; SET LOCAL statement_timeout=0; SET LOCAL lock_timeout='10s'; LOCK TABLE {list} IN SHARE MODE"
    ))
    .execute(&mut *conn)
    .await?;
    let already: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM archived_partitions WHERE archive_group=$1 AND month=$2)",
    )
    .bind(req.group.as_str())
    .bind(&month)
    .fetch_one(&mut *conn)
    .await?;
    if already {
        return Err(Refusal::AlreadyArchived(month).into());
    }
    for p in &parts {
        let attached: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM omg_partition_bounds($1) WHERE partition=$2)",
        )
        .bind(p.parent)
        .bind(&p.name)
        .fetch_one(&mut *conn)
        .await?;
        if !attached {
            return Err(Refusal::Missing(p.parent.into(), month).into());
        }
    }
    let now: DateTime<Utc> = match as_of {
        Some(at) => at,
        None => {
            sqlx::query_scalar("SELECT clock_timestamp()")
                .fetch_one(&mut *conn)
                .await?
        }
    };
    let current = NaiveDate::from_ymd_opt(now.year(), now.month(), 1).expect("month");
    let limit = utc(current - Months::new(req.retention_months));
    if parts.iter().any(|p| p.upper > limit) {
        return Err(Refusal::WithinRetention(month, req.retention_months).into());
    }
    let mut sums = serde_json::Value::Null;
    if req.group == Group::History {
        let open: i64 = sqlx::query_scalar(&format!(
            "SELECT count(*) FROM public.governance_reservations_{} WHERE state IN ('pending','unknown')",
            req.month.suffix()
        ))
        .fetch_one(&mut *conn)
        .await?;
        if open > 0 {
            return Err(Refusal::OpenReservations(open).into());
        }
        let s = req.month.suffix();
        let unreserved: i64 = sqlx::query_scalar(&format!(
            "SELECT count(*) FROM public.inference_executions_{s} e WHERE e.state='started'
              OR NOT EXISTS(SELECT 1 FROM public.governance_reservations_{s} r WHERE r.execution_id=e.id AND r.admitted_at=e.started_at)"
        ))
        .fetch_one(&mut *conn)
        .await?;
        if unreserved > 0 {
            return Err(Refusal::OpenExecutions(unreserved).into());
        }
        let unrolled: i64 = sqlx::query_scalar(&format!(
            "SELECT count(*) FROM (SELECT DISTINCT date_trunc('hour',started_at,'UTC') h FROM public.inference_executions_{s}) x
              WHERE NOT EXISTS(SELECT 1 FROM usage_rollup_hours u WHERE u.hour_start=x.h)
                 OR EXISTS(SELECT 1 FROM usage_rollup_dirty d WHERE d.hour_start=x.h)"
        ))
        .fetch_one(&mut *conn)
        .await?;
        if unrolled > 0 {
            return Err(Refusal::RollupsIncomplete(unrolled).into());
        }
        let (settled, reservations, ledger_entries, ledger_microusd): (String, i64, i64, String) =
            sqlx::query_as(&format!(
                "SELECT (SELECT coalesce(sum(actual_microusd),0)::text FROM public.governance_reservations_{s}),
                        (SELECT count(*) FROM public.governance_reservations_{s}),
                        (SELECT count(*) FROM public.monetary_ledger_{s}),
                        (SELECT coalesce(sum(amount_microusd),0)::text FROM public.monetary_ledger_{s})"
            ))
            .fetch_one(&mut *conn)
            .await?;
        sums = serde_json::json!({
            "settled_microusd": settled, "reservations": reservations,
            "ledger_entries": ledger_entries, "ledger_microusd": ledger_microusd,
        });
    }
    // Export (same snapshot): one CSV per partition, checksummed.
    let mut files = Vec::new();
    for p in &parts {
        let file = format!("{}.csv", p.name);
        let path = dir.join(&file);
        let mut out = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
            .context("creating an export file")?;
        let mut digest = Sha256::new();
        let mut bytes = 0u64;
        let rows: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM public.{}", p.name))
            .fetch_one(&mut *conn)
            .await?;
        {
            let mut stream = conn
                .copy_out_raw(&format!(
                    "COPY (SELECT * FROM public.{} ORDER BY 1) TO STDOUT WITH (FORMAT csv, HEADER)",
                    p.name
                ))
                .await?;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                digest.update(&chunk);
                bytes += chunk.len() as u64;
                out.write_all(&chunk).await?;
            }
        }
        out.sync_all().await?;
        files.push(ArchivedFile {
            parent: p.parent.into(),
            partition: p.name.clone(),
            file,
            rows,
            bytes,
            sha256: hex::encode(digest.finalize()),
        });
    }
    let lineage: Vec<(i64, Vec<u8>)> =
        sqlx::query_as("SELECT version,checksum FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&mut *conn)
            .await?;
    let (lower, upper) = (parts[0].lower, parts[0].upper);
    let disposition = if req.drop { "dropped" } else { "detached" };
    let manifest = serde_json::json!({
        "format": MANIFEST_FORMAT,
        "archive_id": id,
        "schema_family": "enterprise_v1",
        "group": req.group,
        "month": month,
        "lower_bound": lower,
        "upper_bound": upper,
        "migrations": lineage.iter().map(|(v, c)| serde_json::json!({"version": v, "checksum": hex::encode(c)})).collect::<Vec<_>>(),
        "files": files,
        "sums": sums,
        "disposition": disposition,
        "archived_at": now,
    });
    let text = serde_json::to_string_pretty(&manifest)? + "\n";
    let manifest_sha256 = hex::encode(Sha256::digest(text.as_bytes()));
    write_private(&dir.join("manifest.json"), text.as_bytes())?;
    write_private(
        &dir.join("manifest.json.sha256"),
        format!("{manifest_sha256}  manifest.json\n").as_bytes(),
    )?;
    // Record, then detach (the exclusive parent locks are the last step).
    sqlx::query("INSERT INTO archived_partitions(id,archive_group,month,lower_bound,upper_bound,partitions,manifest_sha256,settled_microusd,reservations,ledger_entries,ledger_microusd,disposition) VALUES($1,$2,$3,coalesce($4,'-infinity'::timestamptz),$5,$6,$7,($8::jsonb->>'settled_microusd')::numeric,($8::jsonb->>'reservations')::bigint,($8::jsonb->>'ledger_entries')::bigint,($8::jsonb->>'ledger_microusd')::numeric,$9)")
        .bind(id).bind(req.group.as_str()).bind(&month)
        .bind(lower)
        .bind(upper).bind(serde_json::to_value(&files)?).bind(&manifest_sha256).bind(&sums).bind(disposition)
        .execute(&mut *conn).await?;
    if req.group == Group::History {
        sqlx::query(&format!(
            "INSERT INTO archived_budget_contributions(archive_id,workspace_id,api_key_id,day_start,settled_microusd,reservations)
             SELECT $1,workspace_id,api_key_id,date_trunc('day',admitted_at,'UTC'),sum(coalesce(actual_microusd,0)),count(*)
             FROM public.governance_reservations_{} GROUP BY 2,3,4",
            req.month.suffix()
        ))
        .bind(id)
        .execute(&mut *conn)
        .await?;
    }
    let parents = req
        .group
        .parents()
        .iter()
        .rev()
        .map(|p| format!("public.{p}"))
        .collect::<Vec<_>>()
        .join(",");
    sqlx::raw_sql(&format!("LOCK TABLE {parents} IN ACCESS EXCLUSIVE MODE"))
        .execute(&mut *conn)
        .await?;
    for p in &parts {
        sqlx::raw_sql(&format!(
            "ALTER TABLE public.{} DETACH PARTITION public.{}",
            p.parent, p.name
        ))
        .execute(&mut *conn)
        .await?;
        // The detached copy no longer constrains (or is constrained by) live history.
        let fks: Vec<String> = sqlx::query_scalar(&format!(
            "SELECT conname FROM pg_constraint WHERE conrelid='public.{}'::regclass AND contype='f' AND conparentid=0",
            p.name
        ))
        .fetch_all(&mut *conn)
        .await?;
        for fk in fks {
            sqlx::raw_sql(&format!(
                "ALTER TABLE public.{} DROP CONSTRAINT IF EXISTS \"{}\"",
                p.name,
                fk.replace('"', "\"\"")
            ))
            .execute(&mut *conn)
            .await?;
        }
        if req.drop {
            sqlx::raw_sql(&format!("DROP TABLE public.{}", p.name))
                .execute(&mut *conn)
                .await?;
        } else {
            sqlx::raw_sql(&format!(
                "ALTER TABLE public.{} SET SCHEMA omg_archive",
                p.name
            ))
            .execute(&mut *conn)
            .await?;
        }
    }
    sqlx::raw_sql("COMMIT").execute(&mut *conn).await?;
    Ok(Report {
        archive_id: id,
        group: req.group,
        month,
        directory: dir.to_path_buf(),
        manifest_sha256,
        files,
        disposition,
    })
}

fn write_private(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut f = options.open(path)?;
    f.write_all(data)?;
    f.sync_all()?;
    Ok(())
}

/// Re-check an archive directory: the manifest's checksum and every file's
/// size and SHA-256 (read-only; no database).
pub fn verify_directory(dir: &Path) -> anyhow::Result<serde_json::Value> {
    let text = std::fs::read(dir.join("manifest.json")).context("reading manifest.json")?;
    let recorded = std::fs::read_to_string(dir.join("manifest.json.sha256"))
        .context("reading manifest.json.sha256")?;
    let digest = hex::encode(Sha256::digest(&text));
    anyhow::ensure!(
        recorded.split_whitespace().next() == Some(digest.as_str()),
        "manifest checksum does not match"
    );
    let manifest: serde_json::Value = serde_json::from_slice(&text)?;
    anyhow::ensure!(
        manifest["format"] == MANIFEST_FORMAT,
        "unsupported archive format"
    );
    for f in manifest["files"].as_array().into_iter().flatten() {
        let name = f["file"].as_str().unwrap_or_default();
        anyhow::ensure!(
            !name.is_empty() && !name.contains('/') && !name.contains(".."),
            "invalid file name in manifest"
        );
        let data = std::fs::read(dir.join(name)).with_context(|| format!("reading {name}"))?;
        anyhow::ensure!(
            data.len() as u64 == f["bytes"].as_u64().unwrap_or(u64::MAX)
                && hex::encode(Sha256::digest(&data)) == f["sha256"].as_str().unwrap_or_default(),
            "{name} does not match the manifest"
        );
    }
    Ok(manifest)
}

#[cfg(all(test, feature = "integration-tests"))]
#[path = "archive/tests.rs"]
mod tests;
