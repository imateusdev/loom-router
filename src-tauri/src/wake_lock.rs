use crate::config::SleepPreventionMode;
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub(crate) const ACTIVITY_GRACE: Duration = Duration::from_secs(15 * 60);
const ACQUIRE_RETRY: Duration = Duration::from_secs(5);

enum WakeCommand {
    SetMode(SleepPreventionMode),
    SetProxyRunning(bool),
    BeginActivity,
    EndActivity,
}

trait WakeBackend: Send {
    fn acquire(&mut self) -> anyhow::Result<()>;
    fn release(&mut self);
}

struct SystemWakeBackend {
    lock: Option<keepawake::KeepAwake>,
}

impl WakeBackend for SystemWakeBackend {
    fn acquire(&mut self) -> anyhow::Result<()> {
        if self.lock.is_none() {
            self.lock = Some(
                keepawake::Builder::default()
                    .display(false)
                    .idle(true)
                    .sleep(false)
                    .reason("LoomRouter model activity")
                    .app_name("LoomRouter")
                    .app_reverse_domain("dev.loomrouter.app")
                    .create()?,
            );
        }
        Ok(())
    }

    fn release(&mut self) {
        self.lock = None;
    }
}

#[derive(Clone)]
pub(crate) struct WakeController {
    tx: Option<mpsc::Sender<WakeCommand>>,
}

impl WakeController {
    pub(crate) fn new(mode: SleepPreventionMode) -> Self {
        Self::with_backend(
            mode,
            ACTIVITY_GRACE,
            ACQUIRE_RETRY,
            Box::new(SystemWakeBackend { lock: None }),
        )
    }

    pub(crate) fn disabled() -> Self {
        Self { tx: None }
    }

    fn with_backend(
        mode: SleepPreventionMode,
        grace: Duration,
        retry: Duration,
        backend: Box<dyn WakeBackend>,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("loom-wake-lock".into())
            .spawn(move || run_controller(rx, WakeState::new(mode), grace, retry, backend))
            .expect("wake lock thread");
        Self { tx: Some(tx) }
    }

    pub(crate) fn set_mode(&self, mode: SleepPreventionMode) {
        self.send(WakeCommand::SetMode(mode));
    }

    pub(crate) fn set_proxy_running(&self, running: bool) {
        self.send(WakeCommand::SetProxyRunning(running));
    }

    pub(crate) fn begin_activity(&self) -> WakeLease {
        self.send(WakeCommand::BeginActivity);
        WakeLease {
            tx: self.tx.clone(),
        }
    }

    fn send(&self, command: WakeCommand) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(command);
        }
    }
}

pub(crate) struct WakeLease {
    tx: Option<mpsc::Sender<WakeCommand>>,
}

#[cfg(test)]
pub(crate) fn recording_controller(
    mode: SleepPreventionMode,
) -> (WakeController, mpsc::Receiver<bool>) {
    struct ReportingBackend(mpsc::Sender<bool>);

    impl WakeBackend for ReportingBackend {
        fn acquire(&mut self) -> anyhow::Result<()> {
            self.0.send(true).unwrap();
            Ok(())
        }

        fn release(&mut self) {
            self.0.send(false).unwrap();
        }
    }

    let (events_tx, events_rx) = mpsc::channel();
    (
        WakeController::with_backend(
            mode,
            ACTIVITY_GRACE,
            ACQUIRE_RETRY,
            Box::new(ReportingBackend(events_tx)),
        ),
        events_rx,
    )
}

impl Drop for WakeLease {
    fn drop(&mut self) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(WakeCommand::EndActivity);
        }
    }
}

fn run_controller(
    rx: mpsc::Receiver<WakeCommand>,
    mut state: WakeState,
    grace: Duration,
    retry: Duration,
    mut backend: Box<dyn WakeBackend>,
) {
    let mut held = false;
    let mut retry_at = None;
    loop {
        let now = Instant::now();
        let should_hold = state.should_hold(now);
        let retry_due = retry_at.is_none_or(|deadline| now >= deadline);
        if should_hold && !held && retry_due {
            match backend.acquire() {
                Ok(()) => {
                    held = true;
                    retry_at = None;
                }
                Err(error) => {
                    retry_at = Some(now + retry);
                    tracing::warn!(%error, "failed to prevent idle system sleep");
                }
            }
        } else if !should_hold && held {
            backend.release();
            held = false;
            retry_at = None;
        } else if !should_hold {
            retry_at = None;
        }

        let deadline = [state.next_deadline(now), retry_at]
            .into_iter()
            .flatten()
            .min();
        let command = match deadline {
            Some(deadline) => match rx.recv_timeout(deadline.saturating_duration_since(now)) {
                Ok(command) => Some(command),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => None,
            },
            None => rx.recv().ok(),
        };
        let Some(command) = command else {
            if held {
                backend.release();
            }
            break;
        };
        match command {
            WakeCommand::SetMode(mode) => state.set_mode(mode),
            WakeCommand::SetProxyRunning(running) => state.set_proxy_running(running),
            WakeCommand::BeginActivity => state.begin_activity(),
            WakeCommand::EndActivity => state.end_activity(Instant::now(), grace),
        }
    }
}

struct WakeState {
    mode: SleepPreventionMode,
    proxy_running: bool,
    active_requests: usize,
    idle_until: Option<Instant>,
}

impl WakeState {
    fn new(mode: SleepPreventionMode) -> Self {
        Self {
            mode,
            proxy_running: false,
            active_requests: 0,
            idle_until: None,
        }
    }

    fn set_mode(&mut self, mode: SleepPreventionMode) {
        self.mode = mode;
    }

