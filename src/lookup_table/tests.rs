use super::*;
use solana_address_lookup_table_interface::state::LookupTableMeta;
use solana_instruction::{AccountMeta, Instruction};
use solana_message::v0;
use std::borrow::Cow;

fn table(addresses: Vec<Pubkey>) -> AddressLookupTableAccount {
    AddressLookupTableAccount {
        key: Pubkey::new_unique(),
        addresses,
    }
}

fn serialized_table(meta: LookupTableMeta) -> Vec<u8> {
    AddressLookupTable {
        meta,
        addresses: Cow::Owned(vec![Pubkey::new_unique()]),
    }
    .serialize_for_tests()
    .unwrap()
}

#[test]
fn merges_unique_tables_with_primary_snapshots_first() {
    let shared = table(vec![Pubkey::new_unique()]);
    let extra = table(vec![Pubkey::new_unique()]);
    let refreshed = AddressLookupTableAccount {
        key: shared.key,
        addresses: vec![Pubkey::new_unique()],
    };
    let merged = merge_lookup_tables(
        &[extra.clone(), refreshed.clone(), extra.clone()],
        &[shared],
    );
    assert_eq!(merged.len(), 2);
    assert_eq!(merged[0].key, extra.key);
    assert_eq!(merged[1].addresses, refreshed.addresses);
}

#[test]
fn accepts_active_and_frozen_tables_but_rejects_warmup_or_deactivation() {
    let address = Pubkey::new_unique();
    let meta = LookupTableMeta {
        last_extended_slot: 10,
        ..Default::default()
    };
    let data = serialized_table(meta.clone());
    assert!(decode_lookup_table(address, program::ID, &data, 11).is_ok());
    assert!(decode_lookup_table(address, program::ID, &data, 10).is_err());
    assert!(decode_lookup_table(address, program::ID, &data, 9).is_err());
    let deactivating = serialized_table(LookupTableMeta {
        deactivation_slot: 11,
        ..meta
    });
    assert!(decode_lookup_table(address, program::ID, &deactivating, 12).is_err());
}

#[test]
fn rejects_wrong_owner_and_malformed_lookup_accounts() {
    let address = Pubkey::new_unique();
    let data = serialized_table(LookupTableMeta::default());
    assert!(decode_lookup_table(address, Pubkey::new_unique(), &data, 10).is_err());
    assert!(decode_lookup_table(address, program::ID, &[1, 2, 3], 10).is_err());
}

#[test]
fn compiler_uses_matching_tables_and_leaves_other_addresses_inline() {
    let payer = Pubkey::new_unique();
    let venue = Pubkey::new_unique();
    let first = Pubkey::new_unique();
    let second = Pubkey::new_unique();
    let inline = Pubkey::new_unique();
    let unrelated = table(vec![Pubkey::new_unique()]);
    let shared = table(vec![payer, venue, first]);
    let bridge = table(vec![first, second]);
    let redundant = table(vec![first, second]);
    let instruction = Instruction {
        program_id: venue,
        accounts: vec![
            AccountMeta::new(first, false),
            AccountMeta::new_readonly(second, false),
            AccountMeta::new(inline, false),
        ],
        data: vec![],
    };
    let message = v0::Message::try_compile(
        &payer,
        &[instruction],
        &[unrelated, shared.clone(), bridge.clone(), redundant],
        Default::default(),
    )
    .unwrap();
    assert_eq!(message.address_table_lookups.len(), 2);
    assert_eq!(message.address_table_lookups[0].account_key, shared.key);
    assert_eq!(message.address_table_lookups[0].writable_indexes, vec![2]);
    assert_eq!(message.address_table_lookups[1].account_key, bridge.key);
    assert_eq!(message.address_table_lookups[1].readonly_indexes, vec![1]);
    assert!(message.account_keys.contains(&payer));
    assert!(message.account_keys.contains(&venue));
    assert!(message.account_keys.contains(&inline));
}
