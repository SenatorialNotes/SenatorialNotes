use std::path::Path;
use std::sync::mpsc::{self, Receiver, TryRecvError};

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};

use crate::{Error, Result};

/// Owns the native filesystem watcher for an open vault.
///
/// Events are deliberately reduced to a "rescan needed" signal. The Markdown
/// tree remains authoritative, and a full verification scan is safer than
/// trying to infer state from coalesced or reordered backend events.
pub struct VaultWatcher {
    _watcher: RecommendedWatcher,
    receiver: Receiver<notify::Result<Event>>,
}

impl VaultWatcher {
    pub fn new(root: &Path) -> Result<Self> {
        let (sender, receiver) = mpsc::channel::<notify::Result<Event>>();
        let mut watcher = notify::recommended_watcher(move |event| {
            let _send_result = sender.send(event);
        })
        .map_err(|error| Error::Watcher(error.to_string()))?;
        watcher
            .watch(root, RecursiveMode::Recursive)
            .map_err(|error| Error::Watcher(error.to_string()))?;
        Ok(Self {
            _watcher: watcher,
            receiver,
        })
    }

    /// Drains queued events and reports whether the vault should be rescanned.
    pub fn drain_changes(&self) -> Result<bool> {
        let mut changed = false;
        loop {
            match self.receiver.try_recv() {
                Ok(Ok(_event)) => changed = true,
                Ok(Err(error)) => return Err(Error::Watcher(error.to_string())),
                Err(TryRecvError::Empty) => return Ok(changed),
                Err(TryRecvError::Disconnected) => {
                    return Err(Error::Watcher("watcher channel disconnected".into()));
                }
            }
        }
    }
}