    fn set_proxy_running(&mut self, running: bool) {
        self.proxy_running = running;
        if !running {
            self.active_requests = 0;
            self.idle_until = None;
        }
    }

    fn begin_activity(&mut self) {
        self.active_requests = self.active_requests.saturating_add(1);
        self.idle_until = None;
    }

    fn end_activity(&mut self, now: Instant, grace: Duration) {
        self.active_requests = self.active_requests.saturating_sub(1);
        if self.active_requests == 0 {
            self.idle_until = Some(now + grace);
        }
    }

    fn should_hold(&self, now: Instant) -> bool {
        if !self.proxy_running {
            return false;
        }
        match self.mode {
            SleepPreventionMode::Never => false,
            SleepPreventionMode::Always => true,
            SleepPreventionMode::WhileActive => {
                self.active_requests > 0 || self.idle_until.is_some_and(|until| now < until)
            }
        }
    }

    fn next_deadline(&self, now: Instant) -> Option<Instant> {
        (self.proxy_running
            && self.mode == SleepPreventionMode::WhileActive
            && self.active_requests == 0)
            .then_some(self.idle_until)
            .flatten()
            .filter(|deadline| *deadline >= now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SleepPreventionMode;
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    struct ReportingBackend(mpsc::Sender<bool>);

    impl WakeBackend for ReportingBackend {
        fn acquire(&mut self) -> anyhow::Result<()> {
            self.0.send(true).unwrap();
            Ok(())
        }

        fn release(&mut self) {
            self.0.send(false).unwrap();
        }
    }

    #[test]
    fn controller_applies_mode_changes_on_its_backend_thread() {
        let (events_tx, events_rx) = mpsc::channel();
        let controller = WakeController::with_backend(
            SleepPreventionMode::Always,
            Duration::from_secs(900),
            Duration::from_secs(5),
            Box::new(ReportingBackend(events_tx)),
        );

        controller.set_proxy_running(true);
        assert!(events_rx.recv_timeout(Duration::from_secs(1)).unwrap());

        controller.set_mode(SleepPreventionMode::Never);
        assert!(!events_rx.recv_timeout(Duration::from_secs(1)).unwrap());
    }

    #[test]
    fn an_activity_lease_keeps_the_backend_acquired_until_policy_changes() {
        let (events_tx, events_rx) = mpsc::channel();
        let controller = WakeController::with_backend(
            SleepPreventionMode::WhileActive,
            Duration::from_secs(900),
            Duration::from_secs(5),
            Box::new(ReportingBackend(events_tx)),
        );
        controller.set_proxy_running(true);

        let _lease = controller.begin_activity();
        assert!(events_rx.recv_timeout(Duration::from_secs(1)).unwrap());

        controller.set_mode(SleepPreventionMode::Never);
        assert!(!events_rx.recv_timeout(Duration::from_secs(1)).unwrap());
    }

    #[test]
    fn acquisition_failure_is_retried_without_another_command() {
        struct FailsOnceBackend {
            attempts: Arc<Mutex<usize>>,
            acquired: mpsc::Sender<()>,
        }

        impl WakeBackend for FailsOnceBackend {
            fn acquire(&mut self) -> anyhow::Result<()> {
                let mut attempts = self.attempts.lock().unwrap();
                *attempts += 1;
                if *attempts == 1 {
                    anyhow::bail!("temporary platform failure");
                }
                self.acquired.send(()).unwrap();
                Ok(())
            }

            fn release(&mut self) {}
        }

        let attempts = Arc::new(Mutex::new(0));
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let controller = WakeController::with_backend(
            SleepPreventionMode::Always,
            Duration::from_secs(900),
            Duration::from_millis(10),
            Box::new(FailsOnceBackend {
                attempts: attempts.clone(),
                acquired: acquired_tx,
            }),
        );

        controller.set_proxy_running(true);
        acquired_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(*attempts.lock().unwrap(), 2);
    }

    #[test]
    fn activity_mode_holds_during_a_request_and_for_the_grace_period() {
        let now = Instant::now();
        let mut state = WakeState::new(SleepPreventionMode::WhileActive);
        state.set_proxy_running(true);

        state.begin_activity();
        assert!(state.should_hold(now));

        state.end_activity(now, Duration::from_secs(900));
        assert!(state.should_hold(now + Duration::from_secs(899)));
        assert!(!state.should_hold(now + Duration::from_secs(900)));
    }

    #[test]
    fn always_mode_follows_the_proxy_instead_of_request_activity() {
        let now = Instant::now();
        let mut state = WakeState::new(SleepPreventionMode::Always);

        assert!(!state.should_hold(now));
        state.set_proxy_running(true);
        assert!(state.should_hold(now));
        state.set_proxy_running(false);
        assert!(!state.should_hold(now));
    }

    #[test]
    fn never_mode_releases_even_with_an_active_request() {
        let now = Instant::now();
        let mut state = WakeState::new(SleepPreventionMode::WhileActive);
        state.set_proxy_running(true);
        state.begin_activity();
        assert!(state.should_hold(now));

        state.set_mode(SleepPreventionMode::Never);
        assert!(!state.should_hold(now));
    }

    #[test]
    fn stopping_the_proxy_clears_stale_activity() {
        let now = Instant::now();
        let mut state = WakeState::new(SleepPreventionMode::WhileActive);
        state.set_proxy_running(true);
        state.begin_activity();
        state.end_activity(now, Duration::from_secs(900));

        state.set_proxy_running(false);
        state.set_proxy_running(true);
        assert!(!state.should_hold(now));
    }
}
