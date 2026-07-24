use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc::{self, Receiver, Sender, TryRecvError},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use uuid::Uuid;

use crate::model::{Tunnel, TunnelPhase};

const MAX_RETRY_ATTEMPTS: u32 = 10;

pub enum Action {
    OpenUrl(String),
}

pub struct Tick {
    pub changed: bool,
    pub actions: Vec<Action>,
}

struct Runtime {
    generation: u64,
    started_at: Instant,
    stop_requested: bool,
    stop: Sender<()>,
    worker: JoinHandle<()>,
}

struct ProcessExited {
    id: Uuid,
    generation: u64,
    status: Option<i32>,
    stderr: Vec<u8>,
}

pub struct Store {
    pub tunnels: Vec<Tunnel>,
    pub notice: Option<String>,
    phases: HashMap<Uuid, TunnelPhase>,
    desired: HashSet<Uuid>,
    runtimes: HashMap<Uuid, Runtime>,
    retry_attempts: HashMap<Uuid, u32>,
    retry_deadlines: HashMap<Uuid, Instant>,
    pending_browser: HashSet<Uuid>,
    process_events: Receiver<ProcessExited>,
    event_sender: Sender<ProcessExited>,
    storage_path: PathBuf,
    ssh_executable: PathBuf,
    next_generation: u64,
}

impl Store {
    pub fn load_default() -> Self {
        Self::load(default_storage_path())
    }

