//! One running game: preparation, launch, monitoring, and termination of the
//! backend's whole process tree, off the caller's thread.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use crate::controller_guard::ControllerMouseGuard;
use crate::launch::{LaunchPlan, process_tree_is_running, terminate_process_tree_id};

/// A game that exits this soon with a failure status never really started.
const STARTUP_WINDOW: Duration = Duration::from_secs(10);
/// TERMINATE asks politely first, then stops the tree outright.
const FORCE_AFTER: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionEvent {
    /// A user-facing preparation step, such as first-time Wine setup.
    Phase(String),
    Started {
        process_id: u32,
    },
    /// The backend and every process it started have exited.
    Exited {
        message: String,
        failed: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionPhase {
    Preparing,
    Running,
    Terminating,
    Finished,
}

pub struct GameSession {
    pub catalog_id: String,
    pub title: String,
    pub log_file: Option<PathBuf>,
    events: Receiver<SessionEvent>,
    cancel: Arc<AtomicBool>,
    created: Instant,
    process_id: Option<u32>,
    started_at: Option<Instant>,
    termination_requested_at: Option<Instant>,
    forced: bool,
    finished: bool,
}

impl GameSession {
    /// Starts `plan` on a worker thread. `notify` runs whenever an event is
    /// ready, so a GUI can repaint instead of polling on a timer.
    pub fn start(
        plan: LaunchPlan,
        catalog_id: impl Into<String>,
        title: impl Into<String>,
        notify: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        let title = title.into();
        let (sender, events) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let log_file = plan.log_file.clone();
        let worker_cancel = Arc::clone(&cancel);
        let worker_title = title.clone();
        let worker_log = log_file.clone();
        let refusal = plan.refusal.clone();
        let notify = Arc::new(notify);
        thread::spawn(move || {
            let send = |event| {
                let _ = sender.send(event);
                notify();
            };
            let guard = match ControllerMouseGuard::acquire_if_available() {
                Ok(guard) => guard,
                Err(error) => {
                    send(SessionEvent::Exited {
                        message: format!(
                            "Could not suspend the desktop controller-to-mouse mapping; {worker_title} was not launched: {error}"
                        ),
                        failed: true,
                    });
                    return;
                }
            };
            if let Some(refusal) = &refusal {
                refusal.clear();
            }
            let mut child =
                match plan.spawn(&|phase: &str| send(SessionEvent::Phase(phase.to_owned()))) {
                    Ok(child) => child,
                    Err(error) => {
                        drop(guard);
                        send(SessionEvent::Exited {
                            message: format!("Could not launch {worker_title}: {error}"),
                            failed: true,
                        });
                        return;
                    }
                };
            let process_id = child.id();
            let started = Instant::now();
            send(SessionEvent::Started { process_id });
            if worker_cancel.load(Ordering::SeqCst) {
                let _ = terminate_process_tree_id(process_id, false);
            }
            let status = child.wait();
            while process_tree_is_running(process_id) {
                thread::sleep(Duration::from_millis(100));
            }
            let guard_result = guard.map(ControllerMouseGuard::release).transpose();
            let cancelled = worker_cancel.load(Ordering::SeqCst);
            let refused = refusal
                .as_ref()
                .filter(|_| !cancelled)
                .and_then(|refusal| Some((refusal.reason()?, &refusal.log)));
            let (mut message, failed) = match (status, refused) {
                (Ok(_), _) if cancelled => (format!("{worker_title} was terminated."), false),
                (Ok(_), Some((reason, log))) => (
                    format!(
                        "{worker_title} could not start: {reason} (RetroBat's EmulatorLauncher log: {})",
                        log.display()
                    ),
                    true,
                ),
                (Ok(status), None) if status.success() => {
                    (format!("{worker_title} closed."), false)
                }
                (Ok(status), None) if started.elapsed() < STARTUP_WINDOW => {
                    let mut message = format!(
                        "{worker_title} stopped while starting ({status}); the backend did not keep the game running."
                    );
                    if let Some(log) = worker_log.as_ref().filter(|log| log.is_file()) {
                        message.push_str(&format!(" Backend log: {}", log.display()));
                    }
                    (message, true)
                }
                (Ok(status), None) => (format!("{worker_title} closed ({status})."), false),
                (Err(error), _) => (format!("Could not monitor {worker_title}: {error}"), true),
            };
            if let Err(error) = guard_result {
                message.push_str(&format!(
                    " The desktop controller-to-mouse mapping could not be restored: {error}"
                ));
            }
            send(SessionEvent::Exited { message, failed });
        });
        Self {
            catalog_id: catalog_id.into(),
            title,
            log_file,
            events,
            cancel,
            created: Instant::now(),
            process_id: None,
            started_at: None,
            termination_requested_at: None,
            forced: false,
            finished: false,
        }
    }

    /// Drains pending events and advances termination; call regularly.
    pub fn poll(&mut self) -> Vec<SessionEvent> {
        let events = self.events.try_iter().collect::<Vec<_>>();
        for event in &events {
            match event {
                SessionEvent::Started { process_id } => {
                    self.process_id = Some(*process_id);
                    self.started_at = Some(Instant::now());
                }
                SessionEvent::Exited { .. } => self.finished = true,
                SessionEvent::Phase(_) => {}
            }
        }
        if let (Some(requested), Some(process_id)) =
            (self.termination_requested_at, self.process_id)
            && !self.forced
            && !self.finished
            && requested.elapsed() >= FORCE_AFTER
        {
            self.forced = true;
            let _ = terminate_process_tree_id(process_id, true);
        }
        events
    }

    /// Stops the backend's whole process tree, or prevents it from starting
    /// when it is still being prepared.
    pub fn terminate(&mut self) {
        if self.termination_requested_at.is_some() || self.finished {
            return;
        }
        self.cancel.store(true, Ordering::SeqCst);
        self.termination_requested_at = Some(Instant::now());
        if let Some(process_id) = self.process_id {
            let _ = terminate_process_tree_id(process_id, false);
        }
    }

    pub fn phase(&self) -> SessionPhase {
        if self.finished {
            SessionPhase::Finished
        } else if self.termination_requested_at.is_some() {
            SessionPhase::Terminating
        } else if self.process_id.is_none() {
            SessionPhase::Preparing
        } else {
            SessionPhase::Running
        }
    }

    pub fn process_id(&self) -> Option<u32> {
        self.process_id
    }

    /// Time since PLAY was pressed.
    pub fn age(&self) -> Duration {
        self.created.elapsed()
    }

    /// Time since the backend process started, if it has.
    pub fn running_for(&self) -> Option<Duration> {
        self.started_at.map(|started| started.elapsed())
    }

    pub fn tree_is_running(&self) -> bool {
        self.process_id.is_some_and(process_tree_is_running)
    }
}
