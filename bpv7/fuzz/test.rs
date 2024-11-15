/*
#[cfg(test)]
use hardy_bpv7::prelude::*;

#[test]
fn test() {
    let data = include_bytes!("artifacts/bundle/crash-8aa61d901e8d1de6a6a3784633f9a676dbd3f358");

    println!("Original: {:02x?}", &data);

    let mut f = |_: &Eid, _| Ok(None);

    if let Ok(ValidBundle::Rewritten(_, data, _)) = ValidBundle::parse(data, &mut f) {
        println!("Rewrite: {:02x?}", &data);

        match ValidBundle::parse(&data, &mut f) {
            Ok(ValidBundle::Valid(..)) => {}
            Ok(ValidBundle::Rewritten(..)) => panic!("Rewrite produced non-canonical results"),
            Ok(ValidBundle::Invalid(_, _, e)) => panic!("Rewrite produced invalid results: {e}"),
            Err(_) => panic!("Rewrite errored"),
        };
    }
}
*/
