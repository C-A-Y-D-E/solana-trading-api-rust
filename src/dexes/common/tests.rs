use super::anchor_discriminator;

#[test]
fn discriminators_match_idl() {
    assert_eq!(
        anchor_discriminator("buy_exact_sol_in"),
        [56, 252, 116, 8, 158, 223, 205, 95]
    );
    assert_eq!(
        anchor_discriminator("buy_exact_quote_in"),
        [198, 46, 21, 82, 180, 217, 232, 112]
    );
    assert_eq!(
        anchor_discriminator("sell"),
        [51, 230, 133, 164, 1, 127, 131, 173]
    );
}
