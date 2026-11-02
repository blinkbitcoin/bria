use std::{
    future::Future,
    io,
    ops::{Add, AddAssign},
    sync::{
        atomic::{AtomicU64, AtomicU8, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use sqlx::PgPool;
use tokio::task::JoinHandle;
use tracing::{field, Instrument, Span};

use crate::primitives::*;

const WATCHDOG_INTERVAL: Duration = Duration::from_secs(60);
const PG_SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(10);
const PG_SNAPSHOT_SQL: &str = "SELECT state, wait_event_type, wait_event, \
     (EXTRACT(EPOCH FROM now() - state_change) * 1000)::bigint \
     FROM pg_stat_activity WHERE pid = $1";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CommitStats {
    pub n_addresses: u64,
    pub n_utxos: u64,
    pub n_txs: u64,
    // Estimate of bound parameter sizes, not wire bytes.
    pub bytes: u64,
}

impl Add for CommitStats {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Self {
            n_addresses: self.n_addresses + other.n_addresses,
            n_utxos: self.n_utxos + other.n_utxos,
            n_txs: self.n_txs + other.n_txs,
            bytes: self.bytes + other.bytes,
        }
    }
}

impl AddAssign for CommitStats {
    fn add_assign(&mut self, other: Self) {
        *self = *self + other;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum CommitPhase {
    Begin = 0,
    ScriptPubkeys = 1,
    Utxos = 2,
    Transactions = 3,
    Commit = 4,
}

impl CommitPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            CommitPhase::Begin => "begin",
            CommitPhase::ScriptPubkeys => "bdk_script_pubkeys",
            CommitPhase::Utxos => "bdk_utxos",
            CommitPhase::Transactions => "bdk_transactions",
            CommitPhase::Commit => "commit",
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            1 => CommitPhase::ScriptPubkeys,
            2 => CommitPhase::Utxos,
            3 => CommitPhase::Transactions,
            4 => CommitPhase::Commit,
            _ => CommitPhase::Begin,
        }
    }
}

pub fn chunks_for(rows: usize, chunk_size: usize) -> u64 {
    rows.div_ceil(chunk_size) as u64
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProgressSnapshot {
    pub phase: CommitPhase,
    pub chunk_index: u64,
    pub chunks_total: u64,
    pub bytes_sent: u64,
}

#[derive(Debug, Default)]
pub struct CommitProgress {
    phase: AtomicU8,
    chunk_index: AtomicU64,
    chunks_total: AtomicU64,
    // Estimate of bound parameter sizes, not wire bytes.
    bytes_sent: AtomicU64,
    watchdog_ticks: AtomicU64,
}

impl CommitProgress {
    pub fn new(chunks_total: u64) -> Self {
        Self {
            chunks_total: AtomicU64::new(chunks_total),
            ..Default::default()
        }
    }

    pub fn set_phase(&self, phase: CommitPhase) {
        self.phase.store(phase as u8, Ordering::Relaxed);
    }

    // Returns the 1-based position of this chunk across the whole commit.
    pub fn begin_chunk(&self, phase: CommitPhase) -> u64 {
        self.set_phase(phase);
        self.chunk_index.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn finish_chunk(&self, bytes: u64) {
        self.bytes_sent.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn bytes_sent(&self) -> u64 {
        self.bytes_sent.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub fn watchdog_ticks(&self) -> u64 {
        self.watchdog_ticks.load(Ordering::Relaxed)
    }

    pub fn snapshot(&self) -> ProgressSnapshot {
        ProgressSnapshot {
            phase: CommitPhase::from_u8(self.phase.load(Ordering::Relaxed)),
            chunk_index: self.chunk_index.load(Ordering::Relaxed),
            chunks_total: self.chunks_total.load(Ordering::Relaxed),
            bytes_sent: self.bytes_sent.load(Ordering::Relaxed),
        }
    }
}

// Runs one INSERT chunk inside a `bdk.persist_chunk` span and tracks progress.
pub async fn run_chunk<F, T>(
    progress: &CommitProgress,
    phase: CommitPhase,
    rows: usize,
    bytes: u64,
    statement: F,
) -> Result<T, bdk::Error>
where
    F: Future<Output = Result<T, sqlx::Error>>,
{
    let chunk_index = progress.begin_chunk(phase);
    let span = tracing::info_span!(
        "bdk.persist_chunk",
        table = phase.as_str(),
        chunk_index,
        rows,
        bytes,
    );
    match statement.instrument(span.clone()).await {
        Ok(res) => {
            progress.finish_chunk(bytes);
            Ok(res)
        }
        Err(e) => {
            tracing::error!(parent: &span, error = %e, "persist chunk failed");
            Err(bdk::Error::Generic(e.to_string()))
        }
    }
}

struct CountingWriter(u64);

impl io::Write for CountingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0 += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn json_len(value: &serde_json::Value) -> u64 {
    let mut writer = CountingWriter(0);
    let _ = serde_json::to_writer(&mut writer, value);
    writer.0
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgBackendSnapshot {
    pub state: Option<String>,
    pub wait_event_type: Option<String>,
    pub wait_event: Option<String>,
    pub state_age_ms: Option<i64>,
}

pub async fn pg_backend_snapshot(
    pool: &PgPool,
    backend_pid: i32,
) -> Result<Option<PgBackendSnapshot>, String> {
    let query = sqlx::query_as::<_, (Option<String>, Option<String>, Option<String>, Option<i64>)>(
        PG_SNAPSHOT_SQL,
    )
    .bind(backend_pid)
    .fetch_optional(pool);
    match tokio::time::timeout(PG_SNAPSHOT_TIMEOUT, query).await {
        Ok(Ok(row)) => {
            Ok(row.map(
                |(state, wait_event_type, wait_event, state_age_ms)| PgBackendSnapshot {
                    state,
                    wait_event_type,
                    wait_event,
                    state_age_ms,
                },
            ))
        }
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err(format!(
            "pg_stat_activity query timed out after {}s",
            PG_SNAPSHOT_TIMEOUT.as_secs()
        )),
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CommitIds {
    pub keychain_id: KeychainId,
    pub wallet_id: Option<WalletId>,
}

#[must_use]
pub struct WatchdogGuard(JoinHandle<()>);

impl Drop for WatchdogGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub fn spawn_watchdog(
    started: Instant,
    pool: PgPool,
    backend_pid: Option<i32>,
    progress: Arc<CommitProgress>,
    ids: CommitIds,
    parent_span: Span,
) -> WatchdogGuard {
    spawn_watchdog_every(
        WATCHDOG_INTERVAL,
        started,
        pool,
        backend_pid,
        progress,
        ids,
        parent_span,
    )
}

fn spawn_watchdog_every(
    interval: Duration,
    started: Instant,
    pool: PgPool,
    backend_pid: Option<i32>,
    progress: Arc<CommitProgress>,
    ids: CommitIds,
    parent_span: Span,
) -> WatchdogGuard {
    WatchdogGuard(tokio::spawn(watchdog_loop(
        interval,
        started,
        pool,
        backend_pid,
        progress,
        ids,
        parent_span,
    )))
}

async fn watchdog_loop(
    interval: Duration,
    started: Instant,
    pool: PgPool,
    backend_pid: Option<i32>,
    progress: Arc<CommitProgress>,
    ids: CommitIds,
    parent_span: Span,
) {
    loop {
        tokio::time::sleep(interval).await;
        let tick = progress.watchdog_ticks.fetch_add(1, Ordering::Relaxed) + 1;
        let snapshot = progress.snapshot();
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let span = tracing::info_span!(
            parent: &parent_span,
            "bdk.commit_batch.still_running",
            keychain_id = %ids.keychain_id,
            wallet_id = field::Empty,
            tick,
            elapsed_ms,
            table = snapshot.phase.as_str(),
            chunk_index = snapshot.chunk_index,
            chunks_total = snapshot.chunks_total,
            bytes_sent = snapshot.bytes_sent,
            backend_pid = field::Empty,
            pg.state = field::Empty,
            pg.wait_event_type = field::Empty,
            pg.wait_event = field::Empty,
            pg.state_age_ms = field::Empty,
            pg.error = field::Empty,
        );
        if let Some(wallet_id) = ids.wallet_id {
            span.record("wallet_id", tracing::field::display(wallet_id));
        }
        match backend_pid {
            None => {
                span.record("pg.error", "backend pid unknown");
            }
            Some(pid) => {
                span.record("backend_pid", pid);
                match pg_backend_snapshot(&pool, pid)
                    .instrument(span.clone())
                    .await
                {
                    Ok(Some(pg)) => {
                        if let Some(state) = pg.state.as_deref() {
                            span.record("pg.state", state);
                        }
                        if let Some(wait_event_type) = pg.wait_event_type.as_deref() {
                            span.record("pg.wait_event_type", wait_event_type);
                        }
                        if let Some(wait_event) = pg.wait_event.as_deref() {
                            span.record("pg.wait_event", wait_event);
                        }
                        if let Some(state_age_ms) = pg.state_age_ms {
                            span.record("pg.state_age_ms", state_age_ms);
                        }
                    }
                    Ok(None) => {
                        span.record("pg.error", "no pg_stat_activity row for backend pid");
                    }
                    Err(e) => {
                        span.record("pg.error", e.as_str());
                    }
                }
            }
        }
        tracing::warn!(
            parent: &span,
            elapsed_ms,
            table = snapshot.phase.as_str(),
            chunk_index = snapshot.chunk_index,
            chunks_total = snapshot.chunks_total,
            "commit_batch still running"
        );
    }
}

#[cfg(test)]
pub(super) mod test_capture {
    use std::{
        collections::HashMap,
        fmt::Debug,
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };

    use tracing::{
        field::{Field, Visit},
        span, Dispatch, Event, Level, Subscriber,
    };
    use tracing_subscriber::{
        layer::{Context, SubscriberExt},
        registry::LookupSpan,
        Layer,
    };

    #[derive(Debug, Clone)]
    pub struct CapturedEvent {
        pub level: Level,
        pub fields: HashMap<String, String>,
    }

    #[derive(Debug, Clone)]
    pub struct CapturedSpan {
        pub name: &'static str,
        pub parent: Option<&'static str>,
        pub fields: HashMap<String, String>,
        pub events: Vec<CapturedEvent>,
    }

    impl CapturedSpan {
        pub fn field(&self, name: &str) -> Option<&str> {
            self.fields.get(name).map(String::as_str)
        }

        pub fn has_error_event(&self) -> bool {
            self.events.iter().any(|e| e.level == Level::ERROR)
        }
    }

    type Store = Arc<Mutex<Vec<CapturedSpan>>>;

    // One dispatcher shared by the test thread and every runtime worker.
    pub struct Capture {
        store: Store,
        pub dispatch: Dispatch,
    }

    impl Capture {
        pub fn new() -> Self {
            let store = Store::default();
            let dispatch = Dispatch::new(
                tracing_subscriber::registry().with(CaptureLayer(Arc::clone(&store))),
            );
            Self { store, dispatch }
        }

        pub fn runtime(&self) -> tokio::runtime::Runtime {
            let dispatch = self.dispatch.clone();
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .on_thread_start(move || {
                    std::mem::forget(tracing::dispatcher::set_default(&dispatch));
                })
                .build()
                .expect("runtime should build")
        }

        pub fn named(&self, name: &str) -> Vec<CapturedSpan> {
            self.store
                .lock()
                .expect("capture lock")
                .iter()
                .filter(|s| s.name == name)
                .cloned()
                .collect()
        }

        pub async fn wait_for(
            &self,
            name: &str,
            pred: impl Fn(&CapturedSpan) -> bool,
        ) -> CapturedSpan {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(span) = self.named(name).into_iter().find(|s| pred(s)) {
                    return span;
                }
                assert!(
                    Instant::now() < deadline,
                    "no matching {name} span captured"
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
    }

    struct SpanIdx(usize);

    struct FieldVisitor<'a>(&'a mut HashMap<String, String>);

    impl Visit for FieldVisitor<'_> {
        fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
            self.0
                .insert(field.name().to_string(), format!("{value:?}"));
        }

        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.insert(field.name().to_string(), value.to_string());
        }
    }

    struct CaptureLayer(Store);

    impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for CaptureLayer {
        fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
            let span = ctx.span(id).expect("new span is registered");
            let mut fields = HashMap::new();
            attrs.record(&mut FieldVisitor(&mut fields));
            let parent = span.parent().map(|p| p.name());
            let mut store = self.0.lock().expect("capture lock");
            store.push(CapturedSpan {
                name: span.name(),
                parent,
                fields,
                events: Vec::new(),
            });
            span.extensions_mut().insert(SpanIdx(store.len() - 1));
        }

        fn on_record(&self, id: &span::Id, values: &span::Record<'_>, ctx: Context<'_, S>) {
            if let Some(span) = ctx.span(id) {
                if let Some(SpanIdx(idx)) = span.extensions().get::<SpanIdx>() {
                    let mut store = self.0.lock().expect("capture lock");
                    values.record(&mut FieldVisitor(&mut store[*idx].fields));
                }
            }
        }

        fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
            if let Some(span) = ctx.event_span(event) {
                if let Some(SpanIdx(idx)) = span.extensions().get::<SpanIdx>() {
                    let mut fields = HashMap::new();
                    event.record(&mut FieldVisitor(&mut fields));
                    let mut store = self.0.lock().expect("capture lock");
                    store[*idx].events.push(CapturedEvent {
                        level: *event.metadata().level(),
                        fields,
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_capture::Capture;

    fn pg_url() -> String {
        let pg_host = std::env::var("PG_HOST").unwrap_or("localhost".to_string());
        format!("postgres://user:password@{pg_host}:5432/pg")
    }

    fn ids() -> CommitIds {
        CommitIds {
            keychain_id: uuid::Uuid::nil().into(),
            wallet_id: None,
        }
    }

    #[test]
    fn commit_stats_add() {
        let a = CommitStats {
            n_addresses: 1,
            n_utxos: 2,
            n_txs: 3,
            bytes: 4,
        };
        let mut b = CommitStats {
            n_addresses: 10,
            n_utxos: 20,
            n_txs: 30,
            bytes: 40,
        };
        assert_eq!(
            a + b,
            CommitStats {
                n_addresses: 11,
                n_utxos: 22,
                n_txs: 33,
                bytes: 44,
            }
        );
        b += a;
        assert_eq!(b.n_addresses, 11);
        assert_eq!(b.bytes, 44);
        assert_eq!(b + CommitStats::default(), b);
    }

    #[test]
    fn chunks_for_rounds_up() {
        assert_eq!(chunks_for(0, 5000), 0);
        assert_eq!(chunks_for(1, 5000), 1);
        assert_eq!(chunks_for(5000, 5000), 1);
        assert_eq!(chunks_for(5001, 5000), 2);
        assert_eq!(chunks_for(4001, 2000), 3);
    }

    #[test]
    fn progress_tracks_chunks_and_bytes() {
        let progress = CommitProgress::new(3);
        assert_eq!(progress.begin_chunk(CommitPhase::ScriptPubkeys), 1);
        progress.finish_chunk(100);
        assert_eq!(progress.begin_chunk(CommitPhase::Utxos), 2);
        let snapshot = progress.snapshot();
        assert_eq!(snapshot.phase, CommitPhase::Utxos);
        assert_eq!(snapshot.chunk_index, 2);
        assert_eq!(snapshot.chunks_total, 3);
        assert_eq!(snapshot.bytes_sent, 100);
    }

    #[test]
    fn json_len_matches_serialized_length() {
        let value = serde_json::json!({"a": [1, 2, 3], "b": "text"});
        assert_eq!(json_len(&value), value.to_string().len() as u64);
    }

    #[tokio::test]
    async fn snapshot_reports_idle_in_transaction_for_open_tx() -> anyhow::Result<()> {
        let pool = PgPool::connect(&pg_url()).await?;
        let mut tx = pool.begin().await?;
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(tx.as_mut())
            .await?;

        let snapshot = pg_backend_snapshot(&pool, pid)
            .await
            .map_err(anyhow::Error::msg)?
            .expect("backend should be listed");
        assert_eq!(snapshot.state.as_deref(), Some("idle in transaction"));
        assert!(snapshot.state_age_ms.is_some());

        tx.rollback().await?;
        Ok(())
    }

    #[tokio::test]
    async fn snapshot_returns_none_for_unknown_pid() -> anyhow::Result<()> {
        let pool = PgPool::connect(&pg_url()).await?;
        let snapshot = pg_backend_snapshot(&pool, -1)
            .await
            .map_err(anyhow::Error::msg)?;
        assert!(snapshot.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn watchdog_guard_stops_the_task() -> anyhow::Result<()> {
        let pool = PgPool::connect(&pg_url()).await?;
        let progress = Arc::new(CommitProgress::new(1));
        let guard = spawn_watchdog_every(
            Duration::from_millis(10),
            Instant::now(),
            pool,
            None,
            Arc::clone(&progress),
            ids(),
            Span::none(),
        );

        let deadline = Instant::now() + Duration::from_secs(5);
        while progress.watchdog_ticks() < 2 {
            assert!(Instant::now() < deadline, "watchdog never ticked");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        drop(guard);
        tokio::time::sleep(Duration::from_millis(20)).await;
        let ticks_after_drop = progress.watchdog_ticks();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(progress.watchdog_ticks(), ticks_after_drop);
        Ok(())
    }

    fn with_capture<R>(f: impl FnOnce(&Capture, &tokio::runtime::Runtime) -> R) -> R {
        let capture = Capture::new();
        let rt = capture.runtime();
        tracing::dispatcher::with_default(&capture.dispatch.clone(), || f(&capture, &rt))
    }

    fn assert_progress_fields(span: &test_capture::CapturedSpan) {
        assert_eq!(span.parent, Some("bdk.commit_batch"));
        assert_eq!(span.field("table"), Some("bdk_utxos"));
        assert_eq!(span.field("chunk_index"), Some("2"));
        assert_eq!(span.field("chunks_total"), Some("3"));
        assert_eq!(span.field("bytes_sent"), Some("123"));
        assert!(span.field("elapsed_ms").is_some());
        assert!(span.field("tick").is_some());
    }

    fn progress_mid_commit() -> Arc<CommitProgress> {
        let progress = Arc::new(CommitProgress::new(3));
        progress.begin_chunk(CommitPhase::ScriptPubkeys);
        progress.finish_chunk(123);
        progress.begin_chunk(CommitPhase::Utxos);
        progress
    }

    #[test]
    fn still_running_span_reports_pg_state_for_stuck_backend() -> anyhow::Result<()> {
        with_capture(|capture, rt| {
            rt.block_on(async {
                let pool = PgPool::connect(&pg_url()).await?;
                let mut tx = pool.begin().await?;
                let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                    .fetch_one(tx.as_mut())
                    .await?;
                let parent = tracing::info_span!("bdk.commit_batch");
                let guard = spawn_watchdog_every(
                    Duration::from_millis(20),
                    Instant::now(),
                    pool.clone(),
                    Some(pid),
                    progress_mid_commit(),
                    ids(),
                    parent.clone(),
                );

                let span = capture
                    .wait_for("bdk.commit_batch.still_running", |s| {
                        s.fields.contains_key("pg.state")
                            && s.fields.contains_key("pg.state_age_ms")
                            && s.events.iter().any(|e| e.level == tracing::Level::WARN)
                    })
                    .await;
                drop(guard);

                assert_progress_fields(&span);
                assert_eq!(span.field("backend_pid"), Some(pid.to_string().as_str()));
                assert_eq!(span.field("pg.state"), Some("idle in transaction"));
                assert!(span.field("pg.state_age_ms").is_some());
                assert!(span.field("pg.wait_event_type").is_some());
                assert!(span.field("pg.error").is_none());
                assert!(span.events.iter().any(|e| e.level == tracing::Level::WARN));

                tx.rollback().await?;
                Ok(())
            })
        })
    }

    #[test]
    fn still_running_span_sets_pg_error_without_backend_pid() -> anyhow::Result<()> {
        with_capture(|capture, rt| {
            rt.block_on(async {
                let pool = PgPool::connect(&pg_url()).await?;
                let parent = tracing::info_span!("bdk.commit_batch");
                let guard = spawn_watchdog_every(
                    Duration::from_millis(20),
                    Instant::now(),
                    pool,
                    None,
                    progress_mid_commit(),
                    ids(),
                    parent.clone(),
                );
                let span = capture
                    .wait_for("bdk.commit_batch.still_running", |s| {
                        s.fields.contains_key("pg.error")
                    })
                    .await;
                drop(guard);

                assert_progress_fields(&span);
                assert_eq!(span.field("pg.error"), Some("backend pid unknown"));
                assert!(span.field("backend_pid").is_none());
                assert!(span.field("pg.state").is_none());
                Ok(())
            })
        })
    }

    #[test]
    fn still_running_span_sets_pg_error_when_query_fails() -> anyhow::Result<()> {
        with_capture(|capture, rt| {
            rt.block_on(async {
                let pool = PgPool::connect(&pg_url()).await?;
                pool.close().await;
                let parent = tracing::info_span!("bdk.commit_batch");
                let guard = spawn_watchdog_every(
                    Duration::from_millis(20),
                    Instant::now(),
                    pool,
                    Some(12345),
                    progress_mid_commit(),
                    ids(),
                    parent.clone(),
                );
                let span = capture
                    .wait_for("bdk.commit_batch.still_running", |s| {
                        s.fields.contains_key("pg.error")
                    })
                    .await;
                drop(guard);

                assert_progress_fields(&span);
                assert_eq!(span.field("backend_pid"), Some("12345"));
                assert!(span
                    .field("pg.error")
                    .expect("pg.error set")
                    .contains("closed"));
                assert!(span.field("pg.state").is_none());
                Ok(())
            })
        })
    }

    #[test]
    fn run_chunk_failure_keeps_error_and_marks_chunk_span() -> anyhow::Result<()> {
        with_capture(|capture, rt| {
            rt.block_on(async {
                let pool = PgPool::connect(&pg_url()).await?;
                const BAD_SQL: &str = "INSERT INTO no_such_table VALUES (1)";
                let expected = sqlx::query(BAD_SQL)
                    .execute(&pool)
                    .await
                    .expect_err("statement should fail")
                    .to_string();

                let progress = CommitProgress::new(1);
                let result = run_chunk(
                    &progress,
                    CommitPhase::Transactions,
                    5,
                    100,
                    sqlx::query(BAD_SQL).execute(&pool),
                )
                .await;

                match result {
                    Err(bdk::Error::Generic(message)) => assert_eq!(message, expected),
                    other => panic!("unexpected result: {other:?}"),
                }
                let chunks = capture.named("bdk.persist_chunk");
                assert_eq!(chunks.len(), 1);
                let chunk = &chunks[0];
                assert_eq!(chunk.field("table"), Some("bdk_transactions"));
                assert_eq!(chunk.field("chunk_index"), Some("1"));
                assert_eq!(chunk.field("rows"), Some("5"));
                assert_eq!(chunk.field("bytes"), Some("100"));
                assert!(chunk.has_error_event());
                assert_eq!(progress.bytes_sent(), 0);
                Ok(())
            })
        })
    }

    #[test]
    fn watchdog_stops_when_commit_path_errors() -> anyhow::Result<()> {
        with_capture(|capture, rt| {
            rt.block_on(async {
                let pool = PgPool::connect(&pg_url()).await?;
                let progress = Arc::new(CommitProgress::new(1));

                let failing_commit = async {
                    let _watchdog = spawn_watchdog_every(
                        Duration::from_millis(10),
                        Instant::now(),
                        pool.clone(),
                        None,
                        Arc::clone(&progress),
                        ids(),
                        Span::none(),
                    );
                    while progress.watchdog_ticks() < 1 {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    Err::<(), _>(bdk::Error::Generic("boom".to_string()))
                };
                assert!(failing_commit.await.is_err());

                tokio::time::sleep(Duration::from_millis(20)).await;
                let ticks = progress.watchdog_ticks();
                let spans = capture.named("bdk.commit_batch.still_running").len();
                tokio::time::sleep(Duration::from_millis(100)).await;
                assert_eq!(progress.watchdog_ticks(), ticks);
                assert_eq!(capture.named("bdk.commit_batch.still_running").len(), spans);
                Ok(())
            })
        })
    }
}
