//! PICS Default Context Tests
//!
//! Tests RFC 9173 default security contexts against Hardy.

use hardy_bpv7::block;
use hardy_bpv7::bpsec::encryptor;
use hardy_bpv7::bpsec::key;
use hardy_bpv7::bpsec::rfc9173::ScopeFlags;
use hardy_bpv7::bpsec::signer;
use hardy_bpv7::bundle;
use hardy_bpv7::editor::{Chunk, Editor};
use hardy_bpv7_tools::compare::{CompareOptions, compare_bundles_with_options};

fn integrity_key() -> key::Key {
    serde_json::from_value(serde_json::json!({
        "kid": "hmackey",
        "kty": "oct",
        "alg": "HS384",
        "key_ops": ["sign", "verify"],
        "k": "GisaKxorGisaKxorGisaKw"
    }))
    .unwrap()
}

fn confidentiality_key() -> key::Key {
    serde_json::from_value(serde_json::json!({
        "kid": "aesgcmkey_32",
        "kty": "oct",
        "alg": "dir",
        "enc": "A256GCM",
        "key_ops": ["encrypt", "decrypt"],
        "k": "cXdlcnR5dWlvcGFzZGZnaHF3ZXJ0eXVpb3Bhc2RmZ2g"
    }))
    .unwrap()
}

fn assert_bundles_equivalent(actual: &[u8], expected: &[u8], keys: &key::KeySet) {
    let parsed_a =
        bundle::ParsedBundle::parse_with_keys(actual, keys).expect("Failed to parse actual bundle");
    let parsed_b = bundle::ParsedBundle::parse_with_keys(expected, keys)
        .expect("Failed to parse expected bundle");
    let options = CompareOptions { ignore_crc: true };
    let diffs = compare_bundles_with_options(
        &parsed_a.bundle,
        actual,
        &parsed_b.bundle,
        expected,
        keys,
        &options,
    );
    assert!(diffs.is_empty(), "Bundles differ:\n{}", diffs.join("\n"));
}

// Requirement 1: The same security service MUST NOT be applied to a security
// target more than once in a bundle. (RFC 9172, Section 3.2)

#[test]
fn pics_1_1_duplicate_bcb_on_payload_must_fail() {
    let incoming = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F4240850C030100581B8101020182028203018182014C5477656C76653132313231328180850702000041008501010000583390EAB6457593379298A8724E16E61F837488E127212B59AC91F8A86287B7D07630A122C42BBA8CA26EECBCAB0F8124C2A42BDFFF"
    );

    let enc_key = confidentiality_key();
    let parsed = bundle::ParsedBundle::parse_with_keys(&incoming, &key::KeySet::EMPTY)
        .expect("Failed to parse incoming bundle");

    // MUST FAIL: bundle already includes a BCB with target payload
    let Err((_, err)) = encryptor::Encryptor::new(&parsed.bundle, &incoming).encrypt_block(
        1,
        encryptor::Context::AES_GCM(ScopeFlags::default()),
        "ipn:2.1".parse().unwrap(),
        &enc_key,
    ) else {
        panic!("Expected AlreadyEncrypted error");
    };
    assert!(matches!(err, encryptor::Error::AlreadyEncrypted(1)));
}

#[test]
fn pics_1_2_duplicate_bib_on_payload_must_fail() {
    let incoming = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F4240850B030000583F810101008202820301818182015830F75FE4C37F76F046165855BD5FF72FBFD4E3A64B4695C40E2B787DA005AE819F0A2E30A2E8B325527DE8AEFB52E73D718507020000410085010100005823526561647920746F2067656E657261746520612033322D62797465207061796C6F6164FF"
    );

    let sign_key = integrity_key();
    let keys = key::KeySet::new(vec![sign_key.clone()]);
    let parsed = bundle::ParsedBundle::parse_with_keys(&incoming, &keys)
        .expect("Failed to parse incoming bundle");

    // MUST FAIL: bundle already includes a BIB with target payload
    let Err((_, err)) = signer::Signer::new(&parsed.bundle, &incoming).sign_block(
        1,
        signer::Context::HMAC_SHA2(ScopeFlags::default()),
        "ipn:2.1".parse().unwrap(),
        &sign_key,
    ) else {
        panic!("Expected AlreadySigned error");
    };
    assert!(matches!(err, signer::Error::AlreadySigned(1)));
}

// Requirement 2: A single security block MAY represent multiple security
// operations. (RFC 9172, Section 3.3)

