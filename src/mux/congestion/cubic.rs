use std::time::Instant;

use super::CongestionControl;

/// CUBIC-style congestion control
pub struct Cubic {
    cwnd: f64,
    beta: f64,
    cee: f64,
    last_loss: Option<Instant>,
    cwnd_max: f64,
}

impl Cubic {
    /// Creates a new Cubic instance
    pub fn new(beta: f64, cee: f64) -> Self {
        Self {
            cwnd: 1.0,
            beta,
            cee,
            last_loss: None,
            cwnd_max: 1000.0,
        }
    }

    fn recalculate_cwnd(&mut self) {
        if let Some(last_loss) = self.last_loss {
            let kay = (self.cwnd_max * (1.0 - self.beta) / self.cee).powf(0.3333);
            self.cwnd = (self.cee * (last_loss.elapsed().as_secs_f64() * 1.5 - kay).powi(3)
                + self.cwnd_max)
                .max(4.0);
        }
    }
}

impl CongestionControl for Cubic {
    fn cwnd(&self) -> usize {
        self.cwnd as usize
    }

    fn mark_ack(&mut self) {
        // tracing::debug!("ack => {:.2}", self.cwnd);
        // if no last_loss, just exponentially increase
        let max_cwnd = self.cwnd + (1.0f64).min(64.0 / self.cwnd);
        self.cwnd = max_cwnd;
        // recalculate; if there's a last loss this will fix things
        self.recalculate_cwnd();
        self.cwnd = self.cwnd.min(max_cwnd);
    }

    fn mark_loss(&mut self) {
        tracing::debug!("loss!!!!!!!!!!!!!!! => {:.2}", self.cwnd());
        self.last_loss = Some(Instant::now());
        self.cwnd_max = self.cwnd;
        self.recalculate_cwnd()
    }
}
