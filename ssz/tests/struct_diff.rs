use derive_more::Eq;
use ssz::{H256, Ssz, SszDiff};

#[test]
fn primitive_diffs() {
    let value = 0u8;
    let value2 = 5u8;

    let diff = value.diff(&value2);

    let mut res = value;
    res.apply(&diff);
    assert_eq!(res, value2);
}

#[test]
fn simple_struct_diff() {
    #[derive(Ssz, Debug, Clone, PartialEq, Eq)]
    #[ssz(derive_diff = true)]
    struct SomeStruct {
        value: u8,
    }

    let initial = SomeStruct { value: 0 };
    let result = SomeStruct { value: 5 };

    let diff = initial.diff(&result);

    let mut received = initial.clone();
    received.apply(&diff);

    assert_eq!(result, received);
}
