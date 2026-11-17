use super::*;

// A gateway batch admits at most 50,000 lines with two explicit retries.
// Count execution attempts, including ones missing a reservation, so malformed
// history cannot bypass the resource bound or conceal a missing charge.
const MAX_LINE_ATTEMPTS: i64 = 150_000;

pub(super) async fn bound(
    tx: &mut Transaction<'_, Postgres>,
    initial: &mut Vec<Candidate>,
    more: bool,
) -> Result<(i64, bool), ApiError> {
    let jobs: Vec<_> = initial.iter().map(|r| r.job_id).collect();
    // All jobs precede every reservation. FOR UPDATE excludes runner claims,
    // line claims and admission, keeping the counted attempt set fixed.
    sqlx::query("SELECT id FROM async_jobs WHERE id=ANY($1) ORDER BY id FOR UPDATE")
        .bind(&jobs)
        .execute(&mut **tx)
        .await?;
    // Stop probing once the page's attempt budget is exhausted. The recursive
    // prefix reads at most MAX_LINE_ATTEMPTS+1 attempts across all selected
    // jobs, including the incomplete lookahead for the next whole job.
    let (n, attempts): (i32, i64) = sqlx::query_as(
        "WITH RECURSIVE volume(n,attempts) AS (
         VALUES(0::integer,0::bigint)
         UNION ALL
         SELECT v.n+1,v.attempts+c.n FROM volume v
         CROSS JOIN LATERAL (SELECT count(*) AS n FROM (
          SELECT 1 FROM inference_executions WHERE batch_job_id=($1::uuid[])[v.n+1]
          LIMIT greatest($2-v.attempts+1,0)
         ) limited) c
         WHERE v.n<cardinality($1) AND v.attempts<=$2
        ) SELECT n,attempts FROM volume ORDER BY n DESC LIMIT 1",
    )
    .bind(&jobs)
    .bind(MAX_LINE_ATTEMPTS)
    .fetch_one(&mut **tx)
    .await?;
    if attempts <= MAX_LINE_ATTEMPTS {
        return Ok((i64::from(n), more));
    }
    if n == 1 {
        // Oversized history is ambiguous under the supported admission bounds.
        // Leave it untouched, but let the scan reach later complete jobs.
        let more = more || initial.len() > 1;
        initial.clear();
        return Ok((1, more));
    }
    initial.truncate((n - 1) as usize);
    Ok((i64::from(n - 1), true))
}
