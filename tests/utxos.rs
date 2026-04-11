mod helpers;

use bdk::{
    bitcoin::{ScriptBuf, TxOut},
    wallet::AddressInfo,
    KeychainKind, LocalUtxo,
};
use bria::{
    primitives::{bitcoin::*, *},
    utxo::{SpendDetectedOutcome, Utxos},
};
use sqlx::Row;
use uuid::Uuid;

#[tokio::test]
async fn spend_detected_is_idempotent() -> anyhow::Result<()> {
    let pool = helpers::init_pool().await?;
    let utxos = Utxos::new(&pool);

    let profile = helpers::create_test_account(&pool).await?;
    let account_id = profile.account_id;
    let wallet_id = WalletId::new();
    let keychain_id = KeychainId::new();
    let tx_id = LedgerTransactionId::new();

    sqlx::query("INSERT INTO bria_wallets (id, account_id, name) VALUES ($1, $2, $3)")
        .bind(Uuid::from(wallet_id))
        .bind(Uuid::from(account_id))
        .bind(format!("wallet_{}", wallet_id))
        .execute(&pool)
        .await?;

    let income_outpoint = OutPoint {
        txid: "4010e27ff7dc6d9c66a5657e6b3d94b4c4e394d968398d16fefe4637463d194d".parse()?,
        vout: 0,
    };
    let income_local_utxo = LocalUtxo {
        outpoint: income_outpoint,
        txout: TxOut {
            value: 100_000_000u64,
            script_pubkey: ScriptBuf::new(),
        },
        keychain: KeychainKind::External,
        is_spent: false,
    };
    let income_address_info = AddressInfo {
        index: 0,
        address: "bcrt1qzg4a08kc2xrp08d9k5jadm78ehf7catp735zn0"
            .parse::<bdk::bitcoin::Address<bdk::bitcoin::address::NetworkUnchecked>>()?
            .assume_checked(),
        keychain: KeychainKind::External,
    };

    let change_outpoint = OutPoint {
        txid: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".parse()?,
        vout: 1,
    };
    let change_local_utxo = LocalUtxo {
        outpoint: change_outpoint,
        txout: TxOut {
            value: 40_000_000u64,
            script_pubkey: ScriptBuf::new(),
        },
        keychain: KeychainKind::Internal,
        is_spent: false,
    };
    let change_address_info = AddressInfo {
        index: 0,
        address: "bcrt1q6q79yce8vutqzpnwkxr5x8p5kxw5rc0hqqzwym"
            .parse::<bdk::bitcoin::Address<bdk::bitcoin::address::NetworkUnchecked>>()?
            .assume_checked(),
        keychain: KeychainKind::Internal,
    };
    let change_utxos: Vec<(&LocalUtxo, AddressInfo)> =
        vec![(&change_local_utxo, change_address_info)];

    let (_, db_tx) = utxos
        .new_utxo_detected(
            account_id,
            wallet_id,
            keychain_id,
            &income_address_info,
            &income_local_utxo,
            Satoshis::from(1_000u64),
            200,
            false,
            1,
        )
        .await?
        .expect("income utxo should be newly inserted");
    db_tx.commit().await?;

    let mut db_tx = pool.begin().await?;
    let result = utxos
        .spend_detected(
            &mut db_tx,
            account_id,
            wallet_id,
            keychain_id,
            tx_id,
            std::iter::once(&income_outpoint),
            &change_utxos,
            None,
            Satoshis::from(300u64),
            200,
            1,
        )
        .await?;
    assert!(
        matches!(result, SpendDetectedOutcome::Applied(..)),
        "first call must be Applied"
    );
    db_tx.commit().await?;

    let row = sqlx::query(
        "SELECT spend_detected_ledger_tx_id FROM bria_utxos WHERE keychain_id = $1 AND tx_id = $2 AND vout = $3",
    )
    .bind(keychain_id)
    .bind(income_outpoint.txid.to_string())
    .bind(income_outpoint.vout as i32)
    .fetch_one(&pool)
    .await?;
    assert!(
        row.get::<Option<uuid::Uuid>, _>("spend_detected_ledger_tx_id")
            .is_some(),
        "spend_detected_ledger_tx_id must be set after Applied"
    );

    let mut db_tx = pool.begin().await?;
    let result = utxos
        .spend_detected(
            &mut db_tx,
            account_id,
            wallet_id,
            keychain_id,
            tx_id,
            std::iter::once(&income_outpoint),
            &change_utxos,
            None,
            Satoshis::from(300u64),
            200,
            1,
        )
        .await?;
    assert!(
        matches!(result, SpendDetectedOutcome::AlreadyApplied),
        "retry must be AlreadyApplied, not Deferred"
    );
    db_tx.commit().await?;

    Ok(())
}

#[tokio::test]
async fn spend_detected_deferred_when_inputs_missing() -> anyhow::Result<()> {
    let pool = helpers::init_pool().await?;
    let utxos = Utxos::new(&pool);

    let profile = helpers::create_test_account(&pool).await?;
    let account_id = profile.account_id;
    let wallet_id = WalletId::new();
    let keychain_id = KeychainId::new();
    let unknown_outpoint = OutPoint {
        txid: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".parse()?,
        vout: 0,
    };
    let change_utxos: Vec<(&LocalUtxo, AddressInfo)> = vec![];

    sqlx::query("INSERT INTO bria_wallets (id, account_id, name) VALUES ($1, $2, $3)")
        .bind(Uuid::from(wallet_id))
        .bind(Uuid::from(account_id))
        .bind(format!("wallet_{}", wallet_id))
        .execute(&pool)
        .await?;

    let mut db_tx = pool.begin().await?;
    let result = utxos
        .spend_detected(
            &mut db_tx,
            account_id,
            wallet_id,
            keychain_id,
            LedgerTransactionId::new(),
            std::iter::once(&unknown_outpoint),
            &change_utxos,
            None,
            Satoshis::from(300u64),
            200,
            1,
        )
        .await?;
    assert!(
        matches!(result, SpendDetectedOutcome::Deferred),
        "missing inputs must return Deferred"
    );
    db_tx.rollback().await?;

    Ok(())
}
