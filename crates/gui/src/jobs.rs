//! Runs work on a background thread and wakes the UI when it finishes, also when
//! it panics, so a failed job can never look like one that is still running.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex};

/// The job's closure panicked. Whatever it was doing may be half done.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobPanicked;

enum Slot<T> {
    Running,
    Done(T),
    Panicked,
    Taken,
}

pub struct Job<T> {
    slot: Arc<Mutex<Slot<T>>>,
}

impl<T: Send + 'static> Job<T> {
    pub fn spawn(ctx: egui::Context, f: impl FnOnce() -> T + Send + 'static) -> Self {
        let slot = Arc::new(Mutex::new(Slot::Running));
        let out = Arc::clone(&slot);
        std::thread::spawn(move || {
            let outcome = match catch_unwind(AssertUnwindSafe(f)) {
                Ok(value) => Slot::Done(value),
                Err(payload) => {
                    // Dropping the payload can itself panic, and nothing would catch that.
                    std::mem::forget(payload);
                    Slot::Panicked
                }
            };
            *out.lock().unwrap_or_else(|e| e.into_inner()) = outcome;
            ctx.request_repaint();
        });
        Job { slot }
    }

    /// `None` while running and after the outcome was taken. Otherwise the outcome,
    /// exactly once.
    pub fn poll(&mut self) -> Option<Result<T, JobPanicked>> {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        match std::mem::replace(&mut *slot, Slot::Taken) {
            Slot::Running => {
                *slot = Slot::Running;
                None
            }
            Slot::Done(value) => Some(Ok(value)),
            Slot::Panicked => Some(Err(JobPanicked)),
            Slot::Taken => None,
        }
    }

    /// The result once the job is done, exactly once. A panic reads as `None` here,
    /// so anything whose failure the user must learn about uses [`Job::poll`].
    #[allow(dead_code)]
    pub fn try_take(&mut self) -> Option<T> {
        self.poll()?.ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    fn wait<T: Send + 'static>(job: &mut Job<T>) -> Result<T, JobPanicked> {
        let start = Instant::now();
        loop {
            if let Some(v) = job.poll() {
                return v;
            }
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(5));
        }
    }

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

    #[test]
    fn a_panic_is_reported_once_and_is_not_a_result() {
        let mut job: Job<i32> = Job::spawn(egui::Context::default(), || panic!("boom"));
        assert_eq!(wait(&mut job), Err(JobPanicked));
        assert!(job.poll().is_none());
        assert!(job.try_take().is_none());
    }

    #[test]
    fn a_panic_still_wakes_the_ui() {
        let ctx = egui::Context::default();
        let woken = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&woken);
        ctx.set_request_repaint_callback(move |_| flag.store(true, Ordering::SeqCst));
        let mut job: Job<i32> = Job::spawn(ctx, || panic!("boom"));
        assert_eq!(wait(&mut job), Err(JobPanicked));
        assert!(woken.load(Ordering::SeqCst));
    }

    #[test]
    fn a_running_job_polls_none() {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let mut job = Job::spawn(egui::Context::default(), move || rx.recv().is_ok());
        assert!(job.poll().is_none());
        tx.send(()).unwrap();
        assert_eq!(wait(&mut job), Ok(true));
    }
}
