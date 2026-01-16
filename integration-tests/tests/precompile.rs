use alloy::primitives::U256;
use zksync_os_integration_tests::Tester;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
use zksync_os_integration_tests::contracts::P256GasRecorder;

#[test_log::test(tokio::test)]
async fn p256_precompile_records_remaining_gas() -> anyhow::Result<()> {
    // Regression test for revm-consistency-checker issue with wrong gas for p256 precompile.
    let tester = Tester::setup().await?;

    let contract = P256GasRecorder::deploy(tester.l2_provider.clone()).await?;

    contract
        .callP256()
        .send()
        .await?
        .expect_successful_receipt()
        .await?;

    let call_success = contract.lastSuccess().call().await?;
    let gas_after_call = contract.lastGasAfterCall().call().await?;

    assert!(call_success, "p256 precompile call should succeed");
    assert!(
        gas_after_call > U256::ZERO,
        "gas left after p256 call should be recorded"
    );

    Ok(())
}
