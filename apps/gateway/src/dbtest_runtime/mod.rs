//! Databases for `#[dbtest::test]` (docs/verification.md, "Database tests").
//!
//! Every test gets its own database, as `#[sqlx::test]` gave it, but the
//! schema comes from a copy of a template that the real migrator built once:
//! `CREATE DATABASE ... TEMPLATE` instead of an empty database plus every
//! migration.
//!
//! Invariants:
//! - A template is named after the migrations it holds, so a changed or added
//!   migration is a different template and a stale one is never cloned. The
//!   name also carries the server's UTC hour (migrations seed time-relative
//!   state such as partition coverage and rollup watermarks; a process that
//!   outlives the hour asks for the new hour's template) and the maintenance
//!   database's name
//!   (advisory locks are per database, and the lock below must see every
//!   builder of this name).
//! - A template exists under its final name only when it is complete. It is
//!   built under a temporary name, marked `IS_TEMPLATE` with connections
//!   refused, and renamed last; a process killed earlier leaves only the
//!   temporary name, which the next builder drops. A database with the final
//!   name that is not a template with connections refused is not trusted either.
//! - Builders, garbage collection and lease takers all hold the build lock
//!   (a session advisory lock), so one process builds while the others wait and
//!   then find the finished template. The lock dies with its connection.
//! - Every process holds a shared advisory lock on the template it uses for as
//!   long as it lives (a dedicated thread owns that connection). Collection
//!   drops another template only after taking its exclusive lock, so a
//!   template a live process uses is never dropped.
//! - Test databases are named and recorded exactly as sqlx names and records
//!   them (`_sqlx_test.databases`), so a rerun of the same test replaces its
//!   leftover, and `cargo sqlx database` cleanup still sees them. A passing
//!   test's database is dropped; a failing or panicking test's is kept and its
//!   name printed. Records older than a day whose database nobody is using are
//!   dropped during template setup.

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::str::FromStr;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use futures::FutureExt;
use sha2::{Digest, Sha256};
use sqlx::migrate::Migrator;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::testing::{TestArgs, TestSupport, TestTermination};
use sqlx::{Connection, Executor, PgConnection, PgPool, Postgres, query, query_scalar};

#[cfg(test)]
mod tests;

/// Every database test starts from these migrations.
static ENTERPRISE_MIGRATIONS: Migrator = sqlx::migrate!("./enterprise_migrations");

/// Prefix of the template database names. Anything beginning with it that is
/// not the current template is collectable.
const TEMPLATE_PREFIX: &str = "_omg_tpl";

/// Changes whenever the way a template is built changes, so templates from an
/// older version of this module are not reused.
const TEMPLATE_FORMAT: &str = "dbtest-template-1";

/// Set to anything but `0` to rebuild the template of this process even though
/// a complete one exists (it is kept when another process still uses it).
const REBUILD_ENV: &str = "OMG_DBTEST_REBUILD";

/// Test databases older than this whose connections are gone are dropped.
const LEAK_AGE: &str = "1 day";

/// `CREATE DATABASE ... TEMPLATE` and `ALTER DATABASE ... RENAME` fail with
/// this SQLSTATE while a session is connected to the source or the database.
const OBJECT_IN_USE: &str = "55006";

/// sqlx's own advisory key for its bookkeeping schema ("sqlxtest"), so both
/// serialize the same DDL.
const SQLX_BOOKKEEPING_LOCK: i64 = 8_318_549_251_334_697_844;