#[test]
fn pics_2_1_source_sign_payload_and_bundle_age() {
    let incoming = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F42408507020000410085010100005823526561647920746F2067656E657261746520612033322D62797465207061796C6F6164FF"
    );
    let outgoing = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F4240850B030000587582010201008202820301828182015830F75FE4C37F76F046165855BD5FF72FBFD4E3A64B4695C40E2B787DA005AE819F0A2E30A2E8B325527DE8AEFB52E73D7181820158306EE5CA30AB3A1BF1E7F645EB21418FFC129BACFB69677FDAE0D08CB63159358FA86BE682538299B4B7E53C04FE03FDE88507020000410085010100005823526561647920746F2067656E657261746520612033322D62797465207061796C6F6164FF"
    );

    let sign_key = integrity_key();
    let src: hardy_bpv7::eid::Eid = "ipn:3.1".parse().unwrap();
    let parsed = bundle::ParsedBundle::parse_with_keys(&incoming, &key::KeySet::EMPTY)
        .expect("Failed to parse");

    let signed = signer::Signer::new(&parsed.bundle, &incoming)
        .sign_block(
            1,
            signer::Context::HMAC_SHA2(ScopeFlags::default()),
            src.clone(),
            &sign_key,
        )
        .map_err(|(_, e)| e)
        .unwrap()
        .sign_block(
            2,
            signer::Context::HMAC_SHA2(ScopeFlags::default()),
            src,
            &sign_key,
        )
        .map_err(|(_, e)| e)
        .unwrap()
        .rebuild()
        .unwrap();

    let keys = key::KeySet::new(vec![integrity_key()]);
    assert_bundles_equivalent(&signed, &outgoing, &keys);
}

#[test]
fn pics_2_2_acceptor_verify_and_remove_bib() {
    let incoming = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F4240850B030000587582010201008202820301828182015830F75FE4C37F76F046165855BD5FF72FBFD4E3A64B4695C40E2B787DA005AE819F0A2E30A2E8B325527DE8AEFB52E73D7181820158306EE5CA30AB3A1BF1E7F645EB21418FFC129BACFB69677FDAE0D08CB63159358FA86BE682538299B4B7E53C04FE03FDE88507020000410085010100005823526561647920746F2067656E657261746520612033322D62797465207061796C6F6164FF"
    );
    let outgoing = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F42408507020000410085010100005823526561647920746F2067656E657261746520612033322D62797465207061796C6F6164FF"
    );

    let keys = key::KeySet::new(vec![integrity_key()]);
    let parsed = bundle::ParsedBundle::parse_with_keys(&incoming, &keys).unwrap();

    let result = Editor::new(&parsed.bundle, &incoming)
        .remove_integrity(1)
        .map_err(|(_, e)| e)
        .unwrap()
        .remove_integrity(2)
        .map_err(|(_, e)| e)
        .unwrap()
        .rebuild()
        .map(|c| Chunk::flatten(c, &incoming))
        .unwrap();

    assert_bundles_equivalent(&result, &outgoing, &key::KeySet::EMPTY);
}

#[test]
#[ignore] // GAP-1: Hardy creates separate BCBs per target (IV uniqueness), test expects multi-target BCB
fn pics_2_3_source_encrypt_payload_and_bundle_age() {
    let incoming = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F42408507020000410085010100005823526561647920746F2067656E657261746520612033322D62797465207061796C6F6164FF"
    );
    let outgoing = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F4240850C030100581D820102020182028203018182014C5477656C7665313231323132828080850702000051C225655BB0AF8CC854641DA15AB6BE9FA28501010000583390EAB6457593379298A8724E16E61F837488E127212B59AC91F8A86287B7D07630A122C42BBA8CA26EECBCAB0F8124C2A42BDFFF"
    );

    let enc_key = confidentiality_key();
    let src: hardy_bpv7::eid::Eid = "ipn:3.1".parse().unwrap();
    let parsed = bundle::ParsedBundle::parse_with_keys(&incoming, &key::KeySet::EMPTY)
        .expect("Failed to parse");

    let encrypted = encryptor::Encryptor::new(&parsed.bundle, &incoming)
        .encrypt_block(
            1,
            encryptor::Context::AES_GCM(ScopeFlags::default()),
            src.clone(),
            &enc_key,
        )
        .map_err(|(_, e)| e)
        .unwrap()
        .encrypt_block(
            2,
            encryptor::Context::AES_GCM(ScopeFlags::default()),
            src,
            &enc_key,
        )
        .map_err(|(_, e)| e)
        .unwrap()
        .rebuild()
        .unwrap();

    let keys = key::KeySet::new(vec![confidentiality_key()]);
    assert_bundles_equivalent(&encrypted, &outgoing, &keys);
}

#[test]
fn pics_2_4_acceptor_decrypt_and_remove_bcb() {
    let incoming = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F4240850C030100581D820102020182028203018182014C5477656C7665313231323132828080850702000051C225655BB0AF8CC854641DA15AB6BE9FA28501010000583390EAB6457593379298A8724E16E61F837488E127212B59AC91F8A86287B7D07630A122C42BBA8CA26EECBCAB0F8124C2A42BDFFF"
    );
    let outgoing = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F42408507020000410085010100005823526561647920746F2067656E657261746520612033322D62797465207061796C6F6164FF"
    );

    let keys = key::KeySet::new(vec![confidentiality_key()]);
    let parsed = bundle::ParsedBundle::parse_with_keys(&incoming, &keys).unwrap();

    let result = Editor::new(&parsed.bundle, &incoming)
        .remove_encryption(1, &keys)
        .map_err(|(_, e)| e)
        .unwrap()
        .remove_encryption(2, &keys)
        .map_err(|(_, e)| e)
        .unwrap()
        .rebuild()
        .map(|c| Chunk::flatten(c, &incoming))
        .unwrap();

    assert_bundles_equivalent(&result, &outgoing, &key::KeySet::EMPTY);
}

