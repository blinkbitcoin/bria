use std::time::Duration;

use bria::{fees::*, primitives::TxPriority};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn mempool_fee_response() -> serde_json::Value {
    serde_json::json!({
        "fastestFee": 20,
        "halfHourFee": 10,
        "hourFee": 5
    })
}

/// A 200 with an unparsable body makes the provider fail without triggering
/// the retry middleware, keeping test behavior deterministic.
fn unparsable_response() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_string("invalid json")
}

fn test_fees_config(mempool_url: String, blockstream_url: String) -> FeesConfig {
    FeesConfig {
        mempool_space: MempoolSpaceConfig {
            url: mempool_url,
            timeout: Duration::from_secs(5),
            number_of_retries: 0,
            rate_limit_per_second: 100,
            rate_limit_burst: 100,
        },
        blockstream: BlockstreamConfig {
            url: blockstream_url,
            timeout: Duration::from_secs(5),
            number_of_retries: 0,
            rate_limit_per_second: 100,
            rate_limit_burst: 100,
        },
        cache_ttl: Duration::from_secs(30),
        stale_ttl: Duration::from_secs(120),
        enable_stale_on_error: true,
    }
}

#[tokio::test]
#[ignore = "hits live external endpoint; run manually with --ignored"]
async fn mempool_space() -> anyhow::Result<()> {
    let mempool_space_config = MempoolSpaceConfig::default();
    let mempool_space = MempoolSpaceClient::new(mempool_space_config);
    let fee_rate = mempool_space.fee_rate(TxPriority::NextBlock).await?;
    assert!(fee_rate.as_sat_per_vb() > 0.0);
    Ok(())
}

#[tokio::test]
#[ignore = "hits live external endpoint; run manually with --ignored"]
async fn blockstream() -> anyhow::Result<()> {
    let blockstream_config = BlockstreamConfig::default();
    let blockstream = BlockstreamClient::new(blockstream_config);
    let fee_rate = blockstream.fee_rate(TxPriority::NextBlock).await?;
    assert!(fee_rate.as_sat_per_vb() > 0.0);
    Ok(())
}

#[tokio::test]
async fn cache_hit_avoids_upstream_call() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/fees/recommended"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mempool_fee_response()))
        .expect(1)
        .mount(&server)
        .await;

    let client = FeesClient::new(test_fees_config(server.uri(), "http://unused".to_string()));

    let first = client.fee_rate(TxPriority::NextBlock).await.unwrap();
    let second = client.fee_rate(TxPriority::NextBlock).await.unwrap();

    assert_eq!(first.as_sat_per_vb(), second.as_sat_per_vb());
}

#[tokio::test]
async fn stale_cache_returned_when_both_providers_fail() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/fees/recommended"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mempool_fee_response()))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/fees/recommended"))
        .respond_with(unparsable_response())
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/fee-estimates"))
        .respond_with(unparsable_response())
        .mount(&server)
        .await;

    // cache_ttl = 0 ensures the fresh cache always misses on the second call
    // without needing a sleep, while stale_ttl remains generous.
    let mut config = test_fees_config(server.uri(), server.uri());
    config.cache_ttl = Duration::ZERO;

    let client = FeesClient::new(config);
    let first = client.fee_rate(TxPriority::NextBlock).await.unwrap();
    let stale = client.fee_rate(TxPriority::NextBlock).await.unwrap();

    assert_eq!(first.as_sat_per_vb(), stale.as_sat_per_vb());
}

#[tokio::test]
async fn stale_cache_not_used_when_disabled() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/fees/recommended"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mempool_fee_response()))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/fees/recommended"))
        .respond_with(unparsable_response())
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/fee-estimates"))
        .respond_with(unparsable_response())
        .mount(&server)
        .await;

    let mut config = test_fees_config(server.uri(), server.uri());
    config.cache_ttl = Duration::ZERO;
    config.enable_stale_on_error = false;

    let client = FeesClient::new(config);
    client.fee_rate(TxPriority::NextBlock).await.unwrap();

    assert!(client.fee_rate(TxPriority::NextBlock).await.is_err());
}