/// Run one database test: a database cloned from the process's template, a
/// pool on it, the body, then the database dropped when the body succeeded.
/// The same single-threaded runtime `#[sqlx::test]` used drives it. Futures are
/// boxed so a large test future lives on the heap, not on the test thread's
/// small stack.
pub(crate) fn run<Fut>(test_path: &'static str, test_fn: fn(PgPool) -> Fut) -> Fut::Output
where
    Fut: Future,
    Fut::Output: TestTermination,
{
    sqlx::test_block_on(Box::pin(async move {
        let process = process_template();
        process
            .templates
            .run_test(&process.template, test_path, test_fn)
            .await
    }))
}

struct ProcessTemplate {
    templates: Templates,
    template: String,
}

/// The template this process clones: the current hour's, built (or found) on
/// first use and again when the hour changes under a long run. A thread that
/// outlives every test runtime owns each lease connection, so the shared locks
/// last until the process exits.
fn process_template() -> &'static ProcessTemplate {
    type Prepared = Result<&'static ProcessTemplate, &'static str>;
    static CURRENT: Mutex<Option<(u64, Prepared)>> = Mutex::new(None);
    let hour = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() / 3600);
    // A failed setup is remembered like a finished one: every test of the
    // hour fails at once with the same reason instead of each repeating the
    // build (a broken migration would otherwise cost a build per test).
    let prepared = {
        let mut current = CURRENT.lock().unwrap_or_else(PoisonError::into_inner);
        match *current {
            Some((built_for, prepared)) if built_for == hour => prepared,
            _ => {
                let prepared: Prepared = match prepare_process_template() {
                    Ok(template) => Ok(Box::leak(Box::new(template))),
                    Err(error) => Err(Box::leak(error.into_boxed_str())),
                };
                *current = Some((hour, prepared));
                prepared
            }
        }
    };
    prepared.unwrap_or_else(|error| panic!("cannot prepare the template database: {error}"))
}

fn prepare_process_template() -> Result<ProcessTemplate, String> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("dbtest-template".into())
        .spawn(move || {
            sqlx::test_block_on(async move {
                let templates = Templates::from_env(TEMPLATE_PREFIX, &ENTERPRISE_MIGRATIONS);
                match templates.ensure().await {
                    Ok(template) => {
                        let name = template.name.clone();
                        let _ = sender.send(Ok(ProcessTemplate {
                            templates,
                            template: name,
                        }));
                        let _lease = template;
                        std::future::pending::<()>().await;
                    }
                    Err(error) => {
                        let _ = sender.send(Err(error.to_string()));
                    }
                }
            });
        })
        .map_err(|error| format!("spawn the template thread: {error}"))?;
    receiver
        .recv()
        .unwrap_or_else(|_| Err("the template thread ended without a result".into()))
}

/// A complete template and the connection that keeps it from being collected.
pub(crate) struct Template {
    pub(crate) name: String,
    /// This call built it (false: it was already complete).
    pub(crate) built: bool,
    _lease: PgConnection,
}

/// One family of templates on one server: the maintenance URL, the name
/// prefix and the migrations a template holds. Tests of this module use their
/// own prefix and synthetic migrations.
#[derive(Clone)]
pub(crate) struct Templates {
    url: String,
    prefix: String,
    migrator: &'static Migrator,
}

impl Templates {
    pub(crate) fn from_env(prefix: &str, migrator: &'static Migrator) -> Self {
        let url = dotenvy::var("DATABASE_URL").expect("DATABASE_URL must be set");
        Self::new(url, prefix, migrator)
    }

    pub(crate) fn new(url: String, prefix: &str, migrator: &'static Migrator) -> Self {
        Self {
            url,
            prefix: prefix.to_owned(),
            migrator,
        }
    }

    fn options(&self) -> PgConnectOptions {
        PgConnectOptions::from_str(&self.url).expect("failed to parse DATABASE_URL")
    }

    async fn maintenance(&self) -> Result<PgConnection, sqlx::Error> {
        PgConnection::connect_with(&self.options()).await
    }

