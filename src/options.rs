#[derive(Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct ShellOptions {
    pub interactive: bool,
    pub allexport: bool,
    pub errexit: bool,
    pub pipefail: bool,
    pub noglob: bool,
    pub noexec: bool,
    pub nounset: bool,
    pub globstar: bool,
    pub noclobber: bool,
    pub promptsubst: bool,
    pub verbose: bool,
    pub xtrace: bool,
}

impl ShellOptions {
    pub fn set_named(&mut self, name: &str, enabled: bool) -> Result<(), String> {
        match name {
            "allexport" => self.allexport = enabled,
            "errexit" => self.errexit = enabled,
            "pipefail" => self.pipefail = enabled,
            "globstar" => self.globstar = enabled,
            "noexec" => self.noexec = enabled,
            "nounset" => self.nounset = enabled,
            "noglob" => self.noglob = enabled,
            "noclobber" => self.noclobber = enabled,
            "xtrace" => self.xtrace = enabled,
            "verbose" => self.verbose = enabled,
            "promptsubst" => self.promptsubst = enabled,
            _ => return Err(format!("unsupported option: {name}")),
        }
        Ok(())
    }

    pub fn named(self) -> impl Iterator<Item = (&'static str, bool)> {
        [
            ("allexport", self.allexport),
            ("errexit", self.errexit),
            ("globstar", self.globstar),
            ("noclobber", self.noclobber),
            ("noexec", self.noexec),
            ("noglob", self.noglob),
            ("nounset", self.nounset),
            ("pipefail", self.pipefail),
            ("promptsubst", self.promptsubst),
            ("verbose", self.verbose),
            ("xtrace", self.xtrace),
        ]
        .into_iter()
    }

    pub fn set_short(&mut self, letter: char, enabled: bool) -> Result<(), String> {
        match letter {
            'x' => self.xtrace = enabled,
            'v' => self.verbose = enabled,
            'a' => self.allexport = enabled,
            'e' => self.errexit = enabled,
            'n' => self.noexec = enabled,
            'u' => self.nounset = enabled,
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
        if self.allexport {
            letters.push('a');
        }
        if self.errexit {
            letters.push('e');
        }
        if self.noglob {
            letters.push('f');
        }
        if self.interactive {
            letters.push('i');
        }
        if self.noexec {
            letters.push('n');
        }
        if self.nounset {
            letters.push('u');
        }
        if self.verbose {
            letters.push('v');
        }
        if self.xtrace {
            letters.push('x');
        }
        letters
    }
}
