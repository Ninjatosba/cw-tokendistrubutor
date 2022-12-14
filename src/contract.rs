use cosmwasm_std::{
    attr, entry_point, from_binary, to_binary, Binary, Decimal, Deps, DepsMut, Env, MessageInfo,
    Response, StdError, StdResult, Uint128, WasmMsg, WasmQuery,
};

use crate::msg::{
    AccruedRewardsResponse, ExecuteMsg, HolderResponse, HoldersResponse, InstantiateMsg,
    MigrateMsg, QueryMsg, ReceiveMsg, StateResponse,
};
use crate::state::{list_accrued_rewards, Holder, State, CLAIMS, HOLDERS, STATE};
use crate::ContractError;
use cw20::{BalanceResponse, Cw20ExecuteMsg, Cw20QueryMsg, Cw20ReceiveMsg};
use cw_controllers::ClaimsResponse;
use std::ops::{Add, Mul, Sub};
use std::str::FromStr;

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    _env: Env,
    _info: MessageInfo,
    msg: InstantiateMsg,
) -> StdResult<Response> {
    deps.api.addr_validate(&msg.token_address.as_str())?;
    let state = State {
        token_address: msg.token_address,
        global_index: Decimal::zero(),
        total_balance: Uint128::zero(),
        prev_reward_balance: Uint128::zero(),
    };
    STATE.save(deps.storage, &state)?;

    Ok(Response::default())
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, ContractError> {
    match msg {
        ExecuteMsg::UpdateRewardIndex {} => execute_update_reward_index(deps, env),
        ExecuteMsg::UnbondStake { amount } => execute_unbound(deps, env, info, amount),
        ExecuteMsg::WithdrawStake { cap } => execute_withdraw_stake(deps, env, info, cap),
        ExecuteMsg::Receive(msg) => execute_receive(deps, env, info, msg),
    }
}

/// Increase global_index according to claimed rewards amount
pub fn execute_update_reward_index(deps: DepsMut, env: Env) -> Result<Response, ContractError> {
    let mut state = STATE.load(deps.storage)?;

    // Zero staking balance check
    if state.total_balance.is_zero() {
        return Err(ContractError::NoBond {});
    }

    // For querying the balance of the contract itself, we can use the querier
    let current_balance: Uint128 = deps
        .querier
        .query_balance(&env.contract.address, &state.token_address)?;
    let previous_balance = state.prev_reward_balance;

    // claimed_rewards = current_balance - prev_balance;
    let claimed_rewards = balance_res.balance.checked_sub(previous_balance)?;

    state.prev_reward_balance = current_balance;

    // global_index += claimed_rewards / total_balance;
    state.global_index = state
        .global_index
        .add(Decimal::from_ratio(claimed_rewards, state.total_balance));

    STATE.save(deps.storage, &state)?;

    let res = Response::new()
        .add_attribute("action", "update_reward_index")
        .add_attribute("claimed_rewards", claimed_rewards)
        .add_attribute("new_index", state.global_index.to_string());

    Ok(res)
}

pub fn execute_receive(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: Cw20ReceiveMsg,
) -> Result<Response, ContractError> {
    let config = STATE.load(deps.storage)?;

    if info.sender != config.token_address {
        return Err(ContractError::Unauthorized {});
    }

    let msg: ReceiveMsg = from_binary(&wrapper.msg)?;
    match msg {
        ReceiveMsg::BondStake {} => execute_bond(deps, env, info, wrapper.sender, wrapper.amount),
        ReceiveMsg::UpdateRewardIndex {} => execute_update_reward_index(deps, env),
    }
}

pub fn execute_bond(
    deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    holder_addr: String,
    amount: Uint128,
) -> Result<Response, ContractError> {
    if !info.funds.is_empty() {
        return Err(ContractError::DoNotSendFunds {});
    }
    if amount.is_zero() {
        return Err(ContractError::AmountRequired {});
    }

    let addr = deps.api.addr_validate(&holder_addr.as_str())?;
    let mut state = STATE.load(deps.storage)?;

    let mut holder = HOLDERS.may_load(deps.storage, &addr)?.unwrap_or(Holder {
        balance: Uint128::zero(),
        index: Decimal::zero(),
        pending_rewards: Decimal::zero(),
    });

    // get decimals
    //in new bonding rewards=global_index*balance
    let rewards = calculate_decimal_rewards(state.global_index, holder.index, holder.balance)?;

    holder.index = state.global_index;
    holder.pending_rewards = rewards.sub(holder.pending_rewards);
    holder.balance = amount;
    // save reward and index
    HOLDERS.save(deps.storage, &addr, &holder)?;

    state.total_balance += amount;
    STATE.save(deps.storage, &state)?;

    let res = Response::new()
        .add_attribute("action", "bond_stake")
        .add_attribute("holder_address", holder_addr)
        .add_attribute("amount", amount);

    Ok(res)
}

pub fn execute_unbound(
    deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    amount: Uint128,
) -> Result<Response, ContractError> {
    let mut state = STATE.load(deps.storage)?;

    if !info.funds.is_empty() {
        return Err(ContractError::DoNotSendFunds {});
    }
    if amount.is_zero() {
        return Err(ContractError::AmountRequired {});
    }

    let mut holder = HOLDERS.load(deps.storage, &info.sender)?;
    if holder.balance < amount {
        return Err(ContractError::DecreaseAmountExceeds(holder.balance));
    }

    let rewards = calculate_decimal_rewards(state.global_index, holder.index, holder.balance)?;

    holder.index = state.global_index;
    holder.pending_rewards = rewards.add(holder.pending_rewards);
    holder.balance = (holder.balance.checked_sub(amount))?;
    state.total_balance = (state.total_balance.checked_sub(amount))?;

    STATE.save(deps.storage, &state)?;
    HOLDERS.save(deps.storage, &info.sender, &holder)?;

    let attributes = vec![
        attr("action", "unbound"),
        attr("holder_address", info.sender),
        attr("amount", amount),
    ];

    Ok(Response::new().add_attributes(attributes))
}

pub fn execute_withdraw_stake(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    cap: Option<Uint128>,
) -> Result<Response, ContractError> {
    let state = STATE.load(deps.storage)?;

    let amount = CLAIMS.claim_tokens(deps.storage, &info.sender, &env.block, cap)?;
    if amount.is_zero() {
        return Err(ContractError::WaitUnbonding {});
    }

    let cw20_transfer_msg = Cw20ExecuteMsg::Transfer {
        recipient: info.sender.to_string(),
        amount,
    };
    let msg = WasmMsg::Execute {
        contract_addr: state.token_address,
        msg: to_binary(&cw20_transfer_msg)?,
        funds: vec![],
    };

    Ok(Response::new()
        .add_message(msg)
        .add_attribute("action", "withdraw_stake")
        .add_attribute("holder_address", &info.sender)
        .add_attribute("amount", amount))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, _env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::State {} => to_binary(&query_state(deps, _env, msg)?),
        QueryMsg::AccruedRewards { address } => to_binary(&query_accrued_rewards(deps, address)?),
        QueryMsg::Holder { address } => to_binary(&query_holder(deps, address)?),
        QueryMsg::Holders { start_after, limit } => {
            to_binary(&query_holders(deps, start_after, limit)?)
        }
        QueryMsg::Claims { address } => to_binary(&query_claims(deps, address)?),
    }
}

