/*
#[cfg(test)]
use {hardy_bpv7::prelude::*, std::io::Write};

#[test]
fn test() {
    let data = include_bytes!("artifacts/bundle/crash-2872423a33315b80d8e5102ed6d583d4ba7f6eef");
    //include_bytes!("rewritten_bundle");

    let mut f = |_: &Eid| Ok(None);

    if let Ok(ValidBundle::Rewritten(_, data)) = ValidBundle::parse(data, &mut f) {
        _ = std::fs::File::create("rewritten_bundle")
            .unwrap()
            .write_all(&data);

        match ValidBundle::parse(&data, &mut f) {
            Ok(ValidBundle::Valid(_)) => {}
            Ok(ValidBundle::Rewritten(_, _)) => panic!("Rewrite produced non-canonical results"),
            Ok(ValidBundle::Invalid(_, e)) => panic!("Rewrite produced invalid results: {e}"),
            Err(_) => panic!("Rewrite errored"),
        };
    }
}
*/