    /// The template's name: prefix, then the digest of the format, the
    /// maintenance database, the server's UTC hour and every migration's
    /// version, description, kind, transaction mode and checksum (the checksum
    /// covers the SQL). The hour is part of it because migrations seed state
    /// relative to the moment they run (the history partitions cover the
    /// months from then on with no default partition, and the rollup and
    /// storage watermarks start at the current hour): a template is at most an
    /// hour older than the database a test would have migrated itself.
    pub(crate) async fn template_name(
        &self,
        conn: &mut PgConnection,
    ) -> Result<String, sqlx::Error> {
        let (database, hour): (String, String) = sqlx::query_as(
            "select current_database()::text, to_char(now() at time zone 'utc', 'YYYY-MM-DD\"T\"HH24')",
        )
        .fetch_one(&mut *conn)
        .await?;
        let mut digest = Sha256::new();
        for part in [TEMPLATE_FORMAT, database.as_str(), hour.as_str()] {
            digest.update((part.len() as u64).to_be_bytes());
            digest.update(part.as_bytes());
        }
        digest.update([
            u8::from(self.migrator.ignore_missing),
            u8::from(self.migrator.locking),
            u8::from(self.migrator.no_tx),
        ]);
        for migration in self.migrator.iter() {
            digest.update(migration.version.to_be_bytes());
            digest.update((migration.description.len() as u64).to_be_bytes());
            digest.update(migration.description.as_bytes());
            digest.update(format!("{:?}", migration.migration_type).as_bytes());
            digest.update([u8::from(migration.no_tx)]);
            digest.update((migration.checksum.len() as u64).to_be_bytes());
            digest.update(&*migration.checksum);
        }
        let digest = hex::encode(digest.finalize());
        // The family (everything before the last underscore) names the
        // maintenance database too: collection lists one family, and its
        // leases are advisory locks that only this database's sessions see.
        let family = hex::encode(Sha256::digest(database.as_bytes()));
        Ok(format!(
            "{}_{}_{}",
            self.prefix,
            &family[..8],
            &digest[..24]
        ))
    }

    /// Return the complete template for these migrations, building it first
    /// when no process has, and collect templates nobody uses any more.
    pub(crate) async fn ensure(&self) -> Result<Template, sqlx::Error> {
        let mut conn = self.maintenance().await?;
        let name = self.template_name(&mut conn).await?;
        let build_key = lock_key(&format!("build:{}", self.prefix));
        query("select pg_advisory_lock($1)")
            .bind(build_key)
            .execute(&mut conn)
            .await?;
        let built = self.ensure_locked(&mut conn, &name).await?;
        self.collect_templates(&mut conn, &name).await?;
        self.collect_test_databases(&mut conn).await?;
        query("select pg_advisory_unlock($1)")
            .bind(build_key)
            .execute(&mut conn)
            .await?;
        Ok(Template {
            name,
            built,
            _lease: conn,
        })
    }

    /// With the build lock held: make `name` complete and take the shared use
    /// lock on it. Returns whether this call built it.
    async fn ensure_locked(
        &self,
        conn: &mut PgConnection,
        name: &str,
    ) -> Result<bool, sqlx::Error> {
        let use_key = lock_key(name);
        let mut complete = is_complete_template(conn, name).await?;
        if complete && rebuild_requested() {
            let exclusive: bool = query_scalar("select pg_try_advisory_lock($1)")
                .bind(use_key)
                .fetch_one(&mut *conn)
                .await?;
            if exclusive {
                query("select pg_advisory_unlock($1)")
                    .bind(use_key)
                    .execute(&mut *conn)
                    .await?;
                complete = false;
            } else {
                eprintln!("{REBUILD_ENV}: template {name} is in use, keeping it");
            }
        }
        if !complete {
            self.build(conn, name).await?;
        }
        query("select pg_advisory_lock_shared($1)")
            .bind(use_key)
            .execute(&mut *conn)
            .await?;
        Ok(!complete)
    }

