/*#[cfg(test)]
use {hardy_bpv7::prelude::*, std::io::Write};

#[test]
fn test() {
    let data = include_bytes!("artifacts/bundle/crash-2872423a33315b80d8e5102ed6d583d4ba7f6eef");
    //include_bytes!("rewritten_bundle");

    let mut f = |_: &Eid| Ok(None);

    let r = ValidBundle::parse(data, &mut f);
    dbg!(&r);

    if let Ok(ValidBundle::Rewritten(_, data)) = r {
        _ = std::fs::File::create("rewritten_bundle")
            .unwrap()
            .write_all(&data);

        let r = ValidBundle::parse(&data, &mut f);
        dbg!(&r);

        match r {
            Ok(ValidBundle::Valid(_)) => {}
            Ok(ValidBundle::Rewritten(_, _)) => panic!("Rewrite produced non-canonical results"),
            Ok(ValidBundle::Invalid(_)) => panic!("Rewrite produced invalid results"),
            Err(_) => panic!("Rewrite errored"),
        };
    }
}
*/
