// Large commit_batch over TLS must not hang (launchbadge/sqlx#4248, fixed by vendored sqlx-core).
use std::{
    str::FromStr,
    sync::mpsc,
    time::{Duration, Instant},
};

use bdk::{
    bitcoin::{
        absolute::LockTime, hashes::Hash, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut,
        Txid, Witness,
    },
    database::{BatchDatabase, BatchOperations},
    BlockTime, KeychainKind, LocalUtxo, TransactionDetails,
};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

use super::SqlxWalletDb;
use crate::primitives::KeychainId;

const PG_CON: &str = "BRIA_TLS_COMMIT_TEST_PG_CON";
const APP_NAME: &str = "bria-tls-commit-test";

type StuckSession = (i32, Option<String>, Option<String>, Option<String>);

fn env_or<T: FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn random_bytes(n: usize) -> Vec<u8> {
    (0..n).map(|_| rand::random::<u8>()).collect()
}

fn random_txid() -> Txid {
    Txid::from_slice(&rand::random::<[u8; 32]>()).expect("32 bytes")
}

fn p2wpkh() -> ScriptBuf {
    ScriptBuf::from([vec![0x00, 0x14], random_bytes(20)].concat())
}

fn deposit_tx(n_inputs: usize, height: u32) -> TransactionDetails {
    let input = (0..n_inputs)
        .map(|i| TxIn {
            previous_output: OutPoint::new(random_txid(), i as u32),
            script_sig: ScriptBuf::new(),
            sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
            witness: Witness::from_slice(&[random_bytes(72), random_bytes(33)]),
        })
        .collect();
    let output = vec![
        TxOut {
            value: 50_000,
            script_pubkey: p2wpkh(),
        },
        TxOut {
            value: 1_234_567,
            script_pubkey: p2wpkh(),
        },
    ];
    let transaction = Transaction {
        version: 2,
        lock_time: LockTime::ZERO,
        input,
        output,
    };
    TransactionDetails {
        txid: transaction.txid(),
        transaction: Some(transaction),
        received: 50_000,
        sent: 0,
        fee: Some(1_000),
        confirmation_time: Some(BlockTime {
            height,
            timestamp: 1_700_000_000 + height as u64,
        }),
    }
}

// Large-wallet shape: ~1.7 KB details_json on average, one ~180 KB tx per 1000.
fn deposit_batch(n_txs: usize) -> (Vec<TransactionDetails>, Vec<LocalUtxo>) {
    let txs: Vec<_> = (0..n_txs)
        .map(|i| {
            let n_inputs = if i % 1000 == 999 { 500 } else { 1 + i % 6 };
            deposit_tx(n_inputs, 800_000 + i as u32)
        })
        .collect();
    let utxos = txs
        .iter()
        .map(|tx| LocalUtxo {
            outpoint: OutPoint::new(tx.txid, 0),
            txout: tx.transaction.as_ref().expect("raw tx").output[0].clone(),
            keychain: KeychainKind::External,
            is_spent: false,
        })
        .collect();
    (txs, utxos)
}

