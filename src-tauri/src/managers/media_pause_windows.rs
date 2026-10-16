use super::*;
use windows::Foundation::TypedEventHandler;
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionManager as SessionManager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
    PlaybackInfoChangedEventArgs,
};
use windows::Win32::System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED};

pub(super) struct Apartment;
impl Apartment {
    pub(super) fn new() -> MediaResult<Self> {
        unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.map_err(|e| e.to_string())?;
        Ok(Self)
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { RoUninitialize() };
    }
}

pub(super) struct Backend;
impl MediaBackend for Backend {
    fn sessions(&self) -> MediaResult<Vec<Arc<dyn MediaSession>>> {
        let manager = SessionManager::RequestAsync()
            .and_then(|op| op.get())
            .map_err(|e| e.to_string())?;
        let sessions = manager.GetSessions().map_err(|e| e.to_string())?;
        Ok(sessions
            .into_iter()
            .map(|session| {
                Arc::new(WindowsSession {
                    manager: manager.clone(),
                    session,
                }) as Arc<dyn MediaSession>
            })
            .collect())
    }
}

fn playback_state(session: &Session) -> MediaResult<PlaybackState> {
    session
        .GetPlaybackInfo()
        .and_then(|info| info.PlaybackStatus())
        .map(|status| match status {
            Status::Playing => PlaybackState::Playing,
            Status::Paused => PlaybackState::Paused,
            _ => PlaybackState::Other,
        })
        .map_err(|e| e.to_string())
}

struct WindowsSession {
    manager: SessionManager,
    session: Session,
}

impl MediaSession for WindowsSession {
    fn state(&self) -> MediaResult<PlaybackState> {
        playback_state(&self.session)
    }
    fn pause(&self) -> MediaResult<bool> {
        self.session
            .TryPauseAsync()
            .and_then(|op| op.get())
            .map_err(|e| e.to_string())
    }
    fn play(&self) -> MediaResult<bool> {
        self.session
            .TryPlayAsync()
            .and_then(|op| op.get())
            .map_err(|e| e.to_string())
    }
    fn is_live(&self) -> MediaResult<bool> {
        // Compare the original COM object, never its app ID: one application
        // can expose multiple sessions and can exit/restart during recording.
        self.manager
            .GetSessions()
            .map(|sessions| sessions.into_iter().any(|session| session == self.session))
            .map_err(|e| e.to_string())
    }
    fn watch(&self, ownership: Arc<PauseOwnership>) -> MediaResult<Box<dyn SessionWatch>> {
        let token = self
            .session
            .PlaybackInfoChanged(
                &TypedEventHandler::<Session, PlaybackInfoChangedEventArgs>::new(
                    move |session, _| {
                        if let Some(session) = session.as_ref() {
                            ownership.observe(playback_state(session));
                        } else {
                            ownership.observe(Err("Session disconnected".into()));
                        }
                        Ok(())
                    },
                ),
            )
            .map_err(|e| e.to_string())?;
        Ok(Box::new(Watch {
            session: self.session.clone(),
            token,
        }))
    }
}

struct Watch {
    session: Session,
    token: i64,
}
impl SessionWatch for Watch {}
impl Drop for Watch {
    fn drop(&mut self) {
        let _ = self.session.RemovePlaybackInfoChanged(self.token);
    }
}

#[cfg(test)]
#[path = "media_pause_windows_test.rs"]
mod native_test;
