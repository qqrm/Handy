//! MPRIS sessions are addressed by their original unique D-Bus owner, never by
//! a name which another player can acquire. No shell tools run in this backend.
use super::*;
use futures_util::StreamExt;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{OnceLock, Weak};
use std::time::Duration;
use tokio::runtime::Runtime;
use tokio::sync::{mpsc as async_mpsc, oneshot};
use tokio::task::JoinHandle;
use zbus::names::{BusName, OwnedUniqueName};
use zbus::proxy::CacheProperties;
use zbus::zvariant::OwnedValue;
use zbus::{Connection, MatchRule, MessageStream, Proxy};

const PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
const READ_TIMEOUT: Duration = Duration::from_millis(500);
const SETUP_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_SESSIONS: usize = MAX_MEDIA_LEASES;

// The dedicated executor keeps signals and outstanding replies moving while the
// recording worker waits for its next command. It never runs on the UI thread.
fn runtime() -> MediaResult<&'static Runtime> {
    static RUNTIME: OnceLock<MediaResult<Runtime>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_name("handy-mpris")
                .enable_all()
                .build()
                .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(Clone::clone)
}

fn bounded<T>(deadline: Duration, future: impl Future<Output = MediaResult<T>>) -> MediaResult<T> {
    runtime()?.block_on(async {
        tokio::time::timeout(deadline, future)
            .await
            .map_err(|_| "MPRIS read/setup deadline exceeded".to_string())?
    })
}

fn error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn playback_state(value: &str) -> MediaResult<PlaybackState> {
    match value {
        "Playing" => Ok(PlaybackState::Playing),
        "Paused" => Ok(PlaybackState::Paused),
        "Stopped" => Ok(PlaybackState::Other),
        _ => Err("Invalid MPRIS PlaybackStatus".into()),
    }
}

pub(super) struct Backend;

impl MediaBackend for Backend {
    fn sessions(&self) -> MediaResult<Vec<Arc<dyn MediaSession>>> {
        Ok(enumerate()?
            .into_iter()
            .map(|session| session as Arc<dyn MediaSession>)
            .collect())
    }
}

fn enumerate() -> MediaResult<Vec<Arc<Session>>> {
    bounded(SETUP_TIMEOUT, async {
        // Retain this connection with every session. Reconnecting could reuse a
        // unique name after a bus restart, so an existing session never reconnects.
        let connection = Connection::session().await.map_err(error)?;
        let dbus = zbus::fdo::DBusProxy::new(&connection)
            .await
            .map_err(error)?;
        let mut names = dbus.list_names().await.map_err(error)?;
        names.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        let mut owners = HashSet::new();
        let mut sessions = Vec::new();
        for name in names {
            let service = name.as_str();
            if !service.starts_with("org.mpris.MediaPlayer2.")
                || service == "org.mpris.MediaPlayer2.playerctld"
            {
                // playerctld can route to a different player without changing
                // its own D-Bus owner; it is not a concrete playback session.
                continue;
            }
            let name = BusName::try_from(service).map_err(error)?;
            let Ok(owner) = dbus.get_name_owner(name).await else {
                continue; // A player may exit during enumeration.
            };
            if !owners.insert(owner.clone()) {
                continue;
            }
            if sessions.len() == MAX_SESSIONS {
                return Err("Too many MPRIS sessions to track safely; none will be paused".into());
            }
            sessions.push(Arc::new(Session {
                connection: connection.clone(),
                owner,
                service: service.to_owned(),
                watch: Mutex::new(Weak::new()),
            }));
        }
        Ok(sessions)
    })
}

struct Session {
    connection: Connection,
    owner: OwnedUniqueName,
    service: String,
    watch: Mutex<Weak<WatchControl>>,
}

impl Session {
    // Losing a well-known name revokes permission for NEW commands, but cannot
    // prove that an already-delivered request on the original connection ended.
    fn owns_service_name(&self) -> MediaResult<bool> {
        bounded(READ_TIMEOUT, async {
            let dbus = zbus::fdo::DBusProxy::new(&self.connection)
                .await
                .map_err(error)?;
            let name = BusName::try_from(self.service.as_str()).map_err(error)?;
            match dbus.get_name_owner(name).await {
                Ok(owner) => Ok(owner == self.owner),
                Err(zbus::fdo::Error::NameHasNoOwner(_)) => Ok(false),
                Err(error) => Err(error.to_string()),
            }
        })
    }

