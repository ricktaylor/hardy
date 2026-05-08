use hardy_bpv7::bpsec::key::KeySet;
use hardy_bpv7::bundle;
use hardy_bpv7_tools::compare::compare_bundles;

fn parse(hex: &[u8]) -> (bundle::Bundle, Vec<u8>) {
    let data = hex.to_vec();
    let parsed =
        bundle::ParsedBundle::parse_with_keys(&data, &KeySet::EMPTY).expect("Failed to parse");
    (parsed.bundle, data)
}

fn parse_with_keys(hex: &[u8], keys: &KeySet) -> (bundle::Bundle, Vec<u8>) {
    let data = hex.to_vec();
    let parsed = bundle::ParsedBundle::parse_with_keys(&data, keys).expect("Failed to parse");
    (parsed.bundle, data)
}

// Original plain bundle (RFC 9173, Section A.3.1.4)
const ORIGINAL: &[u8] = &hex_literal::hex!(
    "9F88070000820282010282028202018202820201820018281A000F42408507020000410085010100005823526561647920746F2067656E657261746520612033322D62797465207061796C6F6164FF"
);

#[test]
fn identical_bundles() {
    let (a, da) = parse(ORIGINAL);
    let (b, db) = parse(ORIGINAL);
    let diffs = compare_bundles(&a, &da, &b, &db, &KeySet::EMPTY);
    assert!(
        diffs.is_empty(),
        "Identical bundles should match: {diffs:?}"
    );
}

#[test]
fn different_payload() {
    let (a, da) = parse(ORIGINAL);
    let other = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F424085070200004100850101000058204120646966666572656E742033322D62797465207061796C6F61642121212121FF"
    );
    let (b, db) = parse(&other);
    let diffs = compare_bundles(&a, &da, &b, &db, &KeySet::EMPTY);
    assert!(!diffs.is_empty(), "Different payloads should differ");
    assert!(
        diffs.iter().any(|d| d.contains("Payload")),
        "Should report payload diff: {diffs:?}"
    );
}

#[test]
fn signed_bundle_matches_itself() {
    let signed = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F4240850B030000583F810101008202820301818182015830F75FE4C37F76F046165855BD5FF72FBFD4E3A64B4695C40E2B787DA005AE819F0A2E30A2E8B325527DE8AEFB52E73D718507020000410085010100005823526561647920746F2067656E657261746520612033322D62797465207061796C6F6164FF"
    );
    let keys: KeySet = serde_json::from_value(serde_json::json!({
        "keys": [{"kid": "hmackey", "kty": "oct", "alg": "HS384",
                  "key_ops": ["sign", "verify"], "k": "GisaKxorGisaKxorGisaKw"}]
    }))
    .unwrap();
    let (a, da) = parse_with_keys(&signed, &keys);
    let (b, db) = parse_with_keys(&signed, &keys);
    let diffs = compare_bundles(&a, &da, &b, &db, &keys);
    assert!(
        diffs.is_empty(),
        "Same signed bundle should match: {diffs:?}"
    );
}

#[test]
fn encrypted_bundle_matches_itself() {
    let encrypted = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F4240850C030100581D820102020182028203018182014C5477656C7665313231323132828080850702000051C225655BB0AF8CC854641DA15AB6BE9FA28501010000583390EAB6457593379298A8724E16E61F837488E127212B59AC91F8A86287B7D07630A122C42BBA8CA26EECBCAB0F8124C2A42BDFFF"
    );
    let keys: KeySet = serde_json::from_value(serde_json::json!({
        "keys": [{"kid": "aesgcmkey_32", "kty": "oct", "alg": "dir", "enc": "A256GCM",
                  "key_ops": ["encrypt", "decrypt"],
                  "k": "cXdlcnR5dWlvcGFzZGZnaHF3ZXJ0eXVpb3Bhc2RmZ2g"}]
    }))
    .unwrap();
    let (a, da) = parse_with_keys(&encrypted, &keys);
    let (b, db) = parse_with_keys(&encrypted, &keys);
    let diffs = compare_bundles(&a, &da, &b, &db, &keys);
    assert!(
        diffs.is_empty(),
        "Same encrypted bundle should match: {diffs:?}"
    );
}