fn commit_deposit_batch(workers: usize) {
    let url = std::env::var(PG_CON).unwrap_or_else(|_| panic!("{PG_CON} must be set"));
    let n_txs: usize = env_or("BRIA_TLS_COMMIT_TEST_TXS", 50_000);
    let iterations: usize = env_or("BRIA_TLS_COMMIT_TEST_ITERS", 1);
    let timeout = Duration::from_secs(env_or("BRIA_TLS_COMMIT_TEST_TIMEOUT_SECS", 120));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .expect("runtime");
    let options = PgConnectOptions::from_str(&url)
        .expect("valid connection string")
        .application_name(APP_NAME);
    let pool = rt
        .block_on(
            PgPoolOptions::new()
                .max_connections(10)
                .connect_with(options),
        )
        .expect("connect");
    rt.block_on(sqlx::migrate!().run(&pool))
        .expect("migrations");
    let tls: (bool,) = rt
        .block_on(
            sqlx::query_as("SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()")
                .fetch_one(&pool),
        )
        .expect("pg_stat_ssl");

    let keychain_id = KeychainId::new();
    let (txs, utxos) = deposit_batch(n_txs);
    let json_sizes: Vec<usize> = txs
        .iter()
        .map(|tx| serde_json::to_vec(tx).expect("json").len())
        .collect();
    println!(
        "tls={} workers={workers} txs={n_txs} utxos={} details_json avg={}B max={}B total={:.1}MB",
        tls.0,
        utxos.len(),
        json_sizes.iter().sum::<usize>() / n_txs.max(1),
        json_sizes.iter().max().copied().unwrap_or(0),
        json_sizes.iter().sum::<usize>() as f64 / 1e6
    );

    for iteration in 1..=iterations {
        let (done_tx, done_rx) = mpsc::channel();
        let (db_pool, txs, utxos) = (pool.clone(), txs.clone(), utxos.clone());
        rt.spawn(async move {
            let res = tokio::task::spawn_blocking(move || {
                let mut db = SqlxWalletDb::new(db_pool, keychain_id);
                let mut batch = db.begin_batch();
                for utxo in &utxos {
                    batch.set_utxo(utxo)?;
                }
                for tx in &txs {
                    batch.set_tx(tx)?;
                }
                let start = Instant::now();
                db.commit_batch(batch)?;
                Ok::<_, bdk::Error>(start.elapsed())
            })
            .await;
            let _ = done_tx.send(res);
        });

        match done_rx.recv_timeout(timeout) {
            Ok(Ok(Ok(elapsed))) => println!("iteration {iteration}: commit_batch took {elapsed:?}"),
            Ok(Ok(Err(e))) => panic!("iteration {iteration}: commit_batch failed: {e}"),
            Ok(Err(e)) => panic!("iteration {iteration}: commit_batch panicked: {e}"),
            Err(_) => {
                let stuck: Vec<StuckSession> = rt
                    .block_on(
                        sqlx::query_as(
                            "SELECT pid, state, wait_event, left(query, 60) FROM pg_stat_activity \
                             WHERE application_name = $1 AND state = 'idle in transaction'",
                        )
                        .bind(APP_NAME)
                        .fetch_all(&pool),
                    )
                    .unwrap_or_default();
                for (pid, ..) in &stuck {
                    let _ = rt.block_on(
                        sqlx::query("SELECT pg_terminate_backend($1)")
                            .bind(pid)
                            .execute(&pool),
                    );
                }
                let after_terminate = done_rx.recv_timeout(Duration::from_secs(30));
                rt.shutdown_timeout(Duration::from_secs(5));
                panic!(
                    "iteration {iteration}: commit_batch hung for {timeout:?}; sessions: {stuck:?}; after terminate: {after_terminate:?}"
                );
            }
        }
    }

    let counts: (i64, i64) = rt
        .block_on(
            sqlx::query_as(
                "SELECT (SELECT COUNT(*) FROM bdk_transactions WHERE keychain_id = $1), \
                        (SELECT COUNT(*) FROM bdk_utxos WHERE keychain_id = $1)",
            )
            .bind(uuid::Uuid::from(keychain_id))
            .fetch_one(&pool),
        )
        .expect("counts");
    assert_eq!(counts, (n_txs as i64, n_txs as i64));
}

#[test]
#[ignore = "needs Postgres with TLS: set BRIA_TLS_COMMIT_TEST_PG_CON"]
fn commit_batch_over_tls_one_worker() {
    commit_deposit_batch(1);
}

#[test]
#[ignore = "needs Postgres with TLS: set BRIA_TLS_COMMIT_TEST_PG_CON"]
fn commit_batch_over_tls_two_workers() {
    commit_deposit_batch(2);
}