// 100 ms delay gives the second task time to reach the priority lock
// before the first task's HTTP response arrives.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_requests_for_same_priority_coalesced() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/fees/recommended"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(mempool_fee_response())
                .set_delay(Duration::from_millis(100)),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = FeesClient::new(test_fees_config(server.uri(), "http://unused".to_string()));
    let client2 = client.clone();

    let (r1, r2) = tokio::join!(
        client.fee_rate(TxPriority::NextBlock),
        client2.fee_rate(TxPriority::NextBlock),
    );

    assert_eq!(r1.unwrap().as_sat_per_vb(), r2.unwrap().as_sat_per_vb());
}

// ---------------------------------------------------------------------------
// Characterization tests for blinkbitcoin/blink-wip#1101.
//
// These pin down the *unbounded* behavior of the fee estimators as it exists
// before the fix: whatever the HTTP endpoint says becomes the fee rate used to
// build (and drain-spend) batches. Phase 1 of the fix inverts them.
// ---------------------------------------------------------------------------

fn mempool_fee_response_with_fastest(fastest_fee: u64) -> serde_json::Value {
    serde_json::json!({
        "fastestFee": fastest_fee,
        "halfHourFee": 10,
        "hourFee": 5
    })
}

fn json_body(raw: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(raw.to_string(), "application/json")
}

async fn mempool_server_returning(body: serde_json::Value) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/fees/recommended"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .expect(1)
        .mount(&server)
        .await;
    server
}

fn blockstream_client(url: String) -> BlockstreamClient {
    BlockstreamClient::new(BlockstreamConfig {
        url,
        timeout: Duration::from_secs(5),
        number_of_retries: 0,
        rate_limit_per_second: 100,
        rate_limit_burst: 100,
    })
}

async fn blockstream_server_returning(raw: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/fee-estimates"))
        .respond_with(json_body(raw))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn absurdly_high_fee_rate_is_used_verbatim() {
    let server = mempool_server_returning(mempool_fee_response_with_fastest(100_000)).await;
    let client = FeesClient::new(test_fees_config(server.uri(), "http://unused".to_string()));

    let fee_rate = client.fee_rate(TxPriority::NextBlock).await.unwrap();

    assert_eq!(fee_rate.as_sat_per_vb(), 100_000.0);
}

#[tokio::test]
async fn zero_fee_rate_is_used_verbatim() {
    let server = mempool_server_returning(mempool_fee_response_with_fastest(0)).await;
    let client = FeesClient::new(test_fees_config(server.uri(), "http://unused".to_string()));

    let fee_rate = client.fee_rate(TxPriority::NextBlock).await.unwrap();

    assert_eq!(fee_rate.as_sat_per_vb(), 0.0);
}

#[tokio::test]
async fn absurdly_high_fee_rate_is_cached() {
    // `expect(1)` on the mock asserts the second call never reaches the server,
    // i.e. the unvetted value is served from the cache for the whole cache_ttl.
    let server = mempool_server_returning(mempool_fee_response_with_fastest(100_000)).await;
    let client = FeesClient::new(test_fees_config(server.uri(), "http://unused".to_string()));

    let first = client.fee_rate(TxPriority::NextBlock).await.unwrap();
    let second = client.fee_rate(TxPriority::NextBlock).await.unwrap();

    assert_eq!(first.as_sat_per_vb(), 100_000.0);
    assert_eq!(second.as_sat_per_vb(), 100_000.0);
}

#[tokio::test]
#[should_panic(expected = "is_sign_positive")]
async fn blockstream_negative_fee_rate_panics() {
    let server = blockstream_server_returning(r#"{"1": -5.0, "3": 10.0, "6": 5.0}"#).await;

    let _ = blockstream_client(server.uri())
        .fee_rate(TxPriority::NextBlock)
        .await;
}

#[tokio::test]
#[should_panic(expected = "is_normal")]
async fn blockstream_f32_overflowing_fee_rate_panics() {
    // 1e40 does not fit in f32: serde silently saturates it to +inf.
    let server = blockstream_server_returning(r#"{"1": 1e40, "3": 10.0, "6": 5.0}"#).await;

    let _ = blockstream_client(server.uri())
        .fee_rate(TxPriority::NextBlock)
        .await;
}

#[tokio::test]
#[should_panic(expected = "is_normal")]
async fn blockstream_subnormal_fee_rate_panics() {
    let server = blockstream_server_returning(r#"{"1": 1e-45, "3": 10.0, "6": 5.0}"#).await;

    let _ = blockstream_client(server.uri())
        .fee_rate(TxPriority::NextBlock)
        .await;
}
