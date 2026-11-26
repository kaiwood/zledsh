use std::ffi::CString;

pub(crate) struct TextPattern(CString);

pub(crate) fn unescape(
    mut chars: impl Iterator<Item = (char, bool)>,
) -> impl Iterator<Item = (char, bool)> {
    std::iter::from_fn(move || {
        let (ch, quoted) = chars.next()?;
        Some(if ch == '\\' && !quoted {
            chars.next().map_or(('\\', true), |(ch, _)| (ch, true))
        } else {
            (ch, quoted)
        })
    })
}

impl TextPattern {
    pub(crate) fn compile(chars: impl Iterator<Item = (char, bool)>) -> Result<Self, String> {
        let (pattern, _) = Self::encode(&unescape(chars).collect::<Vec<_>>())?;
        Ok(pattern)
    }

    pub(crate) fn encode(chars: &[(char, bool)]) -> Result<(Self, bool), String> {
        let closing_bracket = chars.iter().rposition(|&(ch, quoted)| ch == ']' && !quoted);
        let mut active = false;
        let mut encoded = String::new();
        for (i, &(ch, quoted)) in chars.iter().enumerate() {
            let unmatched_bracket = ch == '[' && closing_bracket.is_none_or(|end| i >= end);
            active |= !quoted && !unmatched_bracket && matches!(ch, '*' | '?' | '[');
            if unmatched_bracket
                || (quoted
                    && matches!(
                        ch,
                        '*' | '?' | '[' | ']' | '\\' | '-' | '!' | '^' | ':' | '=' | '.'
                    ))
            {
                encoded.push('\\');
            }
            encoded.push(ch);
        }
        Ok((
            Self(CString::new(encoded).map_err(|_| "pattern contains a NUL byte")?),
            active,
        ))
    }

    pub(crate) fn matches(&self, text: &[u8]) -> bool {
        let Ok(text) = CString::new(text) else {
            return false;
        };
        // SAFETY: both arguments are live NUL-terminated strings.
        unsafe { libc::fnmatch(self.0.as_ptr(), text.as_ptr(), 0) == 0 }
    }
}