#[test]
fn missing_block_detected() {
    let (a, da) = parse(ORIGINAL);
    let no_age = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F424085010100005823526561647920746F2067656E657261746520612033322D62797465207061796C6F6164FF"
    );
    let (b, db) = parse(&no_age);
    let diffs = compare_bundles(&a, &da, &b, &db, &KeySet::EMPTY);
    assert!(!diffs.is_empty(), "Missing block should be detected");
    assert!(
        diffs.iter().any(|d| d.contains("BundleAge")),
        "Should report BundleAge: {diffs:?}"
    );
}

#[test]
fn different_extension_block_order_is_equivalent() {
    // RFC 9171: "Block numbering is unrelated to the order in which blocks
    // are sequenced in the bundle."

    // Order A: [primary, BIB, bundle-age, payload]
    let order_a = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F4240"
        "850B030000587582010201008202820301828182015830F75FE4C37F76F046165855BD5FF72FBFD4E3A64B4695C40E2B787DA005AE819F0A2E30A2E8B325527DE8AEFB52E73D7181820158306EE5CA30AB3A1BF1E7F645EB21418FFC129BACFB69677FDAE0D08CB63159358FA86BE682538299B4B7E53C04FE03FDE8"
        "8507020000 4100"
        "85010100005823526561647920746F2067656E657261746520612033322D62797465207061796C6F6164"
        "FF"
    );

    // Order B: [primary, bundle-age, BIB, payload]
    let order_b = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F4240"
        "8507020000 4100"
        "850B030000587582010201008202820301828182015830F75FE4C37F76F046165855BD5FF72FBFD4E3A64B4695C40E2B787DA005AE819F0A2E30A2E8B325527DE8AEFB52E73D7181820158306EE5CA30AB3A1BF1E7F645EB21418FFC129BACFB69677FDAE0D08CB63159358FA86BE682538299B4B7E53C04FE03FDE8"
        "85010100005823526561647920746F2067656E657261746520612033322D62797465207061796C6F6164"
        "FF"
    );

    assert_ne!(order_a.as_slice(), order_b.as_slice());

    let keys: KeySet = serde_json::from_value(serde_json::json!({
        "keys": [{"kid": "hmackey", "kty": "oct", "alg": "HS384",
                  "key_ops": ["sign", "verify"], "k": "GisaKxorGisaKxorGisaKw"}]
    }))
    .unwrap();

    let (a, da) = parse_with_keys(&order_a, &keys);
    let (b, db) = parse_with_keys(&order_b, &keys);

    let diffs = compare_bundles(&a, &da, &b, &db, &keys);
    assert!(
        diffs.is_empty(),
        "Different block order should be equivalent: {diffs:?}"
    );
}

#[test]
fn different_bib_target_order_is_equivalent() {
    // RFC 9172 Section 3.6: "The order of elements in this list has no
    // semantic meaning"

    // Targets [1, 2]
    let targets_12 = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F4240"
        "850B030000587582010201008202820301828182015830F75FE4C37F76F046165855BD5FF72FBFD4E3A64B4695C40E2B787DA005AE819F0A2E30A2E8B325527DE8AEFB52E73D7181820158306EE5CA30AB3A1BF1E7F645EB21418FFC129BACFB69677FDAE0D08CB63159358FA86BE682538299B4B7E53C04FE03FDE8"
        "8507020000 4100"
        "85010100005823526561647920746F2067656E657261746520612033322D62797465207061796C6F6164"
        "FF"
    );

    // Targets [2, 1] with results swapped to match
    let targets_21 = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F4240"
        "850B030000587582020101008202820301828182015830 6EE5CA30AB3A1BF1E7F645EB21418FFC129BACFB69677FDAE0D08CB63159358FA86BE682538299B4B7E53C04FE03FDE8 8182015830 F75FE4C37F76F046165855BD5FF72FBFD4E3A64B4695C40E2B787DA005AE819F0A2E30A2E8B325527DE8AEFB52E73D71"
        "8507020000 4100"
        "85010100005823526561647920746F2067656E657261746520612033322D62797465207061796C6F6164"
        "FF"
    );

    assert_ne!(targets_12.as_slice(), targets_21.as_slice());

    let keys: KeySet = serde_json::from_value(serde_json::json!({
        "keys": [{"kid": "hmackey", "kty": "oct", "alg": "HS384",
                  "key_ops": ["sign", "verify"], "k": "GisaKxorGisaKxorGisaKw"}]
    }))
    .unwrap();

    let (a, da) = parse_with_keys(&targets_12, &keys);
    let (b, db) = parse_with_keys(&targets_21, &keys);

    let diffs = compare_bundles(&a, &da, &b, &db, &keys);
    assert!(
        diffs.is_empty(),
        "Different BIB target order should be equivalent: {diffs:?}"
    );
}

