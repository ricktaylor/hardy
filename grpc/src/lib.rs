// Init gRPC services
grpc::init(&config, bpa.clone(), &mut task_set, cancel_token.clone());
