/// List all open file descriptors for the current process. Only works on
/// on macOS!
pub(crate) fn list_open_descriptors_macos() -> std::io::Result<Vec<String>> {
    // Get the current process ID
    let pid = std::process::id();

    // Run lsof for the current process
    let output = std::process::Command::new("lsof")
        .arg("-p")
        .arg(pid.to_string())
        .arg("-a")
        .arg("-Fn") // Output only the file names/paths
        .output()?;

    if !output.status.success() {
        return Err(std::io::Error::other("lsof command failed"));
    }

    // Convert output to string
    let stdout = String::from_utf8_lossy(&output.stdout);

    let mut open_files = Vec::new();
    // Process each line of output
    for line in stdout.lines() {
        // lsof -Fn prefixes file paths with 'n'
        if let Some(stripped) = line.strip_prefix('n') {
            open_files.push(stripped.to_string());
        }
    }

    Ok(open_files)
}