#[test]
#[ignore] // GAP-1: Hardy creates separate BCBs per target (IV uniqueness), test expects multi-target BCB
fn pics_2_5_source_sign_then_encrypt_both() {
    let incoming = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F42408507020000410085010100005823526561647920746F2067656E657261746520612033322D62797465207061796C6F6164FF"
    );
    let outgoing = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F4240850C040100581F83010302020182028203018182014C5477656C766531323132313283808080850B0300005885408ED5200C31417FBBCE95A1F19526C7E6F764C46D6F8488FED498FFA82186A58B23E09DBC956CAAACD3118DBB3301F97CFBFA6E8DB8A85B85FF9CAC1967EF9C6CE2DBBD9C8EF38CB32A3CC5EF31E71E6839666CEA17424457A1A01F70F08377099F27B4B27EFB839B18C434DF3C6FF425AC662E4817F774EE513D36AF41D8F7ED3055E53B850702000051C2B19A334CC8C895C69A5B3DCE7BDE52FA8501010000583390EAB6457593379298A8724E16E61F837488E127212B59AC91F8A86287B7D07630A122A4A2C8343500978F613F564529596403FF"
    );

    let sign_key = integrity_key();
    let enc_key = confidentiality_key();
    let src: hardy_bpv7::eid::Eid = "ipn:3.1".parse().unwrap();
    let flags = ScopeFlags {
        include_security_header: false,
        ..ScopeFlags::default()
    };

    let parsed = bundle::ParsedBundle::parse_with_keys(&incoming, &key::KeySet::EMPTY).unwrap();

    let signed = signer::Signer::new(&parsed.bundle, &incoming)
        .sign_block(
            1,
            signer::Context::HMAC_SHA2(ScopeFlags::default()),
            src.clone(),
            &sign_key,
        )
        .map_err(|(_, e)| e)
        .unwrap()
        .sign_block(
            2,
            signer::Context::HMAC_SHA2(ScopeFlags::default()),
            src.clone(),
            &sign_key,
        )
        .map_err(|(_, e)| e)
        .unwrap()
        .rebuild()
        .unwrap();

    let keys = key::KeySet::new(vec![sign_key.clone()]);
    let parsed_signed = bundle::ParsedBundle::parse_with_keys(&signed, &keys).unwrap();

    let encrypted = encryptor::Encryptor::new(&parsed_signed.bundle, &signed)
        .encrypt_block(
            1,
            encryptor::Context::AES_GCM(flags.clone()),
            src.clone(),
            &enc_key,
        )
        .map_err(|(_, e)| e)
        .unwrap()
        .encrypt_block(2, encryptor::Context::AES_GCM(flags), src, &enc_key)
        .map_err(|(_, e)| e)
        .unwrap()
        .rebuild()
        .unwrap();

    let all_keys = key::KeySet::new(vec![integrity_key(), confidentiality_key()]);
    assert_bundles_equivalent(&encrypted, &outgoing, &all_keys);
}

#[test]
fn pics_2_6_acceptor_decrypt_then_verify_both() {
    let incoming = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F4240850C040100581F83010302020182028203018182014C5477656C766531323132313283808080850B0300005885408ED5200C31417FBBCE95A1F19526C7E6F764C46D6F8488FED498FFA82186A58B23E09DBC956CAAACD3118DBB3301F97CFBFA6E8DB8A85B85FF9CAC1967EF9C6CE2DBBD9C8EF38CB32A3CC5EF31E71E6839666CEA17424457A1A01F70F08377099F27B4B27EFB839B18C434DF3C6FF425AC662E4817F774EE513D36AF41D8F7ED3055E53B850702000051C2B19A334CC8C895C69A5B3DCE7BDE52FA8501010000583390EAB6457593379298A8724E16E61F837488E127212B59AC91F8A86287B7D07630A122A4A2C8343500978F613F564529596403FF"
    );
    let outgoing = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F42408507020000410085010100005823526561647920746F2067656E657261746520612033322D62797465207061796C6F6164FF"
    );

    let all_keys = key::KeySet::new(vec![integrity_key(), confidentiality_key()]);
    let parsed = bundle::ParsedBundle::parse_with_keys(&incoming, &all_keys).unwrap();

    let decrypted = Editor::new(&parsed.bundle, &incoming)
        .remove_encryption(1, &all_keys)
        .map_err(|(_, e)| e)
        .unwrap()
        .remove_encryption(2, &all_keys)
        .map_err(|(_, e)| e)
        .unwrap()
        .rebuild()
        .map(|c| Chunk::flatten(c, &incoming))
        .unwrap();

    let parsed2 = bundle::ParsedBundle::parse_with_keys(&decrypted, &all_keys).unwrap();

    let mut ed = Editor::new(&parsed2.bundle, &decrypted);
    for &bn in &[1u64, 2] {
        if parsed2.bundle.blocks[&bn].bib != block::BibCoverage::None {
            ed = ed.remove_integrity(bn).map_err(|(_, e)| e).unwrap();
        }
    }
    let result = ed.rebuild().map(|c| Chunk::flatten(c, &decrypted)).unwrap();

    assert_bundles_equivalent(&result, &outgoing, &key::KeySet::EMPTY);
}