    /// Build under `<name>_b`, run the migrator, mark it, then rename.
    async fn build(&self, conn: &mut PgConnection, name: &str) -> Result<(), sqlx::Error> {
        let scratch = format!("{name}_b");
        drop_database(conn, name, false).await?;
        drop_database(conn, &scratch, false).await?;
        conn.execute(&*format!("create database {}", quote(&scratch)))
            .await?;
        let mut builder = PgConnection::connect_with(&self.options().database(&scratch)).await?;
        self.migrator.run_direct(&mut builder).await?;
        let applied: i64 = query_scalar("select count(*) from _sqlx_migrations where success")
            .fetch_one(&mut builder)
            .await?;
        assert_eq!(
            usize::try_from(applied).ok(),
            Some(self.migrator.migrations.len()),
            "the template did not record every migration"
        );
        builder.close().await?;
        conn.execute(&*format!(
            "alter database {} with is_template true allow_connections false",
            quote(&scratch)
        ))
        .await?;
        execute_retrying(
            conn,
            &format!(
                "alter database {} rename to {}",
                quote(&scratch),
                quote(name)
            ),
        )
        .await
    }

    /// Drop every other database of this family that no process leases,
    /// including a scratch build a killed process left (builds happen only
    /// under the build lock this runs under). A lease is a shared advisory
    /// lock, so the exclusive try fails exactly while a live process uses the
    /// template. The family is the prefix and this maintenance database:
    /// templates built through another maintenance database have leases this
    /// session cannot see, and are left to their own processes. No database
    /// of another purpose may begin with the family and an underscore.
    async fn collect_templates(
        &self,
        conn: &mut PgConnection,
        keep: &str,
    ) -> Result<(), sqlx::Error> {
        let others: Vec<String> = query_scalar(
            "select datname::text from pg_database \
             where starts_with(datname, $1) and datname <> $2 \
             order by datname",
        )
        .bind(format!("{}_", family_of(keep)))
        .bind(keep)
        .fetch_all(&mut *conn)
        .await?;
        for other in others {
            let key = lock_key(&other);
            let free: bool = query_scalar("select pg_try_advisory_lock($1)")
                .bind(key)
                .fetch_one(&mut *conn)
                .await?;
            if !free {
                continue;
            }
            let dropped = drop_database(conn, &other, false).await;
            query("select pg_advisory_unlock($1)")
                .bind(key)
                .execute(&mut *conn)
                .await?;
            if let Err(error) = dropped {
                eprintln!("could not collect template {other}: {error}");
            }
        }
        Ok(())
    }

    /// Drop recorded test databases left by runs that died, once they are old
    /// enough that no test can still be using them. A database that still has
    /// connections refuses to drop and is left alone.
    async fn collect_test_databases(&self, conn: &mut PgConnection) -> Result<(), sqlx::Error> {
        ensure_bookkeeping(conn).await?;
        let stale: Vec<String> = query_scalar(&format!(
            "select db_name from _sqlx_test.databases where created_at < now() - interval '{LEAK_AGE}'"
        ))
        .fetch_all(&mut *conn)
        .await?;
        for db_name in stale {
            match conn
                .execute(&*format!("drop database if exists {}", quote(&db_name)))
                .await
            {
                Ok(_) => {
                    query("delete from _sqlx_test.databases where db_name = $1")
                        .bind(&db_name)
                        .execute(&mut *conn)
                        .await?;
                }
                Err(error) => eprintln!("could not clean test database {db_name}: {error}"),
            }
        }
        Ok(())
    }

