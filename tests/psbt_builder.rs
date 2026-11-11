mod helpers;

use rand::Rng;
use serial_test::serial;

use bdk::{bitcoin::Network, blockchain::Blockchain, wallet::AddressIndex, FeeRate, SignOptions};
use uuid::Uuid;

use bria::{fees::BoundedFeeRate, primitives::*, utxo::*, wallet::*, xpub::*};

fn test_progress_context() -> SyncProgressContext {
    SyncProgressContext::new()
}

#[tokio::test]
#[serial]
async fn build_psbt() -> anyhow::Result<()> {
    // ChatGPT description of this test:
    //
    // This test is designed to cover specific scenarios to ensure the proper functioning of the PSBT
    // building process in a multi-wallet and multi-keychain environment. Two notable properties are
    // included as part of the test:
    //
    // 1. A wallet that spends without a change output: The test sets up a domain wallet that receives
    //    funding and sends a transaction without generating a change output. This scenario is created
    //    by carefully selecting the amount to be sent, such that it equals the funded amount minus a
    //    predefined fee. The test verifies that there is no change output for this wallet by asserting
    //    that the change satoshis are zero and that there is no change outpoint present.
    //
    // 2. A wallet with 2 keychains that consolidates UTXOs from the deprecated keychain: The test
    //    creates another wallet with two keychains - one current and one deprecated. Both keychains
    //    receive funding, simulating a real-world scenario where a wallet has UTXOs in a deprecated
    //    keychain. The `PsbtBuilder` is configured to consolidate deprecated keychains using the
    //    `consolidate_deprecated_keychains(true)` method. The test verifies that the UTXOs from both
    //    the current and deprecated keychains are included in the PSBT, ensuring that the consolidation
    //    of UTXOs from the deprecated keychain takes place as expected.
    //
    // By including these properties in the test, it ensures that the PSBT building process can handle
    // scenarios where a wallet spends without a change output and properly consolidates UTXOs from
    // deprecated keychains.

    let pool = helpers::init_pool().await?;

    let external = "wpkh([6f2fa1b2/84'/0'/0']tpubDDDDGYiFda8HfJRc2AHFJDxVzzEtBPrKsbh35EaW2UGd5qfzrF2G87ewAgeeRyHEz4iB3kvhAYW1sH6dpLepTkFUzAktumBN8AXeXWE9nd1/0/*)#l6n08zmr";
    let internal = "wpkh([6f2fa1b2/84'/0'/0']tpubDDDDGYiFda8HfJRc2AHFJDxVzzEtBPrKsbh35EaW2UGd5qfzrF2G87ewAgeeRyHEz4iB3kvhAYW1sH6dpLepTkFUzAktumBN8AXeXWE9nd1/1/*)#wwkw6htm";
    let domain_current_keychain_id = Uuid::new_v4();
    let domain_wallet_id = WalletId::new();
    let keychain_cfg = KeychainConfig::try_from((external, internal))?;
    let domain_current_keychain = KeychainWallet::new(
        pool.clone(),
        Network::Regtest,
        domain_wallet_id,
        domain_current_keychain_id.into(),
        keychain_cfg,
    );

    let other_wallet_current_keychain = helpers::random_bdk_wallet()?;
    let other_wallet_deprecated_keychain = helpers::random_bdk_wallet()?;

    let domain_addr = domain_current_keychain.new_external_address().await?;
    let other_current_addr = other_wallet_current_keychain.get_address(AddressIndex::New)?;
    let other_change_address =
        other_wallet_current_keychain.get_internal_address(AddressIndex::New)?;
    let other_deprecated_addr = other_wallet_deprecated_keychain.get_address(AddressIndex::New)?;

    let bitcoind = helpers::bitcoind_client().await?;
    let wallet_funding = 700_000_000;
    let wallet_funding_sats = Satoshis::from(wallet_funding);
    let tx_id = helpers::fund_addr(&bitcoind, &domain_addr, wallet_funding)?;
    helpers::fund_addr(&bitcoind, &other_current_addr, wallet_funding - 200_000_000)?;
    helpers::fund_addr(&bitcoind, &other_deprecated_addr, 200_000_000)?;
    helpers::gen_blocks(&bitcoind, 10)?;

    let blockchain = helpers::electrum_blockchain().await?;
    for _ in 0..5 {
        other_wallet_current_keychain.sync(&blockchain, Default::default())?;
        other_wallet_deprecated_keychain.sync(&blockchain, Default::default())?;
        if other_wallet_current_keychain.get_balance()?.get_spendable() > 0
            && other_wallet_deprecated_keychain
                .get_balance()?
                .get_spendable()
                > 0
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    while !find_tx_id(&pool, domain_current_keychain_id, tx_id).await? {
        let blockchain = helpers::electrum_blockchain().await?;
        domain_current_keychain
            .sync(blockchain, test_progress_context())
            .await?;
    }

    let fee = FeeRate::from_sat_per_vb(1.0);
    let cfg = PsbtBuilderConfig::builder()
        .consolidate_deprecated_keychains(true)
        .fee_rate(BoundedFeeRate::unchecked(fee))
        .build()
        .unwrap();
    let builder = PsbtBuilder::new(cfg);

    let domain_send_amount = wallet_funding_sats - Satoshis::from(155);
    let other_wallet_id = WalletId::new();
    let send_amount = Satoshis::from(100_000_000);
    let destination = Address::parse_from_trusted_source("mgWUuj1J1N882jmqFxtDepEC73Rr22E9GU");
    let payouts_one = vec![(Uuid::new_v4(), destination.clone(), domain_send_amount)];

    let payouts_two = vec![
        (Uuid::new_v4(), destination.clone(), send_amount),
        (Uuid::new_v4(), destination.clone(), send_amount),
        (Uuid::new_v4(), destination, send_amount * 10),
    ];

    let builder = builder
        .wallet_payouts(domain_wallet_id, payouts_one)
        .accept_current_keychain();
    let builder = domain_current_keychain
        .dispatch_bdk_wallet(builder)
        .await?
        .next_wallet();
    let other_wallet_current_keychain_id =
        uuid::uuid!("00000000-0000-0000-0000-000000000001").into();
    let other_wallet_deprecated_keychain_id =
        uuid::uuid!("00000000-0000-0000-0000-000000000002").into();
    let builder = builder
        .wallet_payouts(other_wallet_id, payouts_two)
        .visit_bdk_wallet(
            other_wallet_deprecated_keychain_id,
            &other_wallet_deprecated_keychain,
        )?
        .accept_current_keychain()
        .visit_bdk_wallet(
            other_wallet_current_keychain_id,
            &other_wallet_current_keychain,
        )?;
    let FinishedPsbtBuild {
        psbt: unsigned_psbt,
        included_payouts,
        included_utxos,
        wallet_totals,
        fee_satoshis,
        ..
    } = builder.finish();
    assert_eq!(
        included_payouts
            .get(&domain_wallet_id)
            .expect("wallet not included in payouts")
            .len(),
        1
    );
    assert_eq!(
        included_payouts
            .get(&other_wallet_id)
            .expect("wallet not included in payouts")
            .len(),
        2
    );
    assert_eq!(
        included_utxos
            .get(&other_wallet_id)
            .expect("wallet not included in utxos")
            .get(&other_wallet_deprecated_keychain_id)
            .expect("keychain not included in utxos")
            .len(),
        1
    );
    assert_eq!(
        included_utxos
            .get(&other_wallet_id)
            .expect("wallet not included in utxos")
            .get(&other_wallet_current_keychain_id)
            .expect("keychain not included in utxos")
            .len(),
        1
    );
    assert_eq!(wallet_totals.len(), 2);
    let domain_wallet_total = wallet_totals.get(&domain_wallet_id).unwrap();
    assert_eq!(domain_wallet_total.input_satoshis, wallet_funding_sats);
    assert_eq!(domain_wallet_total.output_satoshis, domain_send_amount);
    assert_eq!(domain_wallet_total.change_satoshis, Satoshis::ZERO);
    assert!(domain_wallet_total.change_outpoint.is_none());
    assert_eq!(
        domain_wallet_total.output_satoshis + domain_wallet_total.total_fee_satoshis,
        domain_wallet_total.input_satoshis
    );
    let other_wallet_total = wallet_totals.get(&other_wallet_id).unwrap();
    assert_eq!(other_wallet_total.input_satoshis, wallet_funding_sats);
    assert_eq!(other_wallet_total.change_address, other_change_address);
    assert_eq!(other_wallet_total.total_fee_satoshis, Satoshis::from(193));
    assert_eq!(
        other_wallet_total
            .change_outpoint
            .expect("no change output")
            .vout,
        2
    );

    let mut unsigned_psbt = unsigned_psbt.expect("unsigned psbt");
    let total_tx_outs = unsigned_psbt
        .unsigned_tx
        .output
        .iter()
        .fold(0, |acc, out| acc + out.value);
    let total_summary_outs = wallet_totals
        .values()
        .fold(Satoshis::from(0), |acc, total| {
            acc + total.output_satoshis + total.change_satoshis
        });
    assert_eq!(total_tx_outs, u64::from(total_summary_outs));
    assert_eq!(total_tx_outs, u64::from(total_summary_outs));
    let total_summary_fees = wallet_totals
        .values()
        .fold(Satoshis::from(0), |acc, total| {
            acc + total.total_fee_satoshis
        });
    assert_eq!(total_summary_fees, fee_satoshis);
    assert!(unsigned_psbt.inputs.len() >= 3);
    assert_eq!(unsigned_psbt.outputs.len(), 4);

    other_wallet_current_keychain.sign(&mut unsigned_psbt, SignOptions::default())?;
    other_wallet_deprecated_keychain.sign(&mut unsigned_psbt, SignOptions::default())?;
    let mut bitcoind_client = helpers::bitcoind_signing_client().await?;
    let signed_psbt = bitcoind_client.sign_psbt(&unsigned_psbt).await?;
    let tx = domain_current_keychain
        .finalize_psbt(signed_psbt)
        .await?
        .expect("Finalize should have completed")
        .extract_tx();
    helpers::electrum_blockchain().await?.broadcast(&tx)?;

    Ok(())
}

#[tokio::test]
#[serial]
async fn build_psbt_with_cpfp() -> anyhow::Result<()> {
    let pool = helpers::init_pool().await?;

    let domain_current_keychain_id = Uuid::new_v4();
    let domain_wallet_id = WalletId::new();
    let xpub = XPub::try_from(("tpubDD4vFnWuTMEcZiaaZPgvzeGyMzWe6qHW8gALk5Md9kutDvtdDjYFwzauEFFRHgov8pAwup5jX88j5YFyiACsPf3pqn5hBjvuTLRAseaJ6b4", Some("m/84'/0'/0'"))).unwrap();
    let keychain_cfg = KeychainConfig::wpkh(xpub);
    let domain_current_keychain = KeychainWallet::new(
        pool.clone(),
        Network::Regtest,
        domain_wallet_id,
        domain_current_keychain_id.into(),
        keychain_cfg,
    );
    let domain_addr = domain_current_keychain.new_external_address().await?;

    let bitcoind = helpers::bitcoind_client().await?;
    let wallet_funding = 500_000_000;
    let wallet_funding_sats = Satoshis::from(wallet_funding);
    helpers::fund_addr(&bitcoind, &domain_addr, wallet_funding)?;
    helpers::gen_blocks(&bitcoind, 10)?;
    let fee_bump_funding: u64 = rand::thread_rng().gen_range(100_000_000..=201_000_000);
    let fee_bump_funding_sats = Satoshis::from(fee_bump_funding);
    let domain_addr = domain_current_keychain.new_external_address().await?;
    let tx_id = helpers::fund_addr(&bitcoind, &domain_addr, fee_bump_funding)?;
    let (outpoint, cpfp_tx_fee, cpfp_tx_vsize) =
        helpers::lookup_tx_info(&bitcoind, tx_id, u64::from(fee_bump_funding_sats))?;

    let attributions = std::iter::once((
        tx_id,
        FeeWeightAttribution {
            batch_id: Some(BatchId::new()),
            tx_id,
            fee: cpfp_tx_fee,
            vbytes: cpfp_tx_vsize,
        },
    ))
    .collect();
    let cpfp_utxos = vec![CpfpUtxo {
        keychain_id: domain_current_keychain_id.into(),
        outpoint,
        value: fee_bump_funding_sats,
        attributions,
    }];
    let sats_per_vbyte: f64 = 100.0;
    let fee = FeeRate::from_sat_per_vb(sats_per_vbyte as f32);
    let cfg = PsbtBuilderConfig::builder()
        .consolidate_deprecated_keychains(true)
        .fee_rate(BoundedFeeRate::unchecked(fee))
        .cpfp_utxos(
            vec![(KeychainId::from(domain_current_keychain_id), cpfp_utxos)]
                .into_iter()
                .collect(),
        )
        .build()
        .unwrap();
    let builder = PsbtBuilder::new(cfg);

    let domain_send_amount = wallet_funding_sats - Satoshis::from(100_000_000);
    let destination = Address::parse_from_trusted_source("mgWUuj1J1N882jmqFxtDepEC73Rr22E9GU");
    let payouts_one = vec![(Uuid::new_v4(), destination.clone(), domain_send_amount)];

    let builder = builder
        .wallet_payouts(domain_wallet_id, payouts_one)
        .accept_current_keychain();
    while !find_tx_id(&pool, domain_current_keychain_id, tx_id).await? {
        let blockchain = helpers::electrum_blockchain().await?;
        domain_current_keychain
            .sync(blockchain, test_progress_context())
            .await?;
    }
    let builder = domain_current_keychain
        .dispatch_bdk_wallet(builder)
        .await?
        .next_wallet();
    let FinishedPsbtBuild {
        psbt: unsigned_psbt,
        included_payouts,
        wallet_totals,
        fee_satoshis,
        estimated_vbytes,
        ..
    } = builder.finish();
    assert_eq!(
        included_payouts
            .get(&domain_wallet_id)
            .expect("wallet not included in payouts")
            .len(),
        1
    );
    assert_eq!(wallet_totals.len(), 1);
    let domain_wallet_total = wallet_totals.get(&domain_wallet_id).unwrap();
    assert_eq!(domain_wallet_total.output_satoshis, domain_send_amount);
    assert_eq!(
        domain_wallet_total.output_satoshis
            + domain_wallet_total.total_fee_satoshis
            + domain_wallet_total.change_satoshis,
        domain_wallet_total.input_satoshis
    );
    let cpfp_allocations = &domain_wallet_total.cpfp_allocations;
    assert_eq!(cpfp_allocations.len(), 1);

    let unsigned_psbt = unsigned_psbt.expect("unsigned psbt");
    let total_tx_outs = unsigned_psbt
        .unsigned_tx
        .output
        .iter()
        .fold(0, |acc, out| acc + out.value);
    let total_summary_outs = wallet_totals
        .values()
        .fold(Satoshis::from(0), |acc, total| {
            acc + total.output_satoshis + total.change_satoshis
        });
    assert_eq!(total_tx_outs, u64::from(total_summary_outs));
    assert_eq!(total_tx_outs, u64::from(total_summary_outs));
    let total_summary_fees = wallet_totals
        .values()
        .fold(Satoshis::from(0), |acc, total| {
            acc + total.total_fee_satoshis
        });
    assert_eq!(total_summary_fees, fee_satoshis);
    assert!(unsigned_psbt.inputs.len() >= 2);
    assert_eq!(unsigned_psbt.outputs.len(), 2);

    let mut lnd_client = helpers::lnd_signing_client().await?;
    let signed_psbt = lnd_client.sign_psbt(&unsigned_psbt).await?;
    let tx = domain_current_keychain
        .finalize_psbt(signed_psbt)
        .await?
        .expect("Finalize should have completed")
        .extract_tx();

    let size = tx.vsize();
    let actual_sats_per_vbyte = u64::from(fee_satoshis) as f64 / size as f64;
    assert!(actual_sats_per_vbyte > sats_per_vbyte);

    let combined_rate =
        u64::from(cpfp_tx_fee + fee_satoshis) as f64 / (size as f64 + cpfp_tx_vsize as f64);
    assert!(combined_rate >= sats_per_vbyte);
    assert!(combined_rate < sats_per_vbyte * 1.02);

    let sats_per_vbyte_without_cpfp_fees =
        (u64::from(fee_satoshis - domain_wallet_total.cpfp_fee_satoshis) as f64) / size as f64;
    assert!(sats_per_vbyte_without_cpfp_fees >= sats_per_vbyte);
    assert!(sats_per_vbyte_without_cpfp_fees < sats_per_vbyte * 1.02);

    // The batch fee guard compares a batch's fee against what this estimate
    // implies, so it has to be an upper bound that stays close to the signed
    // size.
    let estimated_vbytes = estimated_vbytes.expect("estimated_vbytes");
    assert!(
        estimated_vbytes >= size as u64,
        "estimated {estimated_vbytes} vbytes is below the signed size of {size}"
    );
    assert!(
        (estimated_vbytes as f64) < size as f64 * 1.05,
        "estimated {estimated_vbytes} vbytes is far above the signed size of {size}"
    );

    helpers::electrum_blockchain().await?.broadcast(&tx)?;
    Ok(())
}

#[tokio::test]
#[serial]
async fn build_psbt_with_min_change_output() -> anyhow::Result<()> {
    let pool = helpers::init_pool().await?;

    let domain_current_keychain_id = Uuid::new_v4();
    let domain_wallet_id = WalletId::new();
    let xpub = XPub::try_from(("tpubDD4vFnWuTMEcZiaaZPgvzeGyMzWe6qHW8gALk5Md9kutDvtdDjYFwzauEFFRHgov8pAwup5jX88j5YFyiACsPf3pqn5hBjvuTLRAseaJ6b4", Some("m/84'/0'/0'"))).unwrap();
    let keychain_cfg = KeychainConfig::wpkh(xpub);
    let domain_current_keychain = KeychainWallet::new(
        pool.clone(),
        Network::Regtest,
        domain_wallet_id,
        domain_current_keychain_id.into(),
        keychain_cfg,
    );
    let domain_addr = domain_current_keychain.new_external_address().await?;

    let bitcoind = helpers::bitcoind_client().await?;
    let wallet_funding = 100_000_000;
    let wallet_funding_sats = Satoshis::from(wallet_funding);
    helpers::fund_addr(&bitcoind, &domain_addr, wallet_funding)?;
    let tx_id = helpers::fund_addr(&bitcoind, &domain_addr, wallet_funding)?;
    helpers::gen_blocks(&bitcoind, 10)?;

    let sats_per_vbyte: f64 = 100.0;
    let fee = FeeRate::from_sat_per_vb(sats_per_vbyte as f32);
    let min_change = Satoshis::from(100_000_000);
    let cfg = PsbtBuilderConfig::builder()
        .consolidate_deprecated_keychains(true)
        .fee_rate(BoundedFeeRate::unchecked(fee))
        .force_min_change_output(Some(min_change))
        .build()
        .unwrap();
    let builder = PsbtBuilder::new(cfg);

    let domain_send_amount = wallet_funding_sats - Satoshis::from(50_000_000);
    let destination = Address::parse_from_trusted_source("mgWUuj1J1N882jmqFxtDepEC73Rr22E9GU");
    let payouts_one = vec![(Uuid::new_v4(), destination.clone(), domain_send_amount)];

    let builder = builder
        .wallet_payouts(domain_wallet_id, payouts_one)
        .accept_current_keychain();
    while !find_tx_id(&pool, domain_current_keychain_id, tx_id).await? {
        let blockchain = helpers::electrum_blockchain().await?;
        domain_current_keychain
            .sync(blockchain, test_progress_context())
            .await?;
    }
    let builder = domain_current_keychain
        .dispatch_bdk_wallet(builder)
        .await?
        .next_wallet();
    let FinishedPsbtBuild { wallet_totals, .. } = builder.finish();
    assert_eq!(wallet_totals.len(), 1);
    let domain_wallet_total = wallet_totals.get(&domain_wallet_id).unwrap();
    assert!(domain_wallet_total.change_satoshis >= min_change);

    Ok(())
}

#[tokio::test]
#[serial]
// max_satisfaction_weight is deprecated in miniscript, but it is the measure the
// builder sizes inputs with, so the expectation below has to use the same one.
#[allow(deprecated)]
async fn estimated_vbytes_keeps_each_inputs_own_weight() -> anyhow::Result<()> {
    // A batch that consolidates a 2-of-3 multisig deprecated keychain into a
    // single-sig current keychain. Every input ends up in one psbt, and the
    // current keychain is visited last, so a size estimate that applied the
    // last keychain's weight to all of them would understate the multisig
    // inputs. FeesClient::check_batch_fee compares a batch's fee against
    // `fee_rate * estimated_vbytes`, so understating the size reads a
    // legitimate fee as inconsistent and refuses the batch.
    let deprecated_keychain = helpers::random_multisig_bdk_wallet(2, 3)?;
    let current_keychain = helpers::random_bdk_wallet()?;

    let deprecated_weight = deprecated_keychain
        .get_descriptor_for_keychain(bdk::KeychainKind::External)
        .max_satisfaction_weight()
        .expect("Unsupported descriptor");
    let current_weight = current_keychain
        .get_descriptor_for_keychain(bdk::KeychainKind::External)
        .max_satisfaction_weight()
        .expect("Unsupported descriptor");
    assert!(
        deprecated_weight > current_weight,
        "this test is only meaningful with keychains of differing weight, got {deprecated_weight} and {current_weight}"
    );

    let deprecated_addr = deprecated_keychain.get_address(AddressIndex::New)?;
    let current_addr = current_keychain.get_address(AddressIndex::New)?;

    let bitcoind = helpers::bitcoind_client().await?;
    // Deliberately small: this test only cares about input weights, and modest
    // amounts keep it fundable however far the regtest subsidy has halved.
    helpers::fund_addr(&bitcoind, &deprecated_addr, 500_000)?;
    helpers::fund_addr(&bitcoind, &current_addr, 1_000_000)?;
    helpers::gen_blocks(&bitcoind, 10)?;

    let blockchain = helpers::electrum_blockchain().await?;
    for _ in 0..5 {
        deprecated_keychain.sync(&blockchain, Default::default())?;
        current_keychain.sync(&blockchain, Default::default())?;
        if deprecated_keychain.get_balance()?.get_spendable() > 0
            && current_keychain.get_balance()?.get_spendable() > 0
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    assert!(
        deprecated_keychain.get_balance()?.get_spendable() > 0,
        "deprecated keychain never saw its funding"
    );
    assert!(
        current_keychain.get_balance()?.get_spendable() > 0,
        "current keychain never saw its funding"
    );

    let cfg = PsbtBuilderConfig::builder()
        .consolidate_deprecated_keychains(true)
        .fee_rate(BoundedFeeRate::unchecked(FeeRate::from_sat_per_vb(5.0)))
        .build()
        .unwrap();

    let wallet_id = WalletId::new();
    let deprecated_keychain_id = uuid::uuid!("00000000-0000-0000-0000-000000000011").into();
    let current_keychain_id = uuid::uuid!("00000000-0000-0000-0000-000000000012").into();
    let destination = Address::parse_from_trusted_source("mgWUuj1J1N882jmqFxtDepEC73Rr22E9GU");
    let payouts = vec![(Uuid::new_v4(), destination, Satoshis::from(700_000_u64))];

    let FinishedPsbtBuild {
        psbt,
        estimated_vbytes,
        included_utxos,
        ..
    } = PsbtBuilder::new(cfg)
        .wallet_payouts(wallet_id, payouts)
        .visit_bdk_wallet(deprecated_keychain_id, &deprecated_keychain)?
        .accept_current_keychain()
        .visit_bdk_wallet(current_keychain_id, &current_keychain)?
        .finish();

    let keychain_utxos = included_utxos
        .get(&wallet_id)
        .expect("wallet not included in utxos");
    let deprecated_outpoints: std::collections::HashSet<_> = keychain_utxos
        .get(&deprecated_keychain_id)
        .expect("deprecated keychain not included in utxos")
        .iter()
        .copied()
        .collect();
    let current_outpoints: std::collections::HashSet<_> = keychain_utxos
        .get(&current_keychain_id)
        .expect("current keychain not included in utxos")
        .iter()
        .copied()
        .collect();
    assert!(!deprecated_outpoints.is_empty());
    assert!(!current_outpoints.is_empty());

    let psbt = psbt.expect("unsigned psbt");
    let mut expected_satisfaction = 0usize;
    for input in psbt.unsigned_tx.input.iter() {
        let outpoint = input.previous_output;
        if deprecated_outpoints.contains(&outpoint) {
            expected_satisfaction += deprecated_weight;
        } else {
            assert!(
                current_outpoints.contains(&outpoint),
                "input {outpoint} belongs to neither keychain"
            );
            expected_satisfaction += current_weight;
        }
    }
    // 2 for the segwit marker and flag, as the builder does
    let expected_vbytes = (psbt.unsigned_tx.weight()
        + bdk::bitcoin::Weight::from_wu((expected_satisfaction + 2) as u64))
    .to_vbytes_ceil();

    assert_eq!(estimated_vbytes, Some(expected_vbytes));

    Ok(())
}

async fn find_tx_id(
    pool: &sqlx::PgPool,
    keychain_id: Uuid,
    tx_id: bitcoin::Txid,
) -> anyhow::Result<bool> {
    let utxos = sqlx::query!(
        r#"SELECT count(*) as "count!" FROM bdk_utxos WHERE keychain_id = $1 AND tx_id = $2"#,
        keychain_id,
        tx_id.to_string(),
    )
    .fetch_one(pool)
    .await?;
    Ok(utxos.count != 0)
}