pub fn query_state(deps: Deps, _env: Env, _msg: QueryMsg) -> StdResult<StateResponse> {
    let state = STATE.load(deps.storage)?;

    Ok(StateResponse {
        token_address: state.token_address,
        global_index: state.global_index,
        total_balance: state.total_balance,
        prev_reward_balance: state.prev_reward_balance,
    })
}

pub fn query_accrued_rewards(deps: Deps, address: String) -> StdResult<AccruedRewardsResponse> {
    let state = STATE.load(deps.storage)?;

    let addr = deps.api.addr_validate(address.as_str())?;
    let holder = HOLDERS.load(deps.storage, &addr)?;
    let reward_with_decimals =
        calculate_decimal_rewards(state.global_index, holder.index, holder.balance)?;
    let all_reward_with_decimals = reward_with_decimals.add(holder.pending_rewards);

    let rewards = all_reward_with_decimals * Uint128::new(1);

    Ok(AccruedRewardsResponse { rewards })
}

pub fn query_holder(deps: Deps, address: String) -> StdResult<HolderResponse> {
    let holder: Holder = HOLDERS.load(deps.storage, &deps.api.addr_validate(address.as_str())?)?;
    Ok(HolderResponse {
        address,
        balance: holder.balance,
        index: holder.index,
        pending_rewards: holder.pending_rewards,
    })
}

pub fn query_holders(
    deps: Deps,
    start_after: Option<String>,
    limit: Option<u32>,
) -> StdResult<HoldersResponse> {
    let start_after = if let Some(start_after) = start_after {
        Some(deps.api.addr_validate(&start_after)?)
    } else {
        None
    };

    let holders: Vec<HolderResponse> = list_accrued_rewards(deps, start_after, limit)?;

    Ok(HoldersResponse { holders })
}

pub fn query_claims(deps: Deps, addr: String) -> StdResult<ClaimsResponse> {
    Ok(CLAIMS.query_claims(deps, &deps.api.addr_validate(addr.as_str())?)?)
}

// calculate the reward based on the sender's index and the global index.
pub fn calculate_decimal_rewards(
    global_index: Decimal,
    user_index: Decimal,
    user_balance: Uint128,
) -> StdResult<Decimal> {
    let decimal_balance = Decimal::from_ratio(user_balance, Uint128::new(1));

    Ok(global_index.sub(user_index).mul(decimal_balance))
}

// calculate the reward with decimal
pub fn get_decimals(value: Decimal) -> StdResult<Decimal> {
    let stringed: &str = &*value.to_string();
    let parts: &[&str] = &*stringed.split('.').collect::<Vec<&str>>();
    match parts.len() {
        1 => Ok(Decimal::zero()),
        2 => {
            let decimals = Decimal::from_str(&*("0.".to_owned() + parts[1]))?;
            Ok(decimals)
        }
        _ => Err(StdError::generic_err("Unexpected number of dots")),
    }
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn migrate(_deps: DepsMut, _env: Env, _msg: MigrateMsg) -> StdResult<Response> {
    Ok(Response::default())
}