#[test]
#[ignore] // GAP-1: Hardy creates separate BCBs per target (IV uniqueness), test expects multi-target BCB
fn pics_2_7_source_interleaved_sign_encrypt() {
    let incoming = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F42408507020000410085010100005823526561647920746F2067656E657261746520612033322D62797465207061796C6F6164FF"
    );
    let outgoing = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F4240850C04010058218401030206020182028203018182014C5477656C76653132313231328480808080850B030000584F438ED6218EB1C1FEB94E96A272CC4E004E4C437864E932D8B0D9701D00F916CEBC660D906FC4A68FFFD6CC28101C1F6C58E56824D62EDF7410B9C905ACBDA3CEF84DA12ED941991BEC88C11453BF03850B060000584F438DD6218EB1C1FEB94E96A272CC4EB247B649377C3BA5BC08176B8B5E95EDEC16660F5AFDB4EDB89DC0DB1C1E7982F5F9113FE630ADF50173A1EDE8A6235B5045FC70DABCE2232B345C5CD0BD8BF2850702000051C2B19A334CC8C895C69A5B3DCE7BDE52FA8501010000583390EAB6457593379298A8724E16E61F837488E127212B59AC91F8A86287B7D07630A122A4A2C8343500978F613F564529596403FF"
    );

    let sign_key = integrity_key();
    let enc_key = confidentiality_key();
    let src: hardy_bpv7::eid::Eid = "ipn:3.1".parse().unwrap();
    let flags = ScopeFlags {
        include_security_header: false,
        ..ScopeFlags::default()
    };
    let all_keys = key::KeySet::new(vec![integrity_key(), confidentiality_key()]);

    let parsed = bundle::ParsedBundle::parse_with_keys(&incoming, &key::KeySet::EMPTY).unwrap();

    let step1 = signer::Signer::new(&parsed.bundle, &incoming)
        .sign_block(
            1,
            signer::Context::HMAC_SHA2(ScopeFlags::default()),
            src.clone(),
            &sign_key,
        )
        .map_err(|(_, e)| e)
        .unwrap()
        .rebuild()
        .unwrap();

    let p1 = bundle::ParsedBundle::parse_with_keys(&step1, &all_keys).unwrap();

    let step2 = encryptor::Encryptor::new(&p1.bundle, &step1)
        .encrypt_block(
            1,
            encryptor::Context::AES_GCM(flags.clone()),
            src.clone(),
            &enc_key,
        )
        .map_err(|(_, e)| e)
        .unwrap()
        .rebuild()
        .unwrap();

    let p2 = bundle::ParsedBundle::parse_with_keys(&step2, &all_keys).unwrap();

    let step3 = signer::Signer::new(&p2.bundle, &step2)
        .sign_block(
            2,
            signer::Context::HMAC_SHA2(ScopeFlags::default()),
            src.clone(),
            &sign_key,
        )
        .map_err(|(_, e)| e)
        .unwrap()
        .rebuild()
        .unwrap();

    let p3 = bundle::ParsedBundle::parse_with_keys(&step3, &all_keys).unwrap();

    let step4 = encryptor::Encryptor::new(&p3.bundle, &step3)
        .encrypt_block(2, encryptor::Context::AES_GCM(flags), src, &enc_key)
        .map_err(|(_, e)| e)
        .unwrap()
        .rebuild()
        .unwrap();

    assert_bundles_equivalent(&step4, &outgoing, &all_keys);
}

