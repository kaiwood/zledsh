#[derive(Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct ShellOptions {
    pub interactive: bool,
}

impl ShellOptions {
    pub fn letters(self) -> String {
        let mut letters = String::new();
        if self.interactive {
            letters.push('i');
        }
        letters
    }
}
