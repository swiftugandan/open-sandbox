pub mod constants;
pub mod error;
pub mod types;

pub mod controller {
    tonic::include_proto!("open_sandbox.controller");
}

pub mod proxy {
    tonic::include_proto!("open_sandbox.proxy");
}

pub mod api {
    tonic::include_proto!("open_sandbox.api");
}