#[test]
fn pics_2_8_acceptor_decrypt_and_verify_interleaved() {
    let incoming = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F4240850C04010058218401030206020182028203018182014C5477656C76653132313231328480808080850B030000584F438ED6218EB1C1FEB94E96A272CC4E004E4C437864E932D8B0D9701D00F916CEBC660D906FC4A68FFFD6CC28101C1F6C58E56824D62EDF7410B9C905ACBDA3CEF84DA12ED941991BEC88C11453BF03850B060000584F438DD6218EB1C1FEB94E96A272CC4EB247B649377C3BA5BC08176B8B5E95EDEC16660F5AFDB4EDB89DC0DB1C1E7982F5F9113FE630ADF50173A1EDE8A6235B5045FC70DABCE2232B345C5CD0BD8BF2850702000051C2B19A334CC8C895C69A5B3DCE7BDE52FA8501010000583390EAB6457593379298A8724E16E61F837488E127212B59AC91F8A86287B7D07630A122A4A2C8343500978F613F564529596403FF"
    );
    let outgoing = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F42408507020000410085010100005823526561647920746F2067656E657261746520612033322D62797465207061796C6F6164FF"
    );

    let all_keys = key::KeySet::new(vec![integrity_key(), confidentiality_key()]);
    let parsed = bundle::ParsedBundle::parse_with_keys(&incoming, &all_keys).unwrap();

    let decrypted = Editor::new(&parsed.bundle, &incoming)
        .remove_encryption(1, &all_keys)
        .map_err(|(_, e)| e)
        .unwrap()
        .remove_encryption(2, &all_keys)
        .map_err(|(_, e)| e)
        .unwrap()
        .rebuild()
        .map(|c| Chunk::flatten(c, &incoming))
        .unwrap();

    let parsed2 = bundle::ParsedBundle::parse_with_keys(&decrypted, &all_keys).unwrap();

    let mut ed = Editor::new(&parsed2.bundle, &decrypted);
    for &bn in &[1u64, 2] {
        if parsed2.bundle.blocks[&bn].bib != block::BibCoverage::None {
            ed = ed.remove_integrity(bn).map_err(|(_, e)| e).unwrap();
        }
    }
    let result = ed.rebuild().map(|c| Chunk::flatten(c, &decrypted)).unwrap();

    assert_bundles_equivalent(&result, &outgoing, &key::KeySet::EMPTY);
}

// Requirement 7: A security target in a BIB MUST NOT reference a security
// block (BIB or BCB). (RFC 9172, Section 3.7)

#[test]
fn pics_7_1_bib_cannot_target_bib() {
    let incoming = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F4240850B030000583F810101008202820301818182015830F75FE4C37F76F046165855BD5FF72FBFD4E3A64B4695C40E2B787DA005AE819F0A2E30A2E8B325527DE8AEFB52E73D718507020000410085010100005823526561647920746F2067656E657261746520612033322D62797465207061796C6F6164FF"
    );

    let sign_key = integrity_key();
    let keys = key::KeySet::new(vec![sign_key.clone()]);
    let parsed = bundle::ParsedBundle::parse_with_keys(&incoming, &keys)
        .expect("Failed to parse incoming bundle");

    let bib_bn = parsed
        .bundle
        .blocks
        .iter()
        .find(|(_, b)| b.block_type == block::Type::BlockIntegrity)
        .map(|(bn, _)| *bn)
        .expect("No BIB found");

    // MUST FAIL: a BIB cannot target a BIB
    let Err((_, err)) = signer::Signer::new(&parsed.bundle, &incoming).sign_block(
        bib_bn,
        signer::Context::HMAC_SHA2(ScopeFlags::default()),
        "ipn:3.1".parse().unwrap(),
        &sign_key,
    ) else {
        panic!("Expected InvalidTarget error");
    };
    assert!(matches!(err, signer::Error::InvalidTarget(bn) if bn == bib_bn));
}

#[test]
fn pics_7_2_bib_cannot_target_bcb() {
    let incoming = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F4240850C030100581B8101020182028203018182014C5477656C76653132313231328180850702000041008501010000583390EAB6457593379298A8724E16E61F837488E127212B59AC91F8A86287B7D07630A122C42BBA8CA26EECBCAB0F8124C2A42BDFFF"
    );

    let sign_key = integrity_key();
    let parsed = bundle::ParsedBundle::parse_with_keys(&incoming, &key::KeySet::EMPTY)
        .expect("Failed to parse incoming bundle");

    let bcb_bn = parsed
        .bundle
        .blocks
        .iter()
        .find(|(_, b)| b.block_type == block::Type::BlockSecurity)
        .map(|(bn, _)| *bn)
        .expect("No BCB found");

    // MUST FAIL: a BIB cannot target a BCB
    let Err((_, err)) = signer::Signer::new(&parsed.bundle, &incoming).sign_block(
        bcb_bn,
        signer::Context::HMAC_SHA2(ScopeFlags::default()),
        "ipn:3.1".parse().unwrap(),
        &sign_key,
    ) else {
        panic!("Expected InvalidTarget error");
    };
    assert!(matches!(err, signer::Error::InvalidTarget(bn) if bn == bcb_bn));
}

// Requirement 14: A BCB MUST NOT include another BCB as a security target.
// (RFC 9172, Section 3.8)

