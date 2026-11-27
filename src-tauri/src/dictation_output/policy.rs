//! The destination and output mode are frozen before recognition begins.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputMode {
    #[default]
    Insert,
    ClipboardOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureMoment {
    Start,
    End,
}

pub struct RecordingTarget<T> {
    moment: CaptureMoment,
    target: Option<T>,
}

impl<T> RecordingTarget<T> {
    pub fn begin(moment: CaptureMoment, capture: impl FnOnce() -> T) -> Self {
        Self {
            moment,
            target: (moment == CaptureMoment::Start).then(capture),
        }
    }

    pub fn finish(self, mode: OutputMode, capture: impl FnOnce() -> T) -> Option<T> {
        match (mode, self.moment) {
            (OutputMode::ClipboardOnly, _) => None,
            (OutputMode::Insert, CaptureMoment::Start) => self.target,
            (OutputMode::Insert, CaptureMoment::End) => Some(capture()),
        }
    }
}

/// Keys pressed in an accepted prefix of a synthetic batch that still need
/// releasing. A failed batch must never replay its command key.
pub fn keys_to_release(events: &[(u16, bool)], accepted: usize) -> Vec<u16> {
    let mut held = Vec::new();
    for &(key, released) in events.iter().take(accepted) {
        if released {
            held.retain(|held| *held != key);
        } else if !held.contains(&key) {
            held.push(key);
        }
    }
    held.reverse();
    held
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn partial_shortcut_batch_only_releases_keys_actually_left_down() {
        let events = [(17, false), (13, false), (13, true), (17, true)];
        assert_eq!(keys_to_release(&events, 0), Vec::<u16>::new());
        assert_eq!(keys_to_release(&events, 1), vec![17]);
        assert_eq!(keys_to_release(&events, 2), vec![13, 17]);
        assert_eq!(keys_to_release(&events, 3), vec![17]);
        assert_eq!(keys_to_release(&events, 4), Vec::<u16>::new());
        assert_eq!(keys_to_release(&events, usize::MAX), Vec::<u16>::new());
    }

    #[test]
    fn start_destination_preserves_field_and_selection_together() {
        let focus = Cell::new((1, 4, 7));
        let session = RecordingTarget::begin(CaptureMoment::Start, || focus.get());
        focus.set((2, 8, 8));
        assert_eq!(
            session.finish(OutputMode::Insert, || focus.get()),
            Some((1, 4, 7))
        );
    }

    #[test]
    fn end_destination_is_captured_once_before_recognition() {
        let focus = Cell::new((1, 4, 7));
        let captures = Cell::new(0);
        let capture = || {
            captures.set(captures.get() + 1);
            focus.get()
        };
        let session = RecordingTarget::begin(CaptureMoment::End, capture);
        assert_eq!(captures.get(), 0);
        focus.set((2, 8, 8));
        let prepared = session.finish(OutputMode::Insert, capture);
        focus.set((3, 9, 10));
        assert_eq!(prepared, Some((2, 8, 8)));
        assert_eq!(captures.get(), 1);
    }

    #[test]
    fn clipboard_stop_never_captures_end_or_keeps_an_insertion_target() {
        for moment in [CaptureMoment::Start, CaptureMoment::End] {
            let session = RecordingTarget::begin(moment, || 42);
            let target = session.finish(OutputMode::ClipboardOnly, || {
                panic!("clipboard stop must not capture a target")
            });
            assert!(target.is_none());
        }
    }

    #[test]
    fn failed_start_capture_cannot_be_replaced_with_later_focus() {
        let session = RecordingTarget::begin(CaptureMoment::Start, || Err::<u32, _>("unavailable"));
        assert_eq!(
            session.finish(OutputMode::Insert, || Ok(7)),
            Some(Err("unavailable"))
        );
    }
}
