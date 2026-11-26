#[derive(Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct ShellOptions {
    pub interactive: bool,
    pub pipefail: bool,
    pub noglob: bool,
    pub noclobber: bool,
}

impl ShellOptions {
    pub fn set_named(&mut self, name: &str, enabled: bool) -> Result<(), String> {
        match name {
            "pipefail" => self.pipefail = enabled,
            "noglob" => self.noglob = enabled,
            "noclobber" => self.noclobber = enabled,
            _ => return Err(format!("unsupported option: {name}")),
        }
        Ok(())
    }

    pub fn named(self) -> impl Iterator<Item = (&'static str, bool)> {
        [
            ("noclobber", self.noclobber),
            ("noglob", self.noglob),
            ("pipefail", self.pipefail),
        ]
        .into_iter()
    }

    pub fn set_short(&mut self, letter: char, enabled: bool) -> Result<(), String> {
        match letter {
            'f' => self.noglob = enabled,
            'C' => self.noclobber = enabled,
            _ => {
                return Err(format!(
                    "unsupported option: {}{letter}",
                    if enabled { '-' } else { '+' }
                ));
            }
        }
        Ok(())
    }

    pub fn letters(self) -> String {
        let mut letters = String::new();
        if self.noclobber {
            letters.push('C');
        }
        if self.noglob {
            letters.push('f');
        }
        if self.interactive {
            letters.push('i');
        }
        letters
    }
}