#[test]
fn pics_14_1_bcb_cannot_target_bcb() {
    // Incoming: original + BCB on payload (BCB is block 3)
    let incoming = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F4240850C030100581B8101020182028203018182014C5477656C76653132313231328180850702000041008501010000583390EAB6457593379298A8724E16E61F837488E127212B59AC91F8A86287B7D07630A122C42BBA8CA26EECBCAB0F8124C2A42BDFFF"
    );

    let enc_key = confidentiality_key();
    let parsed = bundle::ParsedBundle::parse_with_keys(&incoming, &key::KeySet::EMPTY)
        .expect("Failed to parse incoming bundle");

    let bcb_bn = parsed
        .bundle
        .blocks
        .iter()
        .find(|(_, b)| b.block_type == block::Type::BlockSecurity)
        .map(|(bn, _)| *bn)
        .expect("No BCB found");

    // MUST FAIL: a BCB cannot target a BCB
    let Err((_, err)) = encryptor::Encryptor::new(&parsed.bundle, &incoming).encrypt_block(
        bcb_bn,
        encryptor::Context::AES_GCM(ScopeFlags::default()),
        "ipn:3.1".parse().unwrap(),
        &enc_key,
    ) else {
        panic!("Expected InvalidTarget error");
    };
    assert!(matches!(err, encryptor::Error::InvalidTarget(bn) if bn == bcb_bn));
}

// Requirement 15: A BCB MUST NOT target the primary block.
// (RFC 9172, Section 3.8)

#[test]
fn pics_15_1_bcb_cannot_target_primary() {
    let incoming = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F42408507020000410085010100005823526561647920746F2067656E657261746520612033322D62797465207061796C6F6164FF"
    );

    let enc_key = confidentiality_key();
    let parsed = bundle::ParsedBundle::parse_with_keys(&incoming, &key::KeySet::EMPTY)
        .expect("Failed to parse incoming bundle");

    // MUST FAIL: a BCB cannot target the primary block (block 0)
    let Err((_, err)) = encryptor::Encryptor::new(&parsed.bundle, &incoming).encrypt_block(
        0,
        encryptor::Context::AES_GCM(ScopeFlags::default()),
        "ipn:3.1".parse().unwrap(),
        &enc_key,
    ) else {
        panic!("Expected InvalidTarget error");
    };
    assert!(matches!(err, encryptor::Error::InvalidTarget(0)));
}

// Requirement 16: A BCB MUST NOT target a BIB unless it shares a security
// target with that BIB. (RFC 9172, Section 3.8)

#[test]
fn pics_16_1_bcb_cannot_target_bib_directly() {
    // Incoming: original + BIB on payload (BIB is block 3)
    let incoming = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F4240850B030000583F810101008202820301818182015830F75FE4C37F76F046165855BD5FF72FBFD4E3A64B4695C40E2B787DA005AE819F0A2E30A2E8B325527DE8AEFB52E73D718507020000410085010100005823526561647920746F2067656E657261746520612033322D62797465207061796C6F6164FF"
    );

    let enc_key = confidentiality_key();
    let sign_key = integrity_key();
    let keys = key::KeySet::new(vec![sign_key]);
    let parsed = bundle::ParsedBundle::parse_with_keys(&incoming, &keys)
        .expect("Failed to parse incoming bundle");

    let bib_bn = parsed
        .bundle
        .blocks
        .iter()
        .find(|(_, b)| b.block_type == block::Type::BlockIntegrity)
        .map(|(bn, _)| *bn)
        .expect("No BIB found");

    // MUST FAIL: a BCB cannot directly target a BIB
    let Err((_, err)) = encryptor::Encryptor::new(&parsed.bundle, &incoming).encrypt_block(
        bib_bn,
        encryptor::Context::AES_GCM(ScopeFlags::default()),
        "ipn:3.1".parse().unwrap(),
        &enc_key,
    ) else {
        panic!("Expected InvalidTarget error");
    };
    assert!(matches!(err, encryptor::Error::InvalidTarget(bn) if bn == bib_bn));
}

// Requirement 21: When a BCB's targets match some (but not all) targets of a BIB,
// the BIB MUST be split: matching targets go into a new encrypted BIB,
// non-matching targets stay in the original BIB.
// (RFC 9172, Section 3.9)