    pub fn load(storage_path: PathBuf) -> Self {
        let (tunnels, notice) = match fs::read(&storage_path) {
            Ok(data) => match serde_json::from_slice(&data) {
                Ok(tunnels) => (tunnels, None),
                Err(error) => (
                    Vec::new(),
                    Some(format!("Could not read saved tunnels: {error}")),
                ),
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => (Vec::new(), None),
            Err(error) => (
                Vec::new(),
                Some(format!("Could not read saved tunnels: {error}")),
            ),
        };
        let (event_sender, process_events) = mpsc::channel();
        Self {
            tunnels,
            notice,
            phases: HashMap::new(),
            desired: HashSet::new(),
            runtimes: HashMap::new(),
            retry_attempts: HashMap::new(),
            retry_deadlines: HashMap::new(),
            pending_browser: HashSet::new(),
            process_events,
            event_sender,
            storage_path,
            ssh_executable: PathBuf::from("/usr/bin/ssh"),
            next_generation: 0,
        }
    }

    pub fn phase(&self, id: Uuid) -> TunnelPhase {
        self.phases.get(&id).cloned().unwrap_or_default()
    }

    pub fn running_count(&self) -> usize {
        self.phases
            .values()
            .filter(|phase| phase.is_active())
            .count()
    }

    pub fn add(&mut self, tunnel: Tunnel) {
        self.tunnels.push(tunnel);
        self.save();
    }

    pub fn update(&mut self, tunnel: Tunnel) {
        let Some(index) = self.tunnels.iter().position(|saved| saved.id == tunnel.id) else {
            return;
        };
        self.stop(tunnel.id);
        self.tunnels[index] = tunnel;
        self.save();
    }

    pub fn delete(&mut self, id: Uuid) {
        self.stop(id);
        self.tunnels.retain(|tunnel| tunnel.id != id);
        self.phases.remove(&id);
        self.save();
    }

    pub fn toggle(&mut self, id: Uuid) {
        if self.desired.contains(&id) {
            self.stop(id);
        } else {
            self.start(id);
        }
    }

    pub fn start(&mut self, id: Uuid) {
        if self.desired.contains(&id) {
            return;
        }
        let Some(tunnel) = self.tunnels.iter().find(|tunnel| tunnel.id == id) else {
            return;
        };
        if !tunnel.is_safe_to_run() {
            self.phases.insert(
                id,
                TunnelPhase::Failed(
                    "This tunnel contains an invalid host or blocked SSH option.".into(),
                ),
            );
            return;
        }
        self.retry_deadlines.remove(&id);
        self.desired.insert(id);
        self.retry_attempts.insert(id, 0);
        if self.runtimes.contains_key(&id) {
            self.phases.insert(id, TunnelPhase::Starting);
        } else {
            self.launch(id);
        }
    }

    pub fn open_in_browser(&mut self, id: Uuid) -> Vec<Action> {
        let Some(tunnel) = self.tunnels.iter().find(|tunnel| tunnel.id == id) else {
            return Vec::new();
        };
        if !tunnel.is_safe_to_run() {
            self.phases.insert(
                id,
                TunnelPhase::Failed(
                    "This tunnel contains an invalid host or blocked SSH option.".into(),
                ),
            );
            return Vec::new();
        }
        let url = tunnel.browser_url();
        if self.phase(id) == TunnelPhase::Running && self.runtimes.contains_key(&id) {
            vec![Action::OpenUrl(url)]
        } else {
            self.pending_browser.insert(id);
            if !self.desired.contains(&id) {
                self.start(id);
            }
            Vec::new()
        }
    }

    pub fn stop(&mut self, id: Uuid) {
        self.desired.remove(&id);
        self.retry_attempts.remove(&id);
        self.retry_deadlines.remove(&id);
        self.pending_browser.remove(&id);
        self.phases.insert(id, TunnelPhase::Stopped);
        if let Some(runtime) = self.runtimes.get_mut(&id) {
            runtime.stop_requested = true;
            let _ = runtime.stop.send(());
        }
    }

    pub fn tick(&mut self) -> Tick {
        let mut changed = false;
        let mut actions = Vec::new();

        while let Ok(event) = self.process_events.try_recv() {
            changed |= self.process_did_exit(event);
        }

        let now = Instant::now();
        let connected: Vec<_> = self
            .runtimes
            .iter()
            .filter_map(|(id, runtime)| {
                (self.desired.contains(id)
                    && self.phase(*id) == TunnelPhase::Starting
                    && now.duration_since(runtime.started_at) >= Duration::from_millis(450))
                .then_some(*id)
            })
            .collect();
        for id in connected {
            self.retry_attempts.insert(id, 0);
            self.phases.insert(id, TunnelPhase::Running);
            if self.pending_browser.remove(&id)
                && let Some(tunnel) = self.tunnels.iter().find(|tunnel| tunnel.id == id)
            {
                actions.push(Action::OpenUrl(tunnel.browser_url()));
            }
            changed = true;
        }

        let due: Vec<_> = self
            .retry_deadlines
            .iter()
            .filter_map(|(id, deadline)| (*deadline <= now).then_some(*id))
            .collect();
        for id in due {
            self.retry_deadlines.remove(&id);
            if self.desired.contains(&id) {
                self.launch(id);
                changed = true;
            }
        }

        Tick { changed, actions }
    }

    pub fn set_notice(&mut self, notice: String) {
        self.notice = Some(notice);
    }

    pub fn shutdown(&mut self) {
        self.desired.clear();
        self.retry_deadlines.clear();
        self.pending_browser.clear();
        let runtimes: Vec<_> = self.runtimes.drain().map(|(_, runtime)| runtime).collect();
        for runtime in &runtimes {
            let _ = runtime.stop.send(());
        }
        for runtime in runtimes {
            let _ = runtime.worker.join();
        }
    }

    fn launch(&mut self, id: Uuid) {
        if self.runtimes.contains_key(&id) {
            return;
        }
        let Some(tunnel) = self.tunnels.iter().find(|tunnel| tunnel.id == id).cloned() else {
            return;
        };
        self.phases.insert(id, TunnelPhase::Starting);

        let child = Command::new(&self.ssh_executable)
            .args(tunnel.ssh_arguments())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(error) => {
                self.schedule_retry(id, error.to_string());
                return;
            }
        };

        self.next_generation += 1;
        let generation = self.next_generation;
        let sender = self.event_sender.clone();
        let (stop, stop_receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            loop {
                match stop_receiver.try_recv() {
                    Ok(()) | Err(TryRecvError::Disconnected) => {
                        let _ = child.kill();
                        break;
                    }
                    Err(TryRecvError::Empty) => {}
                }
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) => thread::sleep(Duration::from_millis(50)),
                    Err(_) => break,
                }
            }
            let status = child.wait().ok().and_then(|status| status.code());
            let mut stderr = Vec::new();
            if let Some(mut pipe) = child.stderr.take() {
                let _ = pipe.read_to_end(&mut stderr);
            }
            if stderr.len() > 16_384 {
                stderr.drain(..stderr.len() - 16_384);
            }
            let _ = sender.send(ProcessExited {
                id,
                generation,
                status,
                stderr,
            });
        });
        self.runtimes.insert(
            id,
            Runtime {
                generation,
                started_at: Instant::now(),
                stop_requested: false,
                stop,
                worker,
            },
        );
    }

    fn process_did_exit(&mut self, event: ProcessExited) -> bool {
        if self
            .runtimes
            .get(&event.id)
            .map(|runtime| runtime.generation)
            != Some(event.generation)
        {
            return false;
        }
        let stop_requested = self
            .runtimes
            .remove(&event.id)
            .is_some_and(|runtime| runtime.stop_requested);
        if !self.desired.contains(&event.id) {
            if self.tunnels.iter().any(|tunnel| tunnel.id == event.id) {
                self.phases.insert(event.id, TunnelPhase::Stopped);
            } else {
                self.phases.remove(&event.id);
            }
            return true;
        }
        if stop_requested {
            self.launch(event.id);
            return true;
        }

        let message = stderr_message(&event.stderr).unwrap_or_else(|| {
            event.status.map_or_else(
                || "SSH process terminated.".into(),
                |status| {
                    if status == 0 {
                        "SSH connection closed.".into()
                    } else {
                        format!("SSH exited with status {status}.")
                    }
                },
            )
        });
        self.schedule_retry(event.id, message);
        true
    }

    fn schedule_retry(&mut self, id: Uuid, message: String) {
        if !self.desired.contains(&id) {
            self.phases.insert(id, TunnelPhase::Stopped);
            return;
        }
        let attempt = self.retry_attempts.get(&id).copied().unwrap_or(0) + 1;
        if attempt > MAX_RETRY_ATTEMPTS {
            self.desired.remove(&id);
            self.retry_attempts.remove(&id);
            self.pending_browser.remove(&id);
            self.phases.insert(
                id,
                TunnelPhase::Failed(format!(
                    "{message} Automatic retry stopped after {MAX_RETRY_ATTEMPTS} attempts."
                )),
            );
            return;
        }
        let delay = retry_delay(attempt);
        self.retry_attempts.insert(id, attempt);
        self.retry_deadlines.insert(id, Instant::now() + delay);
        self.phases.insert(
            id,
            TunnelPhase::Retrying {
                attempt,
                max_attempts: MAX_RETRY_ATTEMPTS,
                delay_seconds: delay.as_secs(),
                message,
            },
        );
    }

    fn save(&mut self) {
        if let Err(error) = save_tunnels(&self.storage_path, &self.tunnels) {
            self.notice = Some(format!("Could not save tunnels: {error}"));
        }
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub fn retry_delay(attempt: u32) -> Duration {
    Duration::from_secs(2_u64.pow(attempt.saturating_sub(1).min(6)).min(60))
}

fn default_storage_path() -> PathBuf {
    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(path).join("relaybar/tunnels.json");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".config/relaybar/tunnels.json");
    }
    PathBuf::from("relaybar-tunnels.json")
}

