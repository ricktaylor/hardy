use hardy_bpa::{
    Bytes,
    cla::{ClaAddress, ClaAddressType},
};

// ClaAddress round-trips through (ClaAddressType, Bytes) conversion.
#[test]
fn test_address_parsing() {
    // TCP address: parse from string representation
    let tcp_addr: core::net::SocketAddr = "192.168.1.1:4556".parse().unwrap();
    let cla_addr = ClaAddress::Tcp(tcp_addr);
    assert_eq!(cla_addr.address_type(), ClaAddressType::Tcp);

    // Round-trip: ClaAddress -> (type, bytes) -> ClaAddress
    let (addr_type, bytes): (ClaAddressType, Bytes) = cla_addr.clone().into();
    assert_eq!(addr_type, ClaAddressType::Tcp);
    let recovered = ClaAddress::try_from((addr_type, bytes)).unwrap();
    assert_eq!(recovered, cla_addr);

    // IPv6 TCP address
    let tcp_v6: core::net::SocketAddr = "[::1]:4556".parse().unwrap();
    let cla_v6 = ClaAddress::Tcp(tcp_v6);
    let (t, b): (ClaAddressType, Bytes) = cla_v6.clone().into();
    let recovered = ClaAddress::try_from((t, b)).unwrap();
    assert_eq!(recovered, cla_v6);

    // Private address
    let private_data = Bytes::from_static(b"\x01\x02\x03\x04");
    let private_addr = ClaAddress::Private(private_data.clone());
    assert_eq!(private_addr.address_type(), ClaAddressType::Private);

    let (t, b): (ClaAddressType, Bytes) = private_addr.clone().into();
    assert_eq!(t, ClaAddressType::Private);
    let recovered = ClaAddress::try_from((t, b)).unwrap();
    assert_eq!(recovered, private_addr);

    // Invalid TCP bytes should error
    let bad_bytes = Bytes::from_static(b"not-a-socket-addr");
    let result = ClaAddress::try_from((ClaAddressType::Tcp, bad_bytes));
    assert!(result.is_err());
}
