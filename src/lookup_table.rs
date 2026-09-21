use solana_address_lookup_table_interface::{program, state::AddressLookupTable};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_message::AddressLookupTableAccount;
use solana_pubkey::Pubkey;

use crate::error::{Result, TradeError};

/// Reusable across target tokens and wallets; refresh if fee recipients or the bridge change.
pub async fn shared_lookup_addresses(
    rpc: std::sync::Arc<RpcClient>,
    bridge_pool: Pubkey,
) -> anyhow::Result<Vec<Pubkey>> {
    use crate::dexes::common::*;
    let pumpfun = crate::PumpFun::new(rpc.clone());
    let pumpswap = crate::PumpSwap::new(rpc).with_sol_usdc_pool(bridge_pool);
    let (curve_addresses, swap_addresses) = tokio::try_join!(
        pumpfun.shared_lookup_addresses(),
        pumpswap.shared_lookup_addresses(),
    )?;
    let mut addresses = vec![
        SYSTEM_PROGRAM,
        TOKEN_PROGRAM,
        TOKEN_2022_PROGRAM,
        ATA_PROGRAM,
        COMPUTE_BUDGET_PROGRAM,
        WSOL,
        crate::USDC_MINT,
        crate::BloxrouteSubmitter::DEFAULT_TIP_ACCOUNT,
    ];
    addresses.extend(curve_addresses);
    addresses.extend(swap_addresses);
    let mut seen = std::collections::HashSet::new();
    addresses.retain(|address| seen.insert(*address));
    Ok(addresses)
}

pub async fn load_address_lookup_table(
    rpc: &RpcClient,
    address: Pubkey,
) -> Result<AddressLookupTableAccount> {
    let response = rpc
        .get_account_with_commitment(&address, rpc.commitment())
        .await
        .map_err(|source| TradeError::Rpc {
            context: "get address lookup table",
            source,
        })?;
    let account = response.value.ok_or_else(|| {
        TradeError::Decode(
            "address lookup table",
            format!("account {address} does not exist on this RPC cluster"),
        )
    })?;
    decode_lookup_table(address, account.owner, &account.data, response.context.slot)
}

fn decode_lookup_table(
    address: Pubkey,
    owner: Pubkey,
    data: &[u8],
    slot: u64,
) -> Result<AddressLookupTableAccount> {
    if !program::check_id(&owner) {
        return Err(TradeError::Decode(
            "address lookup table",
            format!("account {address} has owner {owner}"),
        ));
    }
    let table = AddressLookupTable::deserialize(data).map_err(|error| {
        TradeError::Decode(
            "address lookup table",
            format!("account {address}: {error}"),
        )
    })?;
    // Cache only fully warmed, non-deactivating tables; cached state is not revalidated on each trade.
    if table.meta.deactivation_slot != u64::MAX || slot <= table.meta.last_extended_slot {
        return Err(TradeError::Decode(
            "address lookup table",
            format!(
                "account {address} is deactivating or not warmed up at RPC slot {slot}; use an active table and retry after its extension slot"
            ),
        ));
    }
    Ok(AddressLookupTableAccount {
        key: address,
        addresses: table.addresses.into_owned(),
    })
}

pub async fn load_address_lookup_tables(
    rpc: &RpcClient,
    addresses: &[Pubkey],
) -> Result<Vec<AddressLookupTableAccount>> {
    let mut tables = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for &address in addresses {
        if seen.insert(address) {
            tables.push(load_address_lookup_table(rpc, address).await?);
        }
    }
    Ok(tables)
}

pub(crate) fn merge_lookup_tables(
    primary: &[AddressLookupTableAccount],
    supplemental: &[AddressLookupTableAccount],
) -> Vec<AddressLookupTableAccount> {
    let mut seen = std::collections::HashSet::new();
    primary
        .iter()
        .chain(supplemental)
        .filter(|table| seen.insert(table.key))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests;
