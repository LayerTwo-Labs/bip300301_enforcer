use std::process::Command;

fn get_git_hash() -> Result<String, Box<dyn std::error::Error>> {
    let args = ["describe", "--always", "--dirty"];
    let output = Command::new("git").args(args).output()?;

    if !output.status.success() {
        return Err(format!("Failed to execute `git {}`", args.join(" ")).into());
    }

    let hash = String::from_utf8(output.stdout)?.trim().to_string();

    Ok(hash)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Set git hash as environment variable for runtime access
    match get_git_hash() {
        Ok(hash) => {
            println!("cargo:rustc-env=GIT_HASH={}", hash);
            println!("cargo:rerun-if-changed=.git/HEAD");
            println!("cargo:rerun-if-changed=.git/refs/heads");
        }
        Err(e) => {
            println!("cargo:warning=Failed to get git hash: {}", e);
            println!("cargo:rustc-env=GIT_HASH=unknown");
        }
    }

    Ok(())
}
