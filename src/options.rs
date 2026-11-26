#[derive(Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct ShellOptions {
    pub interactive: bool,
    pub pipefail: bool,
}

impl ShellOptions {
    pub fn set_named(&mut self, name: &str, enabled: bool) -> Result<(), String> {
        match name {
            "pipefail" => self.pipefail = enabled,
            _ => return Err(format!("unsupported option: {name}")),
        }
        Ok(())
    }

    pub fn named(self) -> impl Iterator<Item = (&'static str, bool)> {
        [("pipefail", self.pipefail)].into_iter()
    }

    pub fn letters(self) -> String {
        let mut letters = String::new();
        if self.interactive {
            letters.push('i');
        }
        letters
    }
}