    async fn proxy(&self) -> zbus::Result<Proxy<'static>> {
        zbus::proxy::Builder::new(&self.connection)
            .destination(self.owner.clone())?
            .path(PATH)?
            .interface(PLAYER)?
            .cache_properties(CacheProperties::No)
            .build()
            .await
    }

    fn fence(&self) -> MediaResult<()> {
        let watch = self
            .watch
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .upgrade();
        if let Some(watch) = watch {
            bounded(READ_TIMEOUT, watch.synchronize())?;
        }
        Ok(())
    }

    fn command(
        &self,
        method: &'static str,
        capability: &'static str,
    ) -> MediaResult<MediaOperation> {
        if !self.owns_service_name()? {
            return Ok(completed_operation(Ok(false)));
        }
        let allowed = bounded(READ_TIMEOUT, async {
            let proxy = self.proxy().await.map_err(error)?;
            Ok(proxy
                .get_property::<bool>("CanControl")
                .await
                .map_err(error)?
                && proxy
                    .get_property::<bool>(capability)
                    .await
                    .map_err(error)?)
        })?;
        if !allowed || !self.owns_service_name()? {
            return Ok(completed_operation(Ok(false)));
        }
        self.fence()?;
        let connection = self.connection.clone();
        let owner = self.owner.clone();
        let service = self.service.clone();
        let play_watch = if method == "Play" {
            self.watch.lock().unwrap_or_else(|e| e.into_inner()).upgrade()
        } else {
            None
        };
        let (done, receiver) = mpsc::channel();
        let task = runtime()?.spawn(async move {
            // Recheck in the task as well: it may be scheduled after the caller
            // checked identity. All failures here precede command submission.
            let prepared = tokio::time::timeout(READ_TIMEOUT, async {
                let dbus = zbus::fdo::DBusProxy::new(&connection)
                    .await
                    .map_err(error)?;
                let name = BusName::try_from(service.as_str()).map_err(error)?;
                match dbus.get_name_owner(name).await {
                    Ok(current_owner) if current_owner == owner => {}
                    Ok(_) | Err(zbus::fdo::Error::NameHasNoOwner(_)) => return Ok(None),
                    Err(problem) => return Err(problem.to_string()),
                }
                let proxy: Proxy<'static> = zbus::proxy::Builder::new(&connection)
                    .destination(owner)
                    .map_err(error)?
                    .path(PATH)
                    .map_err(error)?
                    .interface(PLAYER)
                    .map_err(error)?
                    .cache_properties(CacheProperties::No)
                    .build()
                    .await
                    .map_err(error)?;
                if let Some(watch) = &play_watch {
                    watch.synchronize().await?;
                    if !watch.ownership.can_resume() {
                        return Ok(None);
                    }
                }
                Ok(Some(proxy))
            })
            .await;
            let proxy = match prepared {
                Ok(Ok(Some(proxy))) => proxy,
                Ok(Ok(None)) => {
                    let _ = done.send(Ok(false));
                    return;
                }
                Ok(Err(problem)) => {
                    let _ = done.send(Err(problem));
                    return;
                }
                Err(_) => {
                    let _ = done.send(Err(
                        "MPRIS command identity check timed out before submission".into(),
                    ));
                    return;
                }
            };
            // Capability/identity requests and observer synchronization can yield
            // to a user's Play -> Pause. Revalidate at the actual dispatch point.
            if play_watch.as_ref().is_some_and(|watch| !watch.ownership.can_resume()) {
                let _ = done.send(Ok(false));
                return;
            }
            let result = proxy.call_method(method, &()).await;
            // A reply acknowledges the method, not the playback transition.
            // Never turn a timeout/disconnection into a known failed command:
            // the already-delivered command could still take effect later.
            if result.is_ok() {
                let _ = done.send(Ok(true));
            } else {
                debug!("MPRIS {method} result is uncertain; retaining its recording lease");
            }
        });
        let mut pending = PendingCommand { receiver, task };
        Ok(Box::new(move || pending.poll()))
    }
}

struct PendingCommand {
    receiver: mpsc::Receiver<MediaResult<bool>>,
    task: JoinHandle<()>,
}

impl PendingCommand {
    fn poll(&mut self) -> Option<MediaResult<bool>> {
        self.receiver.try_recv().ok()
    }
}

impl Drop for PendingCommand {
    fn drop(&mut self) {
        // This only cancels local observation. It cannot revoke a D-Bus message
        // already delivered; the controller retains unresolved operation leases.
        self.task.abort();
    }
}

impl MediaSession for Session {
    fn state(&self) -> MediaResult<PlaybackState> {
        let state = bounded(READ_TIMEOUT, async {
            let value = self
                .proxy()
                .await
                .map_err(error)?
                .get_property::<String>("PlaybackStatus")
                .await
                .map_err(error)?;
            playback_state(&value)
        })?;
        // A fresh Get alone can miss a queued Playing -> Paused user override.
        // Consume signals already received on this same connection before its
        // Get reply. This cannot observe a player which suppresses/coalesces
        // events, or a user action occurring after the snapshot/fence.
        self.fence()?;
        Ok(state)
    }

