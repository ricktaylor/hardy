pub mod bpa {
    tonic::include_proto!("bpa");
}

pub mod google {
    pub mod rpc {
        tonic::include_proto!("google.rpc");

        impl From<tonic::Status> for Status {
            fn from(status: tonic::Status) -> Self {
                Self {
                    code: status.code().into(),
                    message: status.message().to_string(),
                    details: Vec::new(),
                }
            }
        }
    }
}
