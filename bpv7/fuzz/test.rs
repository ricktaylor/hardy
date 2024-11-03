/*
#[cfg(test)]
use {hardy_bpv7::prelude::*, std::io::Write};

#[test]
fn test() {
    let data = include_bytes!("artifacts/bundle/crash-163063bb421ce2e262c93a7c22409061f9ce7242");
    //include_bytes!("rewritten_bundle");

    let mut f = |_: &Eid| Ok(None);

    if let Ok(ValidBundle::Rewritten(_, data, _)) = ValidBundle::parse(data, &mut f) {
        _ = std::fs::File::create("rewritten_bundle")
            .unwrap()
            .write_all(&data);

        match ValidBundle::parse(&data, &mut f) {
            Ok(ValidBundle::Valid(..)) => {}
            Ok(ValidBundle::Rewritten(..)) => panic!("Rewrite produced non-canonical results"),
            Ok(ValidBundle::Invalid(_, _, e)) => panic!("Rewrite produced invalid results: {e}"),
            Err(_) => panic!("Rewrite errored"),
        };
    }
}
*/
