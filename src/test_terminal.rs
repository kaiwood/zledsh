use std::process::Command;

pub(crate) fn test_command(test: &str, nocapture: bool) -> Command {
    let executable = std::env::current_exe().unwrap();
    let mut script = Command::new("/usr/bin/script");
    #[cfg(target_os = "macos")]
    {
        script
            .args(["-q", "/dev/null"])
            .arg(executable)
            .args(["--exact", test]);
        if nocapture {
            script.arg("--nocapture");
        }
    }
    #[cfg(target_os = "linux")]
    {
        // util-linux script accepts a single command string, not argv.
        let quote = |value: &str| format!("'{}'", value.replace('\'', "'\\''"));
        let executable = quote(executable.to_str().unwrap());
        let flags = if nocapture { " --nocapture" } else { "" };
        script
            .args(["-q", "-e", "-c"])
            .arg(format!("exec {executable} --exact {}{flags}", quote(test)))
            .arg("/dev/null");
    }
    script
}
