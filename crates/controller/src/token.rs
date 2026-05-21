pub trait TokenValidator: Send + Sync {
    fn validate(&self, token: &str) -> bool;
}
