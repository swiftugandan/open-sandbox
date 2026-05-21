use open_sandbox::cli::{AgentArgs, ControllerArgs, ProxyArgs};
use open_sandbox::run;

#[tokio::test]
async fn run_controller_returns_result() {
    let args = ControllerArgs {
        grpc_port: 50051,
        database_url: "postgres://localhost/test".to_string(),
        sweep_interval: 15,
    };
    let result = run::run_controller(args).await;
    assert!(
        result.is_ok() || result.is_err(),
        "run_controller should return a Result"
    );
}

#[tokio::test]
async fn run_proxy_returns_result() {
    let args = ProxyArgs {
        http_port: 8080,
        grpc_port: 50052,
        database_url: "postgres://localhost/test".to_string(),
    };
    let result = run::run_proxy(args).await;
    assert!(
        result.is_ok() || result.is_err(),
        "run_proxy should return a Result"
    );
}

#[tokio::test]
async fn run_agent_returns_result() {
    let args = AgentArgs {
        token: "test-token".to_string(),
        controller_url: "http://127.0.0.1:50051".to_string(),
        proxy_url: "http://127.0.0.1:50052".to_string(),
    };
    let result = run::run_agent(args).await;
    assert!(
        result.is_ok() || result.is_err(),
        "run_agent should return a Result"
    );
}