#[test]
fn different_bcb_target_order_is_equivalent() {
    // RFC 9172 Section 3.6: target order has no semantic meaning in BCB either.

    // BCB targets [1, 2] (from PICS 2.3 outgoing)
    let targets_12 = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F4240"
        "850C030100581D820102020182028203018182014C5477656C76653132313231328280"
        "80850702000051C225655BB0AF8CC854641DA15AB6BE9FA2"
        "8501010000583390EAB6457593379298A8724E16E61F837488E127212B59AC91F8A86287B7D07630A122C42BBA8CA26EECBCAB0F8124C2A42BDFFF"
    );

    // BCB targets [2, 1] — swapped targets and results
    let targets_21 = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F4240"
        "850C030100581D820201020182028203018182014C5477656C76653132313231328280"
        "80850702000051C225655BB0AF8CC854641DA15AB6BE9FA2"
        "8501010000583390EAB6457593379298A8724E16E61F837488E127212B59AC91F8A86287B7D07630A122C42BBA8CA26EECBCAB0F8124C2A42BDFFF"
    );

    assert_ne!(targets_12.as_slice(), targets_21.as_slice());

    let keys: KeySet = serde_json::from_value(serde_json::json!({
        "keys": [{"kid": "aesgcmkey_32", "kty": "oct", "alg": "dir", "enc": "A256GCM",
                  "key_ops": ["encrypt", "decrypt"],
                  "k": "cXdlcnR5dWlvcGFzZGZnaHF3ZXJ0eXVpb3Bhc2RmZ2g"}]
    }))
    .unwrap();

    let (a, da) = parse_with_keys(&targets_12, &keys);
    let (b, db) = parse_with_keys(&targets_21, &keys);

    let diffs = compare_bundles(&a, &da, &b, &db, &keys);
    assert!(
        diffs.is_empty(),
        "Different BCB target order should be equivalent: {diffs:?}"
    );
}

#[test]
fn different_block_numbers_is_equivalent() {
    // RFC 9171: "Block numbering is unrelated to the order in which blocks
    // are sequenced in the bundle." Block numbers are arbitrary (except 0=primary, 1=payload).
    // Same blocks with different block numbers should be equivalent.

    // Bundle-age as block number 2
    let bn_2 = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F4240"
        "8507020000 4100"
        "85010100005823526561647920746F2067656E657261746520612033322D62797465207061796C6F6164"
        "FF"
    );

    // Bundle-age as block number 5
    let bn_5 = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F4240"
        "8507050000 4100"
        "85010100005823526561647920746F2067656E657261746520612033322D62797465207061796C6F6164"
        "FF"
    );

    assert_ne!(bn_2.as_slice(), bn_5.as_slice());

    let (a, da) = parse(&bn_2);
    let (b, db) = parse(&bn_5);

    let diffs = compare_bundles(&a, &da, &b, &db, &KeySet::EMPTY);
    assert!(
        diffs.is_empty(),
        "Different block numbers should be equivalent: {diffs:?}"
    );
}

