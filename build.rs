// Embeds the git commit and build date, shown in the status page footer.
//
// Docker builds don't have the .git directory, so the commit can be passed in
// with the GIT_COMMIT environment variable instead.
fn main() {
    println!("cargo:rerun-if-env-changed=GIT_COMMIT");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads");
    let commit = std::env::var("GIT_COMMIT")
        .ok()
        .filter(|c| !c.is_empty())
        .or_else(|| {
            let output = std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .output()
                .ok()?;
            output
                .status
                .success()
                .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        })
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=STATUS_GIT_COMMIT={commit}");
    let date = chrono::Utc::now().format("%Y-%m-%d");
    println!("cargo:rustc-env=STATUS_BUILD_DATE={date}");
}
