use super::TargetError;
use std::ops::Range;

pub(super) const MAX_UTF16_UNITS: usize = 1_048_576;

/// Windows selections are offsets in UTF-16 code units, not Rust byte offsets.
/// Keep the exact source value only for the lifetime of a recording; never log it.
pub(super) struct TextSnapshot {
    pub value: Vec<u16>,
    pub selection: Range<usize>,
}

impl TextSnapshot {
    pub fn new(value: Vec<u16>, selection: Range<usize>) -> Result<Self, TargetError> {
        if value.len() > MAX_UTF16_UNITS {
            return Err(TargetError::Unsupported);
        }
        if selection.start > selection.end
            || selection.end > value.len()
            || !is_boundary(&value, selection.start)
            || !is_boundary(&value, selection.end)
            || String::from_utf16(&value).is_err()
        {
            return Err(TargetError::InvalidText);
        }
        Ok(Self { value, selection })
    }

    pub fn replacement(&self, current: &[u16], text: &str) -> Result<Vec<u16>, TargetError> {
        if current != self.value {
            return Err(TargetError::Changed);
        }
        if text.contains('\0') {
            return Err(TargetError::InvalidText);
        }
        let inserted: Vec<u16> = text.encode_utf16().collect();
        let size = self.value.len() - self.selection.len() + inserted.len();
        if size > MAX_UTF16_UNITS {
            return Err(TargetError::InvalidText);
        }
        let mut value = Vec::with_capacity(size);
        value.extend_from_slice(&self.value[..self.selection.start]);
        value.extend_from_slice(&inserted);
        value.extend_from_slice(&self.value[self.selection.end..]);
        Ok(value)
    }
}

fn is_boundary(value: &[u16], offset: usize) -> bool {
    offset == 0
        || offset == value.len()
        || !((0xD800..=0xDBFF).contains(&value[offset - 1])
            && (0xDC00..=0xDFFF).contains(&value[offset]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16(value: &str) -> Vec<u16> {
        value.encode_utf16().collect()
    }

    #[test]
    fn frozen_selection_does_not_follow_a_later_caret() {
        let value = utf16("before after");
        let snapshot = TextSnapshot::new(value.clone(), 7..7).unwrap();
        assert_eq!(
            String::from_utf16(&snapshot.replacement(&value, "dictation ").unwrap()).unwrap(),
            "before dictation after"
        );
    }

    #[test]
    fn replaces_only_the_captured_selection() {
        let value = utf16("a selected word z");
        let snapshot = TextSnapshot::new(value.clone(), 2..15).unwrap();
        assert_eq!(
            String::from_utf16(&snapshot.replacement(&value, "replacement").unwrap()).unwrap(),
            "a replacement z"
        );
    }

    #[test]
    fn changed_document_is_not_overwritten() {
        let snapshot = TextSnapshot::new(utf16("original"), 8..8).unwrap();
        assert_eq!(
            snapshot.replacement(&utf16("original edited"), "dictation"),
            Err(TargetError::Changed)
        );
    }

    #[test]
    fn handles_emoji_and_non_ascii_utf16_offsets() {
        let value = utf16("А🦀Б");
        let snapshot = TextSnapshot::new(value.clone(), 1..3).unwrap();
        assert_eq!(
            String::from_utf16(&snapshot.replacement(&value, "ёж").unwrap()).unwrap(),
            "АёжБ"
        );
    }

    #[test]
    fn rejects_selection_inside_surrogate_pair() {
        assert!(matches!(
            TextSnapshot::new(utf16("🦀"), 1..1),
            Err(TargetError::InvalidText)
        ));
    }

    #[test]
    fn rejects_invalid_or_out_of_bounds_selection() {
        for selection in [Range { start: 3, end: 2 }, 0..8] {
            assert!(matches!(
                TextSnapshot::new(utf16("abc"), selection),
                Err(TargetError::InvalidText)
            ));
        }
    }

    #[test]
    fn refuses_nul_instead_of_silently_inserting_prefix() {
        let snapshot = TextSnapshot::new(Vec::new(), 0..0).unwrap();
        assert_eq!(
            snapshot.replacement(&[], "prefix\0lost suffix"),
            Err(TargetError::InvalidText)
        );
    }

    #[test]
    fn empty_original_field_is_a_valid_target() {
        let snapshot = TextSnapshot::new(Vec::new(), 0..0).unwrap();
        assert_eq!(snapshot.replacement(&[], "one").unwrap(), utf16("one"));
    }
}