#[test]
#[ignore] // GAP-1: Hardy creates separate BCBs per target (IV uniqueness), test expects multi-target BCB
fn pics_21_1_bib_split_on_partial_encrypt() {
    // Incoming: BIB with two targets (payload + bundle-age) from test 2.1
    let incoming = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F4240850B030000587582010201008202820301828182015830F75FE4C37F76F046165855BD5FF72FBFD4E3A64B4695C40E2B787DA005AE819F0A2E30A2E8B325527DE8AEFB52E73D7181820158306EE5CA30AB3A1BF1E7F645EB21418FFC129BACFB69677FDAE0D08CB63159358FA86BE682538299B4B7E53C04FE03FDE88507020000410085010100005823526561647920746F2067656E657261746520612033322D62797465207061796C6F6164FF"
    );
    // SUCCESS: two BIBs (one unencrypted targeting bundle-age, one encrypted), one BCB
    let outgoing = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F4240850C040100581D820105020182028203018182014C5477656C7665313231323132828080850B030000583F8102010082028203018181820158306EE5CA30AB3A1BF1E7F645EB21418FFC129BACFB69677FDAE0D08CB63159358FA86BE682538299B4B7E53C04FE03FDE8850B050000584F438ED6218EB1C1FEB94E96A272CC4E004E4C437864E932D8B0D9701D00F916CEBC660D906FC4A68FFFD6CC28101C1F6C58E56824D62EDF7410B9C905ACBDA3ABDACA391691C220AB4E156E793083B8850702000041008501010000583390EAB6457593379298A8724E16E61F837488E127212B59AC91F8A86287B7D07630A122A4A2C8343500978F613F564529596403FF"
    );

    let enc_key = confidentiality_key();
    let sign_key = integrity_key();
    let keys = key::KeySet::new(vec![sign_key]);
    let all_keys = key::KeySet::new(vec![integrity_key(), confidentiality_key()]);
    let src: hardy_bpv7::eid::Eid = "ipn:3.1".parse().unwrap();
    let parsed = bundle::ParsedBundle::parse_with_keys(&incoming, &keys).expect("Failed to parse");

    let flags = ScopeFlags {
        include_security_header: false,
        ..ScopeFlags::default()
    };
    let encrypted = encryptor::Encryptor::new(&parsed.bundle, &incoming)
        .encrypt_block(1, encryptor::Context::AES_GCM(flags), src, &enc_key)
        .map_err(|(_, e)| e)
        .unwrap()
        .rebuild()
        .unwrap();

    assert_bundles_equivalent(&encrypted, &outgoing, &all_keys);
}

// Requirement 22: A BIB MUST NOT be added for a security target that is already
// the security target of a BCB. (RFC 9172, Section 3.9)

#[test]
fn pics_22_1_cannot_sign_encrypted_block() {
    // Incoming: original + BCB on payload
    let incoming = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F4240850C030100581B8101020182028203018182014C5477656C76653132313231328180850702000041008501010000583390EAB6457593379298A8724E16E61F837488E127212B59AC91F8A86287B7D07630A122C42BBA8CA26EECBCAB0F8124C2A42BDFFF"
    );

    let sign_key = integrity_key();
    let parsed = bundle::ParsedBundle::parse_with_keys(&incoming, &key::KeySet::EMPTY)
        .expect("Failed to parse incoming bundle");

    // MUST FAIL: cannot sign a block that is already encrypted
    let Err((_, err)) = signer::Signer::new(&parsed.bundle, &incoming).sign_block(
        1,
        signer::Context::HMAC_SHA2(ScopeFlags::default()),
        "ipn:3.1".parse().unwrap(),
        &sign_key,
    ) else {
        panic!("Expected EncryptedTarget error");
    };
    assert!(matches!(err, signer::Error::EncryptedTarget(1)));
}

// Requirement 25 (test 26.1): Integrity protection covers block metadata.
// If block flags are altered after signing, verification MUST FAIL.
// (RFC 9172, Section 4)

#[test]
fn pics_26_1_tampered_block_flags_must_fail() {
    // Bundle from test 2.1 with payload block flags altered (0x00 -> 0x03)
    let incoming = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F4240850B030000587582010201008202820301828182015830F75FE4C37F76F046165855BD5FF72FBFD4E3A64B4695C40E2B787DA005AE819F0A2E30A2E8B325527DE8AEFB52E73D7181820158306EE5CA30AB3A1BF1E7F645EB21418FFC129BACFB69677FDAE0D08CB63159358FA86BE682538299B4B7E53C04FE03FDE88507020000410085010103005823526561647920746F2067656E657261746520612033322D62797465207061796C6F6164FF"
    );

    let keys = key::KeySet::new(vec![integrity_key()]);

    // MUST FAIL: BIB verification fails because payload block flags were tampered
    let result = bundle::ParsedBundle::parse_with_keys(&incoming, &keys);
    assert!(
        matches!(
            result,
            Err(hardy_bpv7::Error::InvalidBPSec(
                hardy_bpv7::bpsec::Error::IntegrityCheckFailed
            ))
        ),
        "Expected IntegrityCheckFailed, got: {:?}",
        result
    );
}

// Requirement 36: If an encrypted payload block cannot be decrypted, the bundle
// MUST be discarded and processed no further. (RFC 9172, Section 5.1.1)