    async fn run_test<Fut>(
        &self,
        template: &str,
        test_path: &'static str,
        test_fn: fn(PgPool) -> Fut,
    ) -> Fut::Output
    where
        Fut: Future,
        Fut::Output: TestTermination,
    {
        let (db_name, created) = self
            .create_owned(template, test_path)
            .await
            .expect("failed to create the test database");
        let pool = PgPoolOptions::new()
            // A single test should not need more; the server's limit is shared by every test thread.
            .max_connections(5)
            // Idle connections would keep the database from being dropped.
            .idle_timeout(Some(Duration::from_secs(1)))
            // Just-in-time compilation only costs a test here: its queries run once, on a few rows.
            .connect_with(self.options().database(&db_name).options([("jit", "off")]))
            .await
            .expect("failed to connect the test pool");
        let outcome = AssertUnwindSafe(Box::pin(test_fn(pool.clone())))
            .catch_unwind()
            .await;
        if tokio::time::timeout(Duration::from_secs(10), pool.close())
            .await
            .is_err()
        {
            eprintln!("test {test_path} held onto Pool after exiting");
        }
        match outcome {
            Ok(result) if result.is_success() => {
                if let Err(error) = self.drop_owned(&db_name, created).await {
                    eprintln!("failed to delete database {db_name:?}: {error}");
                }
                result
            }
            Ok(result) => {
                eprintln!("test database {db_name} kept for inspection ({test_path})");
                result
            }
            Err(panic) => {
                eprintln!("test database {db_name} kept for inspection ({test_path})");
                std::panic::resume_unwind(panic)
            }
        }
    }

    /// Clone the template into the database named for the test, recorded as
    /// sqlx records it, and return its name. A leftover of the same test is
    /// replaced. When something still uses the leftover (another process
    /// running the same test against this server), the test gets a name of its
    /// own instead of dropping a database that is in use.
    /// `ensure` has created the bookkeeping schema by the time a template exists.
    pub(crate) async fn create_test_database(
        &self,
        template: &str,
        test_path: &'static str,
    ) -> Result<String, sqlx::Error> {
        Ok(self.create_owned(template, test_path).await?.0)
    }

    /// [`Self::create_test_database`], with the new database's OID: the
    /// identity of this creation, which a later database of the same name
    /// does not share.
    async fn create_owned(
        &self,
        template: &str,
        test_path: &'static str,
    ) -> Result<(String, sqlx::postgres::types::Oid), sqlx::Error> {
        let mut db_name = <Postgres as TestSupport>::db_name(&TestArgs::new(test_path));
        let mut conn = self.maintenance().await?;
        match remove_test_database(&mut conn, &db_name, false).await {
            Ok(()) => {}
            Err(sqlx::Error::Database(error)) if error.code().as_deref() == Some(OBJECT_IN_USE) => {
                db_name = format!(
                    "_sqlx_test_p{}_{}",
                    std::process::id(),
                    &hex::encode(Sha256::digest(test_path.as_bytes()))[..24]
                );
                remove_test_database(&mut conn, &db_name, false).await?;
            }
            Err(error) => return Err(error),
        }
        query("insert into _sqlx_test.databases(db_name, test_path) values ($1, $2)")
            .bind(&db_name)
            .bind(test_path)
            .execute(&mut conn)
            .await?;
        let created = execute_retrying(
            &mut conn,
            &format!(
                "create database {} template {}",
                quote(&db_name),
                quote(template)
            ),
        )
        .await;
        let oid = match created {
            Ok(()) => {
                query_scalar("select oid from pg_database where datname = $1")
                    .bind(&db_name)
                    .fetch_one(&mut conn)
                    .await
            }
            Err(error) => Err(error),
        };
        let _ = conn.close().await;
        oid.map(|oid| (db_name, oid))
    }

    /// Drop a passed test's database when it is still the one this test
    /// created. Once the test's pool is closed, another process running the
    /// same test may replace the database under the same name; a forced drop
    /// by name would then end that process's test.
    async fn drop_owned(
        &self,
        db_name: &str,
        created: sqlx::postgres::types::Oid,
    ) -> Result<(), sqlx::Error> {
        let mut conn = self.maintenance().await?;
        let current: Option<sqlx::postgres::types::Oid> =
            query_scalar("select oid from pg_database where datname = $1")
                .bind(db_name)
                .fetch_optional(&mut conn)
                .await?;
        let removed = if current == Some(created) {
            remove_test_database(&mut conn, db_name, true).await
        } else {
            Ok(())
        };
        let _ = conn.close().await;
        removed
    }