    fn pause(&self) -> MediaResult<MediaOperation> {
        self.command("Pause", "CanPause")
    }

    fn play(&self) -> MediaResult<MediaOperation> {
        self.command("Play", "CanPlay")
    }

    fn is_live(&self) -> MediaResult<bool> {
        bounded(READ_TIMEOUT, async {
            let dbus = zbus::fdo::DBusProxy::new(&self.connection)
                .await
                .map_err(error)?;
            // Only disappearance of this unique connection rules out delayed
            // effects. Releasing/reassigning its well-known name is insufficient.
            dbus.name_has_owner(self.owner.clone().into())
                .await
                .map_err(error)
        })
    }

    fn watch(&self, ownership: Arc<PauseOwnership>) -> MediaResult<Box<dyn SessionWatch>> {
        let (properties, owners) = bounded(SETUP_TIMEOUT, async {
            let property_rule = MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .sender(self.owner.clone())
                .map_err(error)?
                .path(PATH)
                .map_err(error)?
                .interface(PROPERTIES)
                .map_err(error)?
                .member("PropertiesChanged")
                .map_err(error)?
                .add_arg(PLAYER)
                .map_err(error)?
                .build();
            let owner_rule = MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .sender("org.freedesktop.DBus")
                .map_err(error)?
                .interface("org.freedesktop.DBus")
                .map_err(error)?
                .member("NameOwnerChanged")
                .map_err(error)?
                .add_arg(self.service.as_str())
                .map_err(error)?
                .build();
            let properties =
                MessageStream::for_match_rule(property_rule, &self.connection, Some(128))
                    .await
                    .map_err(error)?;
            let owners = MessageStream::for_match_rule(owner_rule, &self.connection, Some(8))
                .await
                .map_err(error)?;
            Ok((properties, owners))
        })?;
        let (fences, receiver) = async_mpsc::channel(32);
        let control = Arc::new(WatchControl {
            fences,
            ownership: ownership.clone(),
        });
        *self.watch.lock().unwrap_or_else(|e| e.into_inner()) = Arc::downgrade(&control);
        let task = runtime()?.spawn(observe(properties, owners, receiver, ownership));
        Ok(Box::new(Watch {
            _control: control,
            task,
        }))
    }
}

struct WatchControl {
    fences: async_mpsc::Sender<oneshot::Sender<()>>,
    ownership: Arc<PauseOwnership>,
}

impl WatchControl {
    async fn synchronize(&self) -> MediaResult<()> {
        let (done, result) = oneshot::channel();
        if self.fences.try_send(done).is_err() {
            let problem = "MPRIS observer stopped or its bounded fence queue is full".to_string();
            self.ownership.observe(Err(problem.clone()));
            return Err(problem);
        }
        result.await.map_err(|_| "MPRIS observer stopped".to_string())
    }
}

struct Watch {
    _control: Arc<WatchControl>,
    task: JoinHandle<()>,
}

impl SessionWatch for Watch {}

impl Drop for Watch {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn observe(
    mut properties: MessageStream,
    mut owners: MessageStream,
    mut fences: async_mpsc::Receiver<oneshot::Sender<()>>,
    ownership: Arc<PauseOwnership>,
) {
    loop {
        tokio::select! {
            // Drain events already received before acknowledging a Get fence.
            biased;
            owner = owners.next() => {
                let _ = owner;
                ownership.observe(Err("MPRIS owner changed or bus disconnected".into()));
                break;
            }
            message = properties.next() => {
                let Some(Ok(message)) = message else {
                    ownership.observe(Err("MPRIS property observer disconnected".into()));
                    break;
                };
                let change = message.body().deserialize::<(
                    String,
                    HashMap<String, OwnedValue>,
                    Vec<String>,
                )>();
                match change {
                    Ok((interface, changed, invalidated)) if interface == PLAYER => {
                        if invalidated.iter().any(|name| name == "PlaybackStatus") {
                            // The intermediate value is unknowable; do not infer
                            // ownership from a later snapshot of the property.
                            ownership.observe(Err("MPRIS PlaybackStatus invalidated".into()));
                        } else if let Some(value) = changed.get("PlaybackStatus") {
                            ownership.observe(
                                <&str>::try_from(value).map_err(error).and_then(playback_state)
                            );
                        }
                    }
                    Ok(_) => {}
                    Err(error) => ownership.observe(Err(error.to_string())),
                }
            }
            fence = fences.recv() => {
                let Some(done) = fence else { break };
                let _ = done.send(());
            }
        }
    }
}

#[cfg(test)]
#[path = "media_pause_linux_test.rs"]
mod tests;
