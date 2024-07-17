#![no_main]

use libfuzzer_sys::fuzz_target;

#[cfg(fuzzing)]
mod fuzz {

    use std::io::{Read, Write};
    use tokio::time;

    static INIT: std::sync::Once = std::sync::Once::new();
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    static mut ADDR: std::net::SocketAddr = std::net::SocketAddr::V6(std::net::SocketAddrV6::new(
        std::net::Ipv6Addr::LOCALHOST,
        4556,
        0,
        0,
    ));

    async fn async_setup() {
        let mut filename = std::env::current_dir().unwrap();
        filename.push("fuzz/passive.config");

        let config = config::Config::builder()
            .add_source(
                config::File::from(filename)
                    .format(config::FileFormat::Toml)
                    .required(false),
            )
            .build()
            .unwrap();

        hardy_tcpcl::utils::logger::init(&config);

        unsafe {
            ADDR = hardy_tcpcl::utils::settings::get_with_default(&config, "tcp_address", ADDR)
                .unwrap();
        }

        let (mut task_set, cancel_token) = hardy_tcpcl::utils::cancel::new_cancellable_set();

        let bpa = hardy_tcpcl::bpa::Bpa::new(&config);

        hardy_tcpcl::listener::init(&config, bpa, &mut task_set, cancel_token.clone());

        while let Some(_) = task_set.join_next().await {}
    }

    fn setup() {
        RT.get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap()
        })
        .spawn(async_setup());
    }

    pub fn do_fuzz(data: &[u8]) {
        INIT.call_once(setup);

        // fuzzed code goes here
        let addr = unsafe { ADDR.clone() };

        let mut i = 0;
        let mut stream = loop {
            match std::net::TcpStream::connect(addr) {
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
    }
}

#[cfg(not(fuzzing))]
mod fuzz {
    pub fn do_fuzz(_data: &[u8]) {
        unimplemented!()
    }
}

fuzz_target!(|data: &[u8]| {
    fuzz::do_fuzz(data);
});
