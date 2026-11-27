use std::ffi::CString;

pub(crate) struct TextPattern(CString);

pub(crate) fn unescape(
    mut chars: impl Iterator<Item = (u8, bool)>,
) -> impl Iterator<Item = (u8, bool)> {
    std::iter::from_fn(move || {
        let (ch, quoted) = chars.next()?;
        Some(if ch == b'\\' && !quoted {
            chars.next().map_or((b'\\', true), |(ch, _)| (ch, true))
        } else {
            (ch, quoted)
        })
    })
}

impl TextPattern {
    pub(crate) fn compile(chars: impl Iterator<Item = (u8, bool)>) -> Result<Self, String> {
        let (pattern, _) = Self::encode(&unescape(chars).collect::<Vec<_>>())?;
        Ok(pattern)
    }

    pub(crate) fn encode(chars: &[(u8, bool)]) -> Result<(Self, bool), String> {
        let closing_bracket = chars
            .iter()
            .rposition(|&(ch, quoted)| ch == b']' && !quoted);
        let mut active = false;
        let mut encoded = Vec::new();
        for (i, &(ch, quoted)) in chars.iter().enumerate() {
            let unmatched_bracket = ch == b'[' && closing_bracket.is_none_or(|end| i >= end);
            active |= !quoted && !unmatched_bracket && matches!(ch, b'*' | b'?' | b'[');
            if unmatched_bracket
                || (quoted
                    && matches!(
                        ch,
                        b'*' | b'?' | b'[' | b']' | b'\\' | b'-' | b'!' | b'^' | b':' | b'=' | b'.'
                    ))
            {
                encoded.push(b'\\');
            }
            encoded.push(ch);
        }
        Ok((
            Self(CString::new(encoded).map_err(|_| "pattern contains a NUL byte")?),
            active,
        ))
    }

    pub(crate) fn trim<'a>(&self, value: &'a str, prefix: bool, longest: bool) -> &'a str {
        let mut result = value;
        // At character boundaries only, so a returned slice remains UTF-8.
        for boundary in value
            .char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(value.len()))
        {
            let (left, right) = value.split_at(boundary);
            let (candidate, retained) = if prefix { (left, right) } else { (right, left) };
            if self.matches(candidate.as_bytes()) {
                result = retained;
                if prefix != longest {
                    break;
                }
            }
        }
        result
    }

    pub(crate) fn matches(&self, text: &[u8]) -> bool {
        let Ok(text) = CString::new(text) else {
            return false;
        };
        // SAFETY: both arguments are live NUL-terminated strings.
        unsafe { libc::fnmatch(self.0.as_ptr(), text.as_ptr(), 0) == 0 }
    }
}
