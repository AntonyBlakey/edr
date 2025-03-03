use core::fmt::Debug;

use edr_eth::{BlockSpec, Bytes, SpecId, U256};
use edr_evm::{state::StateOverrides, trace::Trace, transaction};
use edr_rpc_eth::{CallRequest, StateOverrideOptions};

use crate::{
    data::ProviderData, requests::validation::validate_call_request, time::TimeSinceEpoch,
    ProviderError, TransactionFailure,
};

pub fn handle_call_request<LoggerErrorT: Debug, TimerT: Clone + TimeSinceEpoch>(
    data: &mut ProviderData<LoggerErrorT, TimerT>,
    request: CallRequest,
    block_spec: Option<BlockSpec>,
    state_overrides: Option<StateOverrideOptions>,
) -> Result<(Bytes, Trace), ProviderError<LoggerErrorT>> {
    let block_spec = resolve_block_spec_for_call_request(block_spec);
    validate_call_request(data.spec_id(), &request, &block_spec)?;

    let state_overrides =
        state_overrides.map_or(Ok(StateOverrides::default()), StateOverrides::try_from)?;

    let transaction = resolve_call_request(data, request, &block_spec, &state_overrides)?;
    let result = data.run_call(transaction.clone(), &block_spec, &state_overrides)?;

    let spec_id = data.spec_id();
    data.logger_mut()
        .log_call(spec_id, &transaction, &result)
        .map_err(ProviderError::Logger)?;

    if data.bail_on_call_failure() {
        if let Some(failure) =
            TransactionFailure::from_execution_result(&result.execution_result, None, &result.trace)
        {
            return Err(ProviderError::TransactionFailed(
                crate::error::TransactionFailureWithTraces {
                    failure,
                    traces: vec![result.trace],
                },
            ));
        }
    }

    let output = result.execution_result.into_output().unwrap_or_default();
    Ok((output, result.trace))
}

pub(crate) fn resolve_block_spec_for_call_request(block_spec: Option<BlockSpec>) -> BlockSpec {
    block_spec.unwrap_or_else(BlockSpec::latest)
}

pub fn resolve_call_request<LoggerErrorT: Debug, TimerT: Clone + TimeSinceEpoch>(
    data: &mut ProviderData<LoggerErrorT, TimerT>,
    request: CallRequest,
    block_spec: &BlockSpec,
    state_overrides: &StateOverrides,
) -> Result<transaction::Signed, ProviderError<LoggerErrorT>> {
    resolve_call_request_inner(
        data,
        request,
        block_spec,
        state_overrides,
        |_data| Ok(U256::ZERO),
        |_, max_fee_per_gas, max_priority_fee_per_gas| {
            let max_fee_per_gas = max_fee_per_gas
                .or(max_priority_fee_per_gas)
                .unwrap_or(U256::ZERO);

            let max_priority_fee_per_gas = max_priority_fee_per_gas.unwrap_or(U256::ZERO);

            Ok((max_fee_per_gas, max_priority_fee_per_gas))
        },
    )
}