#[test]
fn encrypted_bundle_order_bcb_before_and_after_target() {
    // BCB can appear before or after its target in the wire format.
    // Both orderings are equivalent.

    // Order A: [primary, BCB, bundle-age(encrypted), payload(encrypted)]
    let order_a = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F4240"
        "850C030100581D820102020182028203018182014C5477656C76653132313231328280"
        "80850702000051C225655BB0AF8CC854641DA15AB6BE9FA2"
        "8501010000583390EAB6457593379298A8724E16E61F837488E127212B59AC91F8A86287B7D07630A122C42BBA8CA26EECBCAB0F8124C2A42BDFFF"
    );

    // Order B: [primary, bundle-age(encrypted), BCB, payload(encrypted)]
    let order_b = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F4240"
        "850702000051C225655BB0AF8CC854641DA15AB6BE9FA2"
        "850C030100581D820102020182028203018182014C5477656C76653132313231328280"
        "808501010000583390EAB6457593379298A8724E16E61F837488E127212B59AC91F8A86287B7D07630A122C42BBA8CA26EECBCAB0F8124C2A42BDFFF"
    );

    assert_ne!(order_a.as_slice(), order_b.as_slice());

    let keys: KeySet = serde_json::from_value(serde_json::json!({
        "keys": [{"kid": "aesgcmkey_32", "kty": "oct", "alg": "dir", "enc": "A256GCM",
                  "key_ops": ["encrypt", "decrypt"],
                  "k": "cXdlcnR5dWlvcGFzZGZnaHF3ZXJ0eXVpb3Bhc2RmZ2g"}]
    }))
    .unwrap();

    let (a, da) = parse_with_keys(&order_a, &keys);
    let (b, db) = parse_with_keys(&order_b, &keys);

    let diffs = compare_bundles(&a, &da, &b, &db, &keys);
    assert!(
        diffs.is_empty(),
        "BCB before/after target should be equivalent: {diffs:?}"
    );
}

#[test]
fn combined_bib_and_bcb_different_order() {
    // Bundle with both BIB and BCB — all extension blocks reordered.
    // PICS 2.5 outgoing: [primary, BCB, BIB(encrypted), bundle-age(encrypted), payload(encrypted)]

    // Order A: [primary, BCB, BIB, bundle-age, payload]
    let order_a = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F4240"
        "850C040100581F83010302020182028203018182014C5477656C76653132313231328380808085"
        "0B0300005885408ED5200C31417FBBCE95A1F19526C7E6F764C46D6F8488FED498FFA82186A58B23E09DBC956CAAACD3118DBB3301F97CFBFA6E8DB8A85B85FF9CAC1967EF9C6CE2DBBD9C8EF38CB32A3CC5EF31E71E6839666CEA17424457A1A01F70F08377099F27B4B27EFB839B18C434DF3C6FF425AC662E4817F774EE513D36AF41D8F7ED3055E53B"
        "850702000051C2B19A334CC8C895C69A5B3DCE7BDE52FA"
        "8501010000583390EAB6457593379298A8724E16E61F837488E127212B59AC91F8A86287B7D07630A122A4A2C8343500978F613F564529596403FF"
    );

    // Order B: [primary, bundle-age, BIB, BCB, payload]
    let order_b = hex_literal::hex!(
        "9F88070000820282010282028202018202820201820018281A000F4240"
        "850702000051C2B19A334CC8C895C69A5B3DCE7BDE52FA"
        "850B0300005885408ED5200C31417FBBCE95A1F19526C7E6F764C46D6F8488FED498FFA82186A58B23E09DBC956CAAACD3118DBB3301F97CFBFA6E8DB8A85B85FF9CAC1967EF9C6CE2DBBD9C8EF38CB32A3CC5EF31E71E6839666CEA17424457A1A01F70F08377099F27B4B27EFB839B18C434DF3C6FF425AC662E4817F774EE513D36AF41D8F7ED3055E53B"
        "850C040100581F83010302020182028203018182014C5477656C766531323132313283808080"
        "8501010000583390EAB6457593379298A8724E16E61F837488E127212B59AC91F8A86287B7D07630A122A4A2C8343500978F613F564529596403FF"
    );

    assert_ne!(order_a.as_slice(), order_b.as_slice());

    let keys: KeySet = serde_json::from_value(serde_json::json!({
        "keys": [{"kid": "hmackey", "kty": "oct", "alg": "HS384",
                  "key_ops": ["sign", "verify"], "k": "GisaKxorGisaKxorGisaKw"},
                 {"kid": "aesgcmkey_32", "kty": "oct", "alg": "dir", "enc": "A256GCM",
                  "key_ops": ["encrypt", "decrypt"],
                  "k": "cXdlcnR5dWlvcGFzZGZnaHF3ZXJ0eXVpb3Bhc2RmZ2g"}]
    }))
    .unwrap();

    let (a, da) = parse_with_keys(&order_a, &keys);
    let (b, db) = parse_with_keys(&order_b, &keys);

    let diffs = compare_bundles(&a, &da, &b, &db, &keys);
    assert!(
        diffs.is_empty(),
        "BIB+BCB in different order should be equivalent: {diffs:?}"
    );
}
