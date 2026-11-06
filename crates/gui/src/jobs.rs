//! Runs work on a background thread and wakes the UI when it finishes.

use std::sync::{Arc, Mutex};

#[allow(dead_code)]
pub struct Job<T> {
    slot: Arc<Mutex<Option<T>>>,
}

#[allow(dead_code)]
impl<T: Send + 'static> Job<T> {
    pub fn spawn(ctx: egui::Context, f: impl FnOnce() -> T + Send + 'static) -> Self {
        let slot = Arc::new(Mutex::new(None));
        let out = Arc::clone(&slot);
        std::thread::spawn(move || {
            let value = f();
            *out.lock().unwrap_or_else(|e| e.into_inner()) = Some(value);
            ctx.request_repaint();
        });
        Job { slot }
    }

    /// The result once the job is done, exactly once.
    pub fn try_take(&mut self) -> Option<T> {
        self.slot.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn delivers_result_once() {
        let mut job = Job::spawn(egui::Context::default(), || 7);
        let start = Instant::now();
        let got = loop {
            if let Some(v) = job.try_take() {
                break v;
            }
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(got, 7);
        assert!(job.try_take().is_none());
    }
}
