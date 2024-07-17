#![no_main]

use libfuzzer_sys::fuzz_target;

use std::io::{Read, Write};
use tokio::time;

static INIT: std::sync::Once = std::sync::Once::new();
static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
static CONFIG: std::sync::OnceLock<config::Config> = std::sync::OnceLock::new();

fn get_config() -> &'static config::Config {
    CONFIG.get_or_init(|| {
        let mut filename = std::env::current_dir().unwrap();
        filename.push("fuzz/passive.config");

        config::Config::builder()
            .add_source(
                config::File::from(filename)
                    .format(config::FileFormat::Toml)
                    .required(false),
            )
            .build()
            .unwrap()
    })
}

fn get_addr() -> std::net::SocketAddr {
    match get_config().get("tcp_address") {
        Ok(r) => r,
        Err(config::ConfigError::NotFound(_)) => std::net::SocketAddr::V6(
            std::net::SocketAddrV6::new(std::net::Ipv6Addr::LOCALHOST, 4556, 0, 0),
        ),
        Err(e) => panic!("Invalid 'tcp_address' value in configuration {}", e),
    }
}

fn setup() {
    #[cfg(fuzzing)]
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
    })
    .spawn(async {
        let config = get_config();
        hardy_tcpcl::utils::logger::init(config);

        let (mut task_set, cancel_token) = hardy_tcpcl::utils::cancel::new_cancellable_set();

        hardy_tcpcl::listener::init(
            config,
            hardy_tcpcl::bpa::Bpa::new(config),
            &mut task_set,
            cancel_token,
        );

        while let Some(_) = task_set.join_next().await {}
    });
}

fuzz_target!(|data: &[u8]| {
    INIT.call_once(setup);

    let mut i = 0;
    let mut stream = loop {
        match std::net::TcpStream::connect(get_addr()) {
            Ok(stream) => break stream,
            Err(e) => {
                if e.kind() == std::io::ErrorKind::ConnectionRefused {
                    std::thread::sleep(time::Duration::from_secs(1));
                    i += 1;
                    if i < 10 {
                        continue;
                    }
                }
                panic!("Failed to connect: {}", e);
            }
        }
    };

    let mut stream_cloned = stream.try_clone().unwrap();
    RT.get().unwrap().spawn_blocking(move || {
        let mut buf = [0u8; 1024];
        loop {
            match stream_cloned.read(&mut buf) {
                Ok(0) | Err(_) => break,
                _ => {}
            }
        }
    });

    stream.write_all(data).unwrap();
    let _ = stream.shutdown(std::net::Shutdown::Write);
});
