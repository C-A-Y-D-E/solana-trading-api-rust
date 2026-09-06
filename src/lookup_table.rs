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
    let account = rpc
        .get_account(&address)
        .await
        .map_err(|source| TradeError::Rpc {
            context: "get address lookup table",
            source,
        })?;
    if !program::check_id(&account.owner) {
        return Err(TradeError::Decode(
            "address lookup table",
            format!("account {address} has owner {}", account.owner),
        ));
    }
    let table = AddressLookupTable::deserialize(&account.data).map_err(|error| {
        TradeError::Decode(
            "address lookup table",
            format!("account {address}: {error}"),
        )
    })?;
    Ok(AddressLookupTableAccount {
        key: address,
        addresses: table.addresses.into_owned(),
    })
}