    /// Drop a passed test's database. `force` ends sessions a detached task
    /// of the test may still hold.
    pub(crate) async fn drop_test_database(&self, db_name: &str) -> Result<(), sqlx::Error> {
        let mut conn = self.maintenance().await?;
        let removed = remove_test_database(&mut conn, db_name, true).await;
        let _ = conn.close().await;
        removed
    }

    pub(crate) fn database_options(&self, db_name: &str) -> PgConnectOptions {
        self.options().database(db_name)
    }

    /// A maintenance connection (for tests of this module).
    pub(crate) async fn maintenance_connection(&self) -> Result<PgConnection, sqlx::Error> {
        self.maintenance().await
    }
}

fn rebuild_requested() -> bool {
    std::env::var(REBUILD_ENV).is_ok_and(|value| !value.is_empty() && value != "0")
}

/// A template is complete only as a marked template that refuses connections:
/// the state the builder sets just before the rename.
async fn is_complete_template(conn: &mut PgConnection, name: &str) -> Result<bool, sqlx::Error> {
    query_scalar(
        "select exists(select 1 from pg_database \
         where datname = $1 and datistemplate and not datallowconn)",
    )
    .bind(name)
    .fetch_one(conn)
    .await
}

/// Drop `name` when it exists, demoting a template first.
async fn drop_database(
    conn: &mut PgConnection,
    name: &str,
    force: bool,
) -> Result<(), sqlx::Error> {
    let exists: bool = query_scalar("select exists(select 1 from pg_database where datname = $1)")
        .bind(name)
        .fetch_one(&mut *conn)
        .await?;
    if !exists {
        return Ok(());
    }
    conn.execute(&*format!(
        "alter database {} with is_template false allow_connections true",
        quote(name)
    ))
    .await?;
    let with = if force { " with (force)" } else { "" };
    conn.execute(&*format!("drop database if exists {}{with}", quote(name)))
        .await?;
    Ok(())
}

async fn remove_test_database(
    conn: &mut PgConnection,
    db_name: &str,
    force: bool,
) -> Result<(), sqlx::Error> {
    let with = if force { " with (force)" } else { "" };
    conn.execute(&*format!(
        "drop database if exists {}{with}",
        quote(db_name)
    ))
    .await?;
    query("delete from _sqlx_test.databases where db_name = $1")
        .bind(db_name)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// The same bookkeeping schema `#[sqlx::test]` creates, under its lock.
async fn ensure_bookkeeping(conn: &mut PgConnection) -> Result<(), sqlx::Error> {
    conn.execute(&*format!(
        "select pg_advisory_xact_lock({SQLX_BOOKKEEPING_LOCK}); \
         create schema if not exists _sqlx_test; \
         create table if not exists _sqlx_test.databases (\
             db_name text primary key, \
             test_path text not null, \
             created_at timestamptz not null default now()); \
         create index if not exists databases_created_at on _sqlx_test.databases(created_at); \
         create sequence if not exists _sqlx_test.database_ids;"
    ))
    .await?;
    Ok(())
}

/// Run one statement, retrying while a session that is closing still counts
/// as connected to the database it touches.
async fn execute_retrying(conn: &mut PgConnection, sql: &str) -> Result<(), sqlx::Error> {
    let mut attempts = 0;
    loop {
        match conn.execute(sql).await {
            Ok(_) => return Ok(()),
            Err(sqlx::Error::Database(error))
                if error.code().as_deref() == Some(OBJECT_IN_USE) && attempts < 200 =>
            {
                attempts += 1;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

/// The family of a template name: everything before its migration digest.
fn family_of(template: &str) -> &str {
    template
        .rsplit_once('_')
        .map_or(template, |(family, _)| family)
}

/// The advisory-lock key of a name.
fn lock_key(name: &str) -> i64 {
    let digest = Sha256::digest(name.as_bytes());
    i64::from_be_bytes(digest[..8].try_into().expect("eight bytes"))
}

/// A quoted SQL identifier.
fn quote(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}