fn save_tunnels(path: &Path, tunnels: &[Tunnel]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_vec_pretty(tunnels).map_err(io::Error::other)?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, data)?;
    fs::rename(temporary, path)
}

fn stderr_message(stderr: &[u8]) -> Option<String> {
    let output = String::from_utf8_lossy(stderr);
    let message = output
        .lines()
        .rev()
        .take(2)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join(" ");
    let message = message.trim();
    (!message.is_empty()).then(|| message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_delay_is_exponential_and_capped() {
        assert_eq!(retry_delay(1), Duration::from_secs(1));
        assert_eq!(retry_delay(2), Duration::from_secs(2));
        assert_eq!(retry_delay(7), Duration::from_secs(60));
        assert_eq!(retry_delay(10), Duration::from_secs(60));
    }

    #[test]
    fn persists_tunnels() {
        let path = std::env::temp_dir().join(format!("relaybar-{}.json", Uuid::new_v4()));
        let tunnel = Tunnel::new("Test".into(), 8080, "localhost".into(), 80, "host".into());
        save_tunnels(&path, std::slice::from_ref(&tunnel)).unwrap();
        let loaded: Vec<Tunnel> = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(loaded, [tunnel]);
        let _ = fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn starts_and_stops_child_process() {
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!("relaybar-{}.json", Uuid::new_v4()));
        let executable = path.with_extension("sh");
        fs::write(&executable, "#!/bin/sh\nexec sleep 60\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let mut store = Store::load(path.clone());
        store.ssh_executable = executable.clone();
        let tunnel = Tunnel::new(
            "Process test".into(),
            43_210,
            "localhost".into(),
            80,
            "host".into(),
        );
        let id = tunnel.id;
        store.add(tunnel);
        store.start(id);
        thread::sleep(Duration::from_millis(500));
        store.tick();
        assert_eq!(store.phase(id), TunnelPhase::Running);

        store.stop(id);
        for _ in 0..20 {
            store.tick();
            if !store.runtimes.contains_key(&id) {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(store.phase(id), TunnelPhase::Stopped);
        assert!(!store.runtimes.contains_key(&id));
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(executable);
    }
}
