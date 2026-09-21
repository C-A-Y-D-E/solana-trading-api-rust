use super::*;

#[test]
fn exact_input_compares_average_execution_to_spot_not_post_trade_price() {
    assert_eq!(exact_input(900, 2_000, 100), Some(1_000.0));
    assert_eq!(exact_input(900, 2_000_000, 100), Some(1_000.0));
}

#[test]
fn larger_inputs_have_more_impact_without_overflow() {
    assert!(exact_input(1_000, 2_000, 100) < exact_input(1_000, 2_000, 500));
    assert_eq!(exact_input(u64::MAX, u64::MAX, u64::MAX), Some(5_000.0));
}

#[test]
fn exact_output_uses_requested_output_not_maximum_input_budget() {
    assert_eq!(exact_output(2_000, 200), Some(1_000.0));
}

#[test]
fn empty_reserves_or_unpriceable_amounts_are_unknown() {
    assert_eq!(exact_input(0, 1_000, 100), None);
    assert_eq!(exact_input(1_000, 0, 100), None);
    assert_eq!(exact_input(1_000, 1_000, 0), None);
    assert_eq!(exact_output(0, 100), None);
    assert_eq!(exact_output(100, 100), None);
    assert_eq!(exact_output(100, 101), None);
    assert_eq!(exact_output(100, 0), None);
}

#[test]
fn routes_compound_both_hops_instead_of_adding_percentages() {
    assert_eq!(combine(Some(1_000.0), Some(2_000.0)), Some(2_800.0));
    assert_eq!(combine(Some(0.0), Some(200.0)), Some(200.0));
    assert_eq!(combine(None, Some(200.0)), None);
    assert_eq!(combine(Some(200.0), None), None);
}
