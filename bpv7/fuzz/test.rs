#[cfg(test)]
use {hardy_bpv7::prelude::*, std::io::Write};

#[test]
fn test() {
    let data = include_bytes!("artifacts/bundle/crash-94c2b30914ab8551c2ac0067caa9a7e421c33c17");
    //include_bytes!("rewritten_bundle");

    let mut f = |_: &Eid| Ok(None);

    let r = ValidBundle::parse(data, &mut f);
    dbg!(&r);

    if let Ok(ValidBundle::Canonicalised(_, data)) = r {
        _ = std::fs::File::create("rewritten_bundle")
            .unwrap()
            .write_all(&data);

        let r = ValidBundle::parse(&data, &mut f);
        dbg!(&r);

        let Ok(ValidBundle::Valid(_)) = r else {
            panic!("Rewrite borked");
        };
    }
}
