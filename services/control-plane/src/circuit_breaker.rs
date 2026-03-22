//! Per-node circuit breaker with a three-state machine.
//!
//! State transitions:
//! ```text
//! Closed ──(5 failures in window)──► Open ──(30s cooldown)──► HalfOpen
//!   ▲                                                              │
//!   └──────────────(1 success)────────────────────────────────────┘
//!                                   ▲
//!   Open ◄──────(failure in HalfOpen)──────────────────────────────┘
//! ```

use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub enum CircuitState {
    /// Normal operation; requests pass through.
    Closed,
    /// Too many failures; requests are blocked until cooldown expires.
    Open,
    /// Cooldown elapsed; one probe request is allowed to test recovery.
    HalfOpen,
}

#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    pub state: CircuitState,
    /// Consecutive failure count (reset on success).
    pub failure_count: u32,
    /// Number of failures required to open the circuit.
    pub failure_threshold: u32,
    /// Timestamp of the most recent failure.
    pub last_failure: Option<Instant>,
    /// How long to wait in Open state before transitioning to HalfOpen.
    pub cooldown: Duration,
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self {
            state: CircuitState::Closed,
            failure_count: 0,
            failure_threshold: 5,
            last_failure: None,
            cooldown: Duration::from_secs(30),
        }
    }
}

impl CircuitBreaker {
    /// Returns `true` if a request should be allowed through.
    ///
    /// Side effect: if `Open` and cooldown has elapsed, transitions to `HalfOpen`.
    pub fn allow_request(&mut self) -> bool {
        match self.state {
            CircuitState::Closed => true,
            CircuitState::Open => {
                let elapsed = self
                    .last_failure
                    .map(|t| t.elapsed())
                    .unwrap_or(Duration::ZERO);
                if elapsed >= self.cooldown {
                    self.state = CircuitState::HalfOpen;
                    true
                } else {
                    false
                }
            }
            CircuitState::HalfOpen => true,
        }
    }

    /// Record a successful response.  Resets the circuit to Closed.
    pub fn record_success(&mut self) {
        self.failure_count = 0;
        self.state = CircuitState::Closed;
    }

    /// Record a failed response.
    ///
    /// - Closed: increment counter; open if threshold reached.
    /// - HalfOpen: probe failed → re-open immediately.
    /// - Open: update timestamp (already open).
    pub fn record_failure(&mut self) {
        self.failure_count += 1;
        self.last_failure = Some(Instant::now());
        match self.state {
            CircuitState::Closed => {
                if self.failure_count >= self.failure_threshold {
                    self.state = CircuitState::Open;
                }
            }
            CircuitState::HalfOpen => {
                self.state = CircuitState::Open;
            }
            CircuitState::Open => {}
        }
    }

    /// Human-readable state name for metrics / logging.
    pub fn state_name(&self) -> &'static str {
        match self.state {
            CircuitState::Closed => "closed",
            CircuitState::Open => "open",
            CircuitState::HalfOpen => "half_open",
        }
    }
}