pub(crate) fn resolve_call_request_inner<LoggerErrorT: Debug, TimerT: Clone + TimeSinceEpoch>(
    data: &mut ProviderData<LoggerErrorT, TimerT>,
    request: CallRequest,
    block_spec: &BlockSpec,
    state_overrides: &StateOverrides,
    default_gas_price_fn: impl FnOnce(
        &ProviderData<LoggerErrorT, TimerT>,
    ) -> Result<U256, ProviderError<LoggerErrorT>>,
    max_fees_fn: impl FnOnce(
        &ProviderData<LoggerErrorT, TimerT>,
        // max_fee_per_gas
        Option<U256>,
        // max_priority_fee_per_gas
        Option<U256>,
    ) -> Result<(U256, U256), ProviderError<LoggerErrorT>>,
) -> Result<transaction::Signed, ProviderError<LoggerErrorT>> {
    let CallRequest {
        from,
        to,
        gas,
        gas_price,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        value,
        data: input,
        access_list,
        // We ignore the transaction type
        transaction_type: _transaction_type,
        blobs: _blobs,
        blob_hashes: _blob_hashes,
        authorization_list,
    } = request;

    let chain_id = data.chain_id_at_block_spec(block_spec)?;
    let from = from.unwrap_or_else(|| data.default_caller());
    let gas_limit = gas.unwrap_or_else(|| data.block_gas_limit());
    let input = input.map_or(Bytes::new(), Bytes::from);
    let nonce = data.nonce(&from, Some(block_spec), state_overrides)?;
    let value = value.unwrap_or(U256::ZERO);

    let transaction = if data.spec_id() < SpecId::LONDON || gas_price.is_some() {
        let gas_price = gas_price.map_or_else(|| default_gas_price_fn(data), Ok)?;
        match access_list {
            Some(access_list) if data.spec_id() >= SpecId::BERLIN => {
                transaction::Request::Eip2930(transaction::request::Eip2930 {
                    nonce,
                    gas_price,
                    gas_limit,
                    value,
                    input,
                    kind: to.into(),
                    chain_id,
                    access_list,
                })
            }
            _ => transaction::Request::Eip155(transaction::request::Eip155 {
                nonce,
                gas_price,
                gas_limit,
                kind: to.into(),
                value,
                input,
                chain_id,
            }),
        }
    } else {
        let (max_fee_per_gas, max_priority_fee_per_gas) =
            max_fees_fn(data, max_fee_per_gas, max_priority_fee_per_gas)?;

        if let Some(authorization_list) = authorization_list {
            transaction::Request::Eip7702(transaction::request::Eip7702 {
                chain_id,
                nonce,
                max_fee_per_gas,
                max_priority_fee_per_gas,
                gas_limit,
                to: to.ok_or(ProviderError::Eip7702TransactionMissingReceiver)?,
                value,
                input,
                access_list: access_list.unwrap_or_default(),
                authorization_list,
            })
        } else {
            transaction::Request::Eip1559(transaction::request::Eip1559 {
                chain_id,
                nonce,
                max_fee_per_gas,
                max_priority_fee_per_gas,
                gas_limit,
                kind: to.into(),
                value,
                input,
                access_list: access_list.unwrap_or_default(),
            })
        }
    };

    let transaction = transaction.fake_sign(from);

    transaction::validate(transaction, SpecId::LATEST)
        .map_err(ProviderError::TransactionCreationError)
}

#[cfg(test)]
mod tests {
    use edr_eth::transaction::Transaction;

    use super::*;
    use crate::{data::test_utils::ProviderTestFixture, test_utils::pending_base_fee};

    #[test]
    fn resolve_call_request_inner_with_gas_price() -> anyhow::Result<()> {
        let mut fixture = ProviderTestFixture::new_local()?;

        let pending_base_fee = pending_base_fee(&mut fixture.provider_data)?;

        let request = CallRequest {
            from: Some(fixture.nth_local_account(0)?),
            to: Some(fixture.nth_local_account(1)?),
            gas_price: Some(pending_base_fee),
            ..CallRequest::default()
        };

        let resolved = resolve_call_request_inner(
            &mut fixture.provider_data,
            request,
            &BlockSpec::pending(),
            &StateOverrides::default(),
            |_data| unreachable!("gas_price is set"),
            |_, _, _| unreachable!("gas_price is set"),
        )?;

        assert_eq!(resolved.gas_price(), pending_base_fee);

        Ok(())
    }

    #[test]
    fn resolve_call_request_inner_with_max_fee_and_max_priority_fee() -> anyhow::Result<()> {
        let mut fixture = ProviderTestFixture::new_local()?;

        let max_fee_per_gas = pending_base_fee(&mut fixture.provider_data)?;
        let max_priority_fee_per_gas = Some(max_fee_per_gas / U256::from(2));

        let request = CallRequest {
            from: Some(fixture.nth_local_account(0)?),
            to: Some(fixture.nth_local_account(1)?),
            max_fee_per_gas: Some(max_fee_per_gas),
            max_priority_fee_per_gas,
            ..CallRequest::default()
        };

        let resolved = resolve_call_request_inner(
            &mut fixture.provider_data,
            request,
            &BlockSpec::pending(),
            &StateOverrides::default(),
            |_data| unreachable!("max fees are set"),
            |_, max_fee_per_gas, max_priority_fee_per_gas| {
                Ok((
                    max_fee_per_gas.expect("max fee is set"),
                    max_priority_fee_per_gas.expect("max priority fee is set"),
                ))
            },
        )?;

        assert_eq!(resolved.gas_price(), max_fee_per_gas);
        assert_eq!(
            resolved.max_priority_fee_per_gas(),
            max_priority_fee_per_gas
        );

        Ok(())
    }
}
