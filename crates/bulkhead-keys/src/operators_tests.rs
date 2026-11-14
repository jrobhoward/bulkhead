#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;
use crate::key::generate_key;

#[test]
fn parse_operators____names_and_keys____read_or_refused_with_the_name() {
    let (_, alice) = generate_key().unwrap();

    let good = parse_operators(&format!("[operators]\nalice = [\"{alice}\"]\n")).unwrap();
    let bad = parse_operators("[operators]\nbob = [\"rsa:nope\"]\n").unwrap_err();

    assert_eq!(good.get("alice"), Some(&vec![alice]));
    assert!(bad.starts_with("operators.bob:"), "{bad}");
}

#[test]
fn operator_of____a_listed_key_and_another____its_name_or_none() {
    let (_, alice) = generate_key().unwrap();
    let (_, stranger) = generate_key().unwrap();
    let operators = BTreeMap::from([("alice".to_string(), vec![alice.clone()])]);

    assert_eq!(operator_of(&operators, &alice), Some("alice".into()));
    assert_eq!(operator_of(&operators, &stranger), None);
}
