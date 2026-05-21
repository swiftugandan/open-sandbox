use std::time::Duration;

use open_sandbox_contracts::constants::{RECONNECT_BASE_DELAY, RECONNECT_MAX_DELAY};

pub struct ExponentialBackoff {
    base: Duration,
    max: Duration,
    current: Duration,
}

impl ExponentialBackoff {
    pub fn new() -> Self {
        Self {
            base: RECONNECT_BASE_DELAY,
            max: RECONNECT_MAX_DELAY,
            current: RECONNECT_BASE_DELAY,
        }
    }

    pub fn next_delay(&mut self) -> Duration {
        todo!()
    }

    pub fn reset(&mut self) {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_delay_is_base() {
        let mut backoff = ExponentialBackoff::new();
        assert_eq!(backoff.next_delay(), RECONNECT_BASE_DELAY);
    }

    #[test]
    fn delay_doubles_up_to_max() {
        let mut backoff = ExponentialBackoff::new();
        let _ = backoff.next_delay();

        let second = backoff.next_delay();
        assert_eq!(second, RECONNECT_BASE_DELAY * 2);

        let third = backoff.next_delay();
        assert_eq!(third, RECONNECT_BASE_DELAY * 4);

        for _ in 0..20 {
            let _ = backoff.next_delay();
        }
        let capped = backoff.next_delay();
        assert_eq!(capped, RECONNECT_MAX_DELAY);
    }

    #[test]
    fn reset_restores_base_delay() {
        let mut backoff = ExponentialBackoff::new();
        let _ = backoff.next_delay();
        let _ = backoff.next_delay();

        backoff.reset();
        assert_eq!(backoff.next_delay(), RECONNECT_BASE_DELAY);
    }
}