#[test]
fn pics_36_1_payload_decrypt_wrong_key_must_discard() {
    // Original + BCB on payload
    let incoming = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F4240850C030100581B8101020182028203018182014C5477656C76653132313231328180850702000041008501010000583390EAB6457593379298A8724E16E61F837488E127212B59AC91F8A86287B7D07630A122C42BBA8CA26EECBCAB0F8124C2A42BDFFF"
    );

    let wrong_key: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "wrong_aesgcmkey",
        "kty": "oct",
        "alg": "dir",
        "enc": "A256GCM",
        "key_ops": ["encrypt", "decrypt"],
        "k": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
    }))
    .unwrap();
    let wrong_keys = key::KeySet::new(vec![wrong_key]);

    // Parse succeeds (payload stays encrypted at parse level)
    let parsed = bundle::ParsedBundle::parse_with_keys(&incoming, &wrong_keys)
        .expect("Parse should succeed with encrypted payload");

    // MUST FAIL: decryption with wrong key
    let Err((_, err)) = Editor::new(&parsed.bundle, &incoming).remove_encryption(1, &wrong_keys)
    else {
        panic!("Expected decryption failure with wrong key");
    };
    assert!(
        matches!(
            err,
            hardy_bpv7::editor::Error::Builder(hardy_bpv7::builder::Error::InternalError(
                hardy_bpv7::Error::InvalidBPSec(hardy_bpv7::bpsec::Error::DecryptionFailed)
            ))
        ),
        "Expected DecryptionFailed, got: {err:?}"
    );
}

// Requirement 37: If an encrypted security target other than the payload block
// cannot be decrypted, then the associated security target and all security blocks
// associated with that target MUST be discarded. (RFC 9172, Section 5.1.1)

#[test]
fn pics_37_1_non_payload_decrypt_wrong_key_removes_target() {
    // Original + BCB on bundle-age only
    let incoming = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F4240850C030100581B8102020182028203018182014C5477656C76653132313231328180850702000051C225655BB0AF8CC854641DA15AB6BE9FA285010100005823526561647920746F2067656E657261746520612033322D62797465207061796C6F6164FF"
    );

    let wrong_key: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "wrong_aesgcmkey",
        "kty": "oct",
        "alg": "dir",
        "enc": "A256GCM",
        "key_ops": ["encrypt", "decrypt"],
        "k": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
    }))
    .unwrap();
    let wrong_keys = key::KeySet::new(vec![wrong_key]);

    // RFC 9172 Section 5.1.1: non-payload decrypt failure should remove
    // the target + BCB, bundle survives.
    let rewritten = bundle::RewrittenBundle::parse_with_keys(&incoming, &wrong_keys)
        .expect("Should not fail to parse");

    let bundle = match &rewritten {
        bundle::RewrittenBundle::Valid { bundle, .. }
        | bundle::RewrittenBundle::Rewritten { bundle, .. } => bundle,
        bundle::RewrittenBundle::Invalid { error, .. } => {
            panic!("Bundle should survive non-payload decrypt failure, got: {error}")
        }
    };

    assert!(
        !bundle.blocks.contains_key(&2),
        "Bundle-age block should have been removed"
    );
    for (_, blk) in &bundle.blocks {
        assert_ne!(
            blk.block_type,
            block::Type::BlockSecurity,
            "BCB should have been removed"
        );
    }
}

// Requirement 56: A BCB or BIB MUST NOT be added to a bundle if the 'Bundle is a
// fragment' flag is set. (RFC 9172, Section 5.2)

#[test]
fn pics_56_1_cannot_add_bcb_to_fragment() {
    let incoming = hex_literal::hex!(
        "9F8A070100820282010282028202018202820201820018281A000F424000008507020000410086010100015823526561647920746F2067656E657261746520612033322D62797465207061796C6F6164425114FF"
    );

    let enc_key = confidentiality_key();
    let parsed = bundle::ParsedBundle::parse_with_keys(&incoming, &key::KeySet::EMPTY)
        .expect("Failed to parse fragment bundle");

    assert!(
        parsed.bundle.flags.is_fragment,
        "Bundle should be a fragment"
    );

    let Err((_, err)) = encryptor::Encryptor::new(&parsed.bundle, &incoming).encrypt_block(
        1,
        encryptor::Context::AES_GCM(ScopeFlags::default()),
        "ipn:3.1".parse().unwrap(),
        &enc_key,
    ) else {
        panic!("Expected FragmentedBundle error");
    };
    assert!(matches!(err, encryptor::Error::FragmentedBundle));
}

#[test]
fn pics_56_1b_cannot_add_bib_to_fragment() {
    let incoming = hex_literal::hex!(
        "9F8A070100820282010282028202018202820201820018281A000F424000008507020000410086010100015823526561647920746F2067656E657261746520612033322D62797465207061796C6F6164425114FF"
    );

    let sign_key = integrity_key();
    let parsed = bundle::ParsedBundle::parse_with_keys(&incoming, &key::KeySet::EMPTY)
        .expect("Failed to parse fragment bundle");

    let Err((_, err)) = signer::Signer::new(&parsed.bundle, &incoming).sign_block(
        1,
        signer::Context::HMAC_SHA2(ScopeFlags::default()),
        "ipn:3.1".parse().unwrap(),
        &sign_key,
    ) else {
        panic!("Expected FragmentedBundle error");
    };
    assert!(matches!(err, signer::Error::FragmentedBundle));
}
